//! RDCleanPath proxy for tabby-rdp's IronRDP web client (ported from its rdcleanpath.ts, which needed Node's `ws` and
//! `tls`). The WASM client talks WebSocket to this proxy on 127.0.0.1; the proxy asks the page where to connect (a
//! socket, a program like `ssh -W`, a channel of one of the page's SSH connections, or any of these through an RD
//! Gateway, see gateway.rs), opens that, forwards the X.224 request (or a Hyper-V console's pre-connection blob),
//! stops before TLS when the server doesn't use Network Level Authentication unless the page allows it, terminates TLS,
//! has the page check the server's certificate, hands the certificate chain back to the client (for CredSSP), then
//! relays. The RDP bytes never pass through the page on their way upstream.
//!
//! Shared folders live beside it (drives.rs). Streaming downloads use the page-owned file bridge in files.rs.
//! These commands are bundled with the built-in Remote Desktop plugin.
mod drives;
mod gateway;
mod ntlm;

pub use drives::*;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::State;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

const VERSION_1: u32 = 3390;
const GENERAL_ERROR: u32 = 1;

/// The client's first message (the RDCleanPath request: a destination, a token, an X.224 request) is small. Anything on
/// this computer can connect to the port: before the token is checked, no more than this is kept.
const MAX_HELLO: usize = 64 * 1024;
/// How long a client has to upgrade to WebSocket and send that request.
#[cfg(not(test))]
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const HELLO_TIMEOUT: Duration = Duration::from_millis(500);
/// How long reaching the server may take: the upstream, X.224 and TLS (a gateway's sign-in and tunnel included).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(45);
/// What the client may send ahead while the server is being reached (CredSSP hasn't started: next to nothing).
const MAX_EARLY: usize = 256 * 1024;
/// One client at a time uses a proxy; a reconnect can overlap the connection it replaces.
const MAX_CLIENTS: usize = 4;
/// A WebSocket message from the client: RDP PDUs, at most a few megabytes for a shared drive's read.
const MAX_MESSAGE: usize = 32 * 1024 * 1024;
/// What may wait, unread, from a desktop reached through SSH (see Drained): the SSH connection can't be told to slow
/// down one channel without stalling its others (the terminal), so past this the desktop is closed instead.
const MAX_UNREAD_SSH: usize = 64 * 1024 * 1024;

// ---- API -------------------------------------------------------------------------------------------------------

/// Where a connection goes: the page's answer to an Upstream event, for each client connection.
#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Upstream {
    /// A plain socket from this computer (LAN, VPN).
    Tcp { host: String, port: u16 },
    /// A program whose stdin and stdout are the stream: `ssh -W host:port …`.
    Process { program: String, args: Vec<String> },
    /// host:port as an SSH server sees it (direct-tcpip), over the page's SSH client with that handle (src/ssh).
    SshTcp { client: u32, host: String, port: u16 },
    /// A command on an SSH server (e.g. its own `ssh -W`), its stdin and stdout the stream.
    SshExec { client: u32, command: String },
    /// `target` as an RD Gateway sees it, through that gateway (reached over `via`): see gateway.rs.
    #[serde(rename_all = "camelCase")]
    Gateway {
        via: Box<Upstream>,
        gateway: gateway::Address,
        account: gateway::Account,
        target: gateway::Address,
        #[serde(default)]
        client_name: String,
    },
}

#[derive(Clone, Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum ProxyEvent {
    Log { message: String },
    Drained { bytes: usize },
    /// A client connected: the page answers (rdp_proxy_answer) with the Upstream to open for it.
    Upstream { request: u32 },
    /// The page decides on the server's certificate and answers (rdp_proxy_answer) with null, or an error to refuse it.
    #[serde(rename_all = "camelCase")]
    Certificate { request: u32, fingerprint: String, valid: bool, reason: String, without_nla: bool },
    /// The server chose TLS without Network Level Authentication: the client would send it the password as it is. The
    /// page answers null to go on, or an error to stop here (nothing but the connection request has gone to it).
    WithoutNla { request: u32 },
    /// An RD Gateway's certificate, before the sign-in goes to it: `valid` for the gateway's name by this computer's
    /// certificate authorities. `upstream`: the Upstream request that named the gateway. Answered like Certificate.
    GatewayCertificate { request: u32, upstream: u32, fingerprint: String, valid: bool },
    /// The gateway took the sign-in (the page may now keep a password just entered).
    GatewaySignedIn { upstream: u32 },
    /// The gateway refused the account: its sign-in, or its connection authorization policy.
    GatewayRefused { upstream: u32, message: String },
    /// Why the last connection failed before the relay was up, when the proxy can tell better than the client; null
    /// when one starts.
    Failure { message: Option<String> },
}

type Answer = Result<serde_json::Value, String>;

struct Proxy {
    token: String,
    autologon: bool,
    server_name: Mutex<Option<String>>,
    events: Channel<ProxyEvent>,
    /// For the page's SSH clients (none in tests).
    app: Option<tauri::AppHandle>,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    next_request: AtomicU32,
    answers: Mutex<HashMap<u32, oneshot::Sender<Answer>>>,
    closed: AtomicBool,
    /// Client connections open now (at most MAX_CLIENTS).
    clients: AtomicUsize,
}

impl Proxy {
    fn log(&self, message: impl Into<String>) {
        let _ = self.events.send(ProxyEvent::Log { message: message.into() });
    }

    /// Asks the page something (the event made from a request number) and waits for its answer.
    async fn ask(&self, event: impl FnOnce(u32) -> ProxyEvent) -> Answer {
        let request = self.next_request.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        {
            let mut answers = self.answers.lock().unwrap();
            if self.closed.load(Ordering::Relaxed) {
                return Err("RDP proxy is closed".into());
            }
            answers.insert(request, tx);
        }
        let _ = self.events.send(event(request));
        rx.await.unwrap_or_else(|_| Err("the page did not answer".into()))
    }

    /// Asks the page and returns the request number too (later events about the answer refer to it).
    async fn ask_numbered(&self, event: impl FnOnce(u32) -> ProxyEvent) -> (u32, Answer) {
        let mut number = 0;
        let answer = self.ask(|request| { number = request; event(request) }).await;
        (number, answer)
    }

    /// Where the page wants this connection to go, and the number of that question.
    async fn upstream(&self) -> Result<(u32, Upstream), String> {
        let (request, value) = self.ask_numbered(|request| ProxyEvent::Upstream { request }).await;
        Ok((request, serde_json::from_value(value?).map_err(|e| format!("bad upstream: {e}"))?))
    }

    fn event(&self, event: ProxyEvent) {
        let _ = self.events.send(event);
    }

    /// The page's verdict on the certificate: Err with its reason to refuse it.
    async fn check_certificate(&self, fingerprint: String, valid: bool, reason: String, without_nla: bool) -> Result<(), String> {
        self.ask(|request| ProxyEvent::Certificate { request, fingerprint, valid, reason, without_nla }).await.map(|_| ())
    }
}

struct Running {
    owner: String,
    generation: u64,
    proxy: Arc<Proxy>,
    listener: tokio::task::AbortHandle,
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut answers = self.proxy.answers.lock().unwrap();
        self.proxy.closed.store(true, Ordering::Relaxed);
        answers.clear();
        self.listener.abort();
    }
}

#[derive(Default)]
struct ProxyTable {
    running: HashMap<u32, Running>,
    generations: HashMap<String, u64>,
}

#[derive(Default)]
pub struct Proxies {
    next: AtomicU32,
    table: Mutex<ProxyTable>,
}

impl Proxies {
    fn generation(&self, owner: &str) -> u64 {
        *self.table.lock().unwrap().generations.entry(owner.into()).or_default()
    }

    fn insert(&self, running: Running) -> Result<u32, String> {
        let mut table = self.table.lock().unwrap();
        if table.generations.get(&running.owner).copied().unwrap_or_default() != running.generation {
            return Err("RDP proxy's page has gone".into());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        table.running.insert(id, running);
        Ok(id)
    }

    pub(crate) fn reset(&self, owner: &str) {
        let mut table = self.table.lock().unwrap();
        *table.generations.entry(owner.into()).or_default() += 1;
        table.running.retain(|_, running| running.owner != owner);
    }

    fn get(&self, id: u32) -> Result<Arc<Proxy>, String> {
        self.table.lock().unwrap().running.get(&id).map(|r| r.proxy.clone()).ok_or_else(|| format!("no RDP proxy {id}"))
    }
}

#[derive(Serialize)]
pub struct Started {
    id: u32,
    url: String,
    token: String,
}

#[tauri::command]
pub async fn rdp_proxy_start(
    app: tauri::AppHandle,
    webview: tauri::Webview,
    proxies: State<'_, Proxies>,
    autologon: bool,
    server_name: Option<String>,
    events: Channel<ProxyEvent>,
) -> Result<Started, String> {
    let owner = webview.label().to_string();
    let generation = proxies.generation(&owner);
    let (proxy, port, listener) = start(Some(app), autologon, events).await?;
    *proxy.server_name.lock().unwrap() = server_name;
    let token = proxy.token.clone();
    let id = proxies.insert(Running { owner, generation, proxy, listener })?;
    Ok(Started { id, url: format!("ws://127.0.0.1:{port}/{token}"), token })
}

async fn start(app: Option<tauri::AppHandle>, autologon: bool, events: Channel<ProxyEvent>) -> Result<(Arc<Proxy>, u16, tokio::task::AbortHandle), String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let proxy = Arc::new(Proxy {
        token: random_token()?,
        autologon,
        server_name: Mutex::new(None),
        events,
        app,
        bytes_in: AtomicU64::new(0),
        bytes_out: AtomicU64::new(0),
        next_request: AtomicU32::new(0),
        answers: Mutex::new(HashMap::new()),
        closed: AtomicBool::new(false),
        clients: AtomicUsize::new(0),
    });
    let serving = proxy.clone();
    // The connections live in the listener task's JoinSet: closing the proxy aborts them all, idle or not.
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            while connections.try_join_next().is_some() {}
            let Ok((socket, _)) = listener.accept().await else { break };
            if serving.clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
                continue; // dropped: closed at once
            }
            let _ = socket.set_nodelay(true);
            connections.spawn(serve(serving.clone(), socket));
        }
    });
    Ok((proxy, port, task.abort_handle()))
}

/// The page's answer to an event's request: `value`, or `error` (a refused certificate, no SSH connection, …).
#[tauri::command]
pub fn rdp_proxy_answer(proxies: State<'_, Proxies>, id: u32, request: u32, value: Option<serde_json::Value>, error: Option<String>) -> Result<(), String> {
    let proxy = proxies.get(id)?;
    let waiting = proxy.answers.lock().unwrap().remove(&request);
    if let Some(tx) = waiting {
        let _ = tx.send(match error {
            Some(error) => Err(error),
            None => Ok(value.unwrap_or(serde_json::Value::Null)),
        });
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    bytes_in: u64,
    bytes_out: u64,
}

/// RDP bytes relayed since the proxy started (after TLS, before any SSH): from the server, and to it.
#[tauri::command]
pub fn rdp_proxy_stats(proxies: State<'_, Proxies>, id: u32) -> Result<Stats, String> {
    let proxy = proxies.get(id)?;
    Ok(Stats { bytes_in: proxy.bytes_in.load(Ordering::Relaxed), bytes_out: proxy.bytes_out.load(Ordering::Relaxed) })
}

#[tauri::command]
pub fn rdp_proxy_close(proxies: State<'_, Proxies>, id: u32) {
    proxies.table.lock().unwrap().running.remove(&id);
}

fn random_token() -> Result<String, String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 24];
    ring::rand::SystemRandom::new().fill(&mut bytes).map_err(|_| "no randomness".to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A SHA-256 fingerprint as `AB:CD:…`, as openssl and Node print it.
fn fingerprint_of(der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, der);
    digest.as_ref().iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

// ---- minimal DER -----------------------------------------------------------------------------------------------

fn der_length(n: usize, out: &mut Vec<u8>) {
    if n < 0x80 {
        out.push(n as u8);
        return;
    }
    let bytes: Vec<u8> = n.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
    out.push(0x80 | bytes.len() as u8);
    out.extend(bytes);
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    der_length(content.len(), &mut out);
    out.extend_from_slice(content);
    out
}

fn der_integer(n: u32) -> Vec<u8> {
    let mut bytes: Vec<u8> = n.to_be_bytes().into_iter().skip_while(|b| *b == 0).collect();
    if bytes.is_empty() || bytes[0] & 0x80 != 0 {
        bytes.insert(0, 0);
    }
    tlv(0x02, &bytes)
}

fn seq(items: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x30, &items.concat())
}

fn explicit(n: u8, inner: Vec<u8>) -> Vec<u8> {
    tlv(0xa0 | n, &inner)
}

struct Tlv {
    tag: u8,
    start: usize,
    end: usize,
}

/// The TLV at `off`, once its header is all there (its content may not be yet).
fn read_tlv(buf: &[u8], off: usize) -> Option<Tlv> {
    if buf.len() < off + 2 {
        return None;
    }
    let mut len = buf[off + 1] as usize;
    let mut p = off + 2;
    if len & 0x80 != 0 {
        let k = len & 0x7f;
        if k > 4 || buf.len() < p + k {
            return None;
        }
        len = buf[p..p + k].iter().fold(0, |acc, b| acc * 256 + *b as usize);
        p += k;
    }
    Some(Tlv { tag: buf[off], start: p, end: p + len })
}

#[derive(Default)]
struct Request {
    version: Option<u32>,
    destination: Option<String>,
    proxy_auth: Option<String>,
    /// The pre-connection blob's text (a Hyper-V VM's id, for one): sent to the server before anything else.
    pcb: Option<String>,
    /// The X.224 connection request. Absent for a Hyper-V VM's console: there TLS comes first, and the client does
    /// X.224 after CredSSP.
    x224: Option<Vec<u8>>,
}

fn decode_request(buf: &[u8]) -> Result<Request, String> {
    let malformed = || "malformed request".to_string();
    let outer = read_tlv(buf, 0).filter(|t| t.tag == 0x30 && t.end <= buf.len()).ok_or_else(malformed)?;
    let mut req = Request::default();
    let mut p = outer.start;
    while p < outer.end {
        let field = read_tlv(buf, p).filter(|t| t.end <= outer.end).ok_or_else(malformed)?;
        let inner = read_tlv(buf, field.start).filter(|t| t.end <= field.end).ok_or_else(malformed)?;
        let value = &buf[inner.start..inner.end];
        match field.tag & 0x1f {
            0 => req.version = Some(value.iter().fold(0u32, |acc, b| acc.wrapping_mul(256) + *b as u32)),
            2 => req.destination = Some(String::from_utf8_lossy(value).into_owned()),
            3 => req.proxy_auth = Some(String::from_utf8_lossy(value).into_owned()),
            5 => req.pcb = Some(String::from_utf8_lossy(value).into_owned()),
            6 => req.x224 = Some(value.to_vec()),
            _ => {}
        }
        p = field.end;
    }
    Ok(req)
}

/// The response: the server's X.224 confirm (none for a Hyper-V VM's console), its certificate chain, its address.
fn encode_response(x224: Option<&[u8]>, chain: &[CertificateDer<'_>], server_addr: &str) -> Vec<u8> {
    let certs: Vec<Vec<u8>> = chain.iter().map(|c| tlv(0x04, c.as_ref())).collect();
    let mut fields = vec![explicit(0, der_integer(VERSION_1))];
    if let Some(x224) = x224 {
        fields.push(explicit(6, tlv(0x04, x224)));
    }
    fields.push(explicit(7, seq(&certs)));
    fields.push(explicit(9, tlv(0x0c, server_addr.as_bytes())));
    seq(&fields)
}

/// A version 2 pre-connection PDU ([MS-RDPEPS] 2.2.1.2) carrying `text`: what a client sends first to a server that
/// routes by it, as Hyper-V does to a VM's console by the VM's id.
fn preconnection_pdu(text: &str) -> Vec<u8> {
    let blob: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let size = 18 + blob.len() * 2;
    let mut pdu = Vec::with_capacity(size);
    pdu.extend_from_slice(&(size as u32).to_le_bytes()); // cbSize
    pdu.extend_from_slice(&0u32.to_le_bytes()); // Flags
    pdu.extend_from_slice(&2u32.to_le_bytes()); // Version
    pdu.extend_from_slice(&0u32.to_le_bytes()); // Id
    pdu.extend_from_slice(&(blob.len() as u16).to_le_bytes()); // cchPCB, with the terminator
    for unit in blob {
        pdu.extend_from_slice(&unit.to_le_bytes());
    }
    pdu
}

/// Whether the server's answer to the connection request chose Network Level Authentication (CredSSP: HYBRID or
/// HYBRID_EX). With it, the sign-in is a proof both ways and the password only goes to a server that knows it already.
/// Without it (plain TLS, as xrdp does), the client sends the password itself in its Client Info PDU: encrypted on the
/// way, readable to whatever answered.
fn offers_nla(x224: &[u8]) -> bool {
    x224.len() >= 19 && x224[11] == 0x02 && u32::from_le_bytes([x224[15], x224[16], x224[17], x224[18]]) & (2 | 8) != 0
}

fn encode_error() -> Vec<u8> {
    seq(&[explicit(0, der_integer(VERSION_1)), explicit(1, seq(&[explicit(0, der_integer(GENERAL_ERROR))]))])
}

// ---- RDP -------------------------------------------------------------------------------------------------------

/// Reads exactly one TPKT-framed PDU (the X.224 Connection Confirm), and nothing of the TLS after it.
async fn read_tpkt(stream: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, String> {
    let closed = |e: std::io::Error| format!("upstream closed during X.224 ({e})");
    let mut pdu = vec![0u8; 4];
    stream.read_exact(&mut pdu).await.map_err(closed)?;
    let len = u16::from_be_bytes([pdu[2], pdu[3]]) as usize;
    if pdu[0] != 0x03 || len < 4 {
        return Err("the server's answer is not RDP".into());
    }
    pdu.resize(len, 0);
    stream.read_exact(&mut pdu[4..]).await.map_err(closed)?;
    Ok(pdu)
}

/// The web client's connect() calls Framed::into_inner_no_leftover() before starting its session reader. If one
/// WebSocket message contains FontMap followed by drdynvc capabilities, those unread capabilities are discarded:
/// RTT and clipboard still work, but the graphics channel never opens. Keep one server PDU per WebSocket message,
/// independent of TLS records and SSH read boundaries. Before RDP there can be DER CredSSP messages and, with
/// HYBRID_EX, a four-byte Early User Authorization Result. After activation both TPKT and fast-path are legal.
struct ServerFraming {
    rdp: bool,
    early_auth: bool,
}

impl ServerFraming {
    async fn read(&mut self, stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Option<Vec<u8>>> {
        use std::io::{Error, ErrorKind};
        let invalid = || Error::new(ErrorKind::InvalidData, "invalid RDP server frame");
        let mut first = [0u8; 1];
        if stream.read(&mut first).await? == 0 {
            return Ok(None);
        }
        let mut pdu = vec![first[0], stream.read_u8().await?];
        let len = if !self.rdp && pdu[0] == 0x30 {
            // TSRequest: DER SEQUENCE, definite short or long length. Bound allocation for malformed peers.
            let content_len = if pdu[1] & 0x80 == 0 {
                pdu[1] as usize
            } else {
                let octets = (pdu[1] & 0x7f) as usize;
                if octets == 0 || octets > 4 {
                    return Err(invalid());
                }
                pdu.resize(2 + octets, 0);
                stream.read_exact(&mut pdu[2..]).await?;
                pdu[2..].iter().fold(0usize, |n, b| (n << 8) | *b as usize)
            };
            if content_len > 16 * 1024 * 1024 {
                return Err(invalid());
            }
            pdu.len() + content_len
        } else if !self.rdp && self.early_auth {
            self.early_auth = false;
            4
        } else if pdu[0] == 0x03 {
            if pdu[1] != 0 {
                return Err(invalid());
            }
            self.rdp = true;
            pdu.resize(4, 0);
            stream.read_exact(&mut pdu[2..]).await?;
            u16::from_be_bytes([pdu[2], pdu[3]]) as usize
        } else if self.rdp && pdu[0] & 0x03 == 0 {
            if pdu[1] & 0x80 == 0 {
                pdu[1] as usize
            } else {
                pdu.push(stream.read_u8().await?);
                (((pdu[1] & 0x7f) as usize) << 8) | pdu[2] as usize
            }
        } else {
            return Err(invalid());
        };
        if len < pdu.len() {
            return Err(invalid());
        }
        let header_len = pdu.len();
        pdu.resize(len, 0);
        stream.read_exact(&mut pdu[header_len..]).await?;
        Ok(Some(pdu))
    }
}

/// The server's answer to the client's security negotiation, when it rules out TLS (which IronRDP and RDCleanPath
/// need): a server with RDP's standard security only (xrdp with security_layer=rdp, for example) answers without a
/// negotiation response, or with PROTOCOL_RDP, or refuses the negotiation. None when TLS can go ahead.
fn no_tls(x224: &[u8]) -> Option<String> {
    // TPKT (4), X.224 Connection Confirm (7), then RDP_NEG_RSP or RDP_NEG_FAILURE (8): type, flags, length, value.
    let standard_only = "the RDP server only offers standard RDP security, without TLS (for xrdp: set security_layer=negotiate in /etc/xrdp/xrdp.ini and restart xrdp)";
    if x224.len() < 19 {
        return Some(standard_only.into());
    }
    let value = u32::from_le_bytes([x224[15], x224[16], x224[17], x224[18]]);
    match x224[11] {
        0x02 if value == 0 => Some(standard_only.into()),
        0x03 => Some(match value {
            2 => standard_only.into(), // SSL_NOT_ALLOWED_BY_SERVER
            3 => "the RDP server has no TLS certificate (for xrdp: see certificate= and key_file= in /etc/xrdp/xrdp.ini)".into(),
            _ => format!("the RDP server refused the security negotiation (code {value})"),
        }),
        _ => None,
    }
}

/// xrdp signs in with the Client Info PDU's credentials only when its INFO_AUTOLOGON flag is set, and otherwise shows
/// its own login window; IronRDP's web client never sets it. The proxy has the client's side in the clear (it does the
/// TLS), so it sets the flag in that one PDU, in place, and passes everything else through untouched.
#[derive(Default)]
struct Autologon {
    pending: Vec<u8>,
    done: bool,
}

impl Autologon {
    const INFO_AUTOLOGON: u32 = 0x08;
    const SEC_INFO_PKT: u16 = 0x40;

    // TPKT (4), X.224 Data (3: 02 f0 80), MCS Send Data Request (0x64, initiator 2, channel 2, priority 1, PER length
    // 1 or 2), basic security header (flags 2 with SEC_INFO_PKT, flagsHi 2), then TS_INFO_PACKET: CodePage 4, flags 4.
    fn patch(pdu: &mut [u8]) -> bool {
        if pdu.len() < 14 || pdu[4] != 0x02 || pdu[5] != 0xf0 || pdu[7] >> 2 != 25 {
            return false;
        }
        let sec = 13 + if pdu[13] & 0x80 != 0 { 2 } else { 1 };
        if pdu.len() < sec + 12 || u16::from_le_bytes([pdu[sec], pdu[sec + 1]]) & Self::SEC_INFO_PKT == 0 {
            return false;
        }
        let at = sec + 8;
        let flags = u32::from_le_bytes(pdu[at..at + 4].try_into().unwrap()) | Self::INFO_AUTOLOGON;
        pdu[at..at + 4].copy_from_slice(&flags.to_le_bytes());
        true
    }

    /// Client data in, what goes upstream out (whole TPKT frames; a partial one waits for the rest).
    fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        if self.done {
            return data.to_vec();
        }
        self.pending.extend_from_slice(data);
        let mut p = 0;
        // Anything but TPKT (fast-path input) means the connection sequence, and the Client Info PDU with it, is over.
        while self.pending.len() - p >= 4 {
            if self.pending[p] != 0x03 {
                self.done = true;
                break;
            }
            let length = u16::from_be_bytes([self.pending[p + 2], self.pending[p + 3]]) as usize;
            if length < 4 || self.pending.len() - p < length {
                break;
            }
            self.done = Self::patch(&mut self.pending[p..p + length]);
            p += length;
            if self.done {
                break;
            }
        }
        if self.done {
            std::mem::take(&mut self.pending)
        } else {
            let rest = self.pending.split_off(p);
            std::mem::replace(&mut self.pending, rest)
        }
    }
}

// ---- TLS -------------------------------------------------------------------------------------------------------

/// Takes any certificate: RDP servers' are self-signed, and the page pins its fingerprint (check_certificate) before
/// the client sends anything. The handshake's signatures are still verified, so the server holds that certificate's
/// key. Unlike BoringSSL, rustls doesn't enforce key usage, so Windows' key-encipherment-only certificate works with
/// ECDHE and TLS 1.3 (the Electron build needed a TLS 1.2 RSA fallback for it).
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_connector() -> Result<tokio_rustls::TlsConnector, String> {
    static CONFIG: std::sync::OnceLock<Result<Arc<rustls::ClientConfig>, String>> = std::sync::OnceLock::new();
    let config = CONFIG.get_or_init(|| {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth();
        // As Node's tls.connect over a socket: no SNI (RDP servers don't expect it).
        config.enable_sni = false;
        Ok(Arc::new(config))
    });
    config.clone().map(tokio_rustls::TlsConnector::from)
}

// ---- upstream --------------------------------------------------------------------------------------------------

pub(crate) trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}

/// A stream that keeps something alive as long as it is (the ssh process behind it).
struct Guarded<T> {
    io: T,
    _guard: Box<dyn Send>,
}

impl<T: AsyncRead + Unpin> AsyncRead for Guarded<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Guarded<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// A desktop's stream through an SSH connection, read as it arrives into a queue that the reader takes from. The SSH
/// connection carries the terminal too, and russh hands a channel's data over as it comes: a reader that stops (the
/// page not taking the desktop's pictures) would otherwise hold up every channel of that connection. What the reader
/// hasn't taken is capped: past `limit` the desktop is closed rather than growing. The server's end of the stream is
/// the reader's end too (the desktop then ends: RDP has no use for a connection open one way).
struct Drained<T> {
    rx: tokio::sync::mpsc::UnboundedReceiver<std::io::Result<Vec<u8>>>,
    pending: Option<(Vec<u8>, usize)>,
    queued: Arc<AtomicUsize>,
    write: tokio::io::WriteHalf<T>,
    task: tokio::task::AbortHandle,
}

impl<T: AsyncRead + AsyncWrite + Send + 'static> Drained<T> {
    fn new(io: T, limit: usize) -> Self {
        let (mut read, write) = tokio::io::split(io);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let counted = queued.clone();
        let task = tokio::spawn(async move {
            loop {
                let mut buf = vec![0u8; 64 * 1024];
                match read.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.truncate(n);
                        if counted.fetch_add(n, Ordering::Relaxed) + n > limit {
                            let _ = tx.send(Err(std::io::Error::other("the desktop's data isn't being read: connection closed")));
                            break;
                        }
                        if tx.send(Ok(buf)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        Self { rx, pending: None, queued, write, task: task.abort_handle() }
    }
}

impl<T> Drop for Drained<T> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Drained<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        loop {
            if let Some((data, at)) = self.pending.as_mut() {
                let n = buf.remaining().min(data.len() - *at);
                buf.put_slice(&data[*at..*at + n]);
                *at += n;
                if *at == data.len() {
                    self.pending = None;
                }
                self.queued.fetch_sub(n, Ordering::Relaxed);
                return Poll::Ready(Ok(()));
            }
            match std::task::ready!(self.rx.poll_recv(cx)) {
                Some(Ok(data)) => self.pending = Some((data, 0)),
                Some(Err(e)) => return Poll::Ready(Err(e)),
                None => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl<T: AsyncWrite> AsyncWrite for Drained<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.write).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.write).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.write).poll_shutdown(cx)
    }
}

fn format_address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// What a gateway connection asks of the page: through the proxy's events, tagged with the upstream it came from.
struct GatewayHooks<'a> {
    proxy: &'a Proxy,
    upstream: u32,
}

impl gateway::Hooks for GatewayHooks<'_> {
    fn log(&self, message: String) {
        self.proxy.log(message);
    }

    fn check_certificate(&self, fingerprint: String, valid: bool) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        let upstream = self.upstream;
        Box::pin(async move {
            self.proxy.ask(|request| ProxyEvent::GatewayCertificate { request, upstream, fingerprint, valid }).await.map(|_| ())
        })
    }
}

/// Opens where the page said this connection goes (`request`: the number of its answer, for a gateway's events).
async fn open_upstream(proxy: &Proxy, request: u32, upstream: &Upstream) -> Result<Box<dyn Duplex>, String> {
    let Upstream::Gateway { via, gateway, account, target, client_name } = upstream else {
        return open_direct(upstream, proxy.app.as_ref()).await;
    };
    let raw = open_direct(via, proxy.app.as_ref()).await?;
    let hooks = GatewayHooks { proxy, upstream: request };
    match gateway::connect(raw, gateway, account, target, client_name, &hooks).await {
        Ok(stream) => {
            proxy.event(ProxyEvent::GatewaySignedIn { upstream: request });
            Ok(stream)
        }
        Err(gateway::Error::Refused(message)) => {
            proxy.event(ProxyEvent::GatewayRefused { upstream: request, message: message.clone() });
            Err(message)
        }
        Err(gateway::Error::Failed(message)) => Err(message),
    }
}

/// A stream to host:port as one of the ways says (not through a gateway).
async fn open_direct(upstream: &Upstream, app: Option<&tauri::AppHandle>) -> Result<Box<dyn Duplex>, String> {
    use crate::ssh::client::StreamTo;
    let ssh_client = |id: u32| {
        use tauri::Manager;
        app.ok_or_else(|| "no SSH here".to_string())?.state::<crate::ssh::Ssh>().client(id)
    };
    match upstream {
        Upstream::Gateway { .. } => Err("a gateway can't be reached through another gateway".into()),
        Upstream::SshTcp { client, host, port } => {
            let stream = ssh_client(*client)?.open_stream(StreamTo::Tcp { host: host.clone(), port: *port }).await?;
            Ok(Box::new(Drained::new(stream, MAX_UNREAD_SSH)))
        }
        Upstream::SshExec { client, command } => {
            let stream = ssh_client(*client)?.open_stream(StreamTo::Exec { command: command.clone() }).await?;
            Ok(Box::new(Drained::new(stream, MAX_UNREAD_SSH)))
        }
        Upstream::Tcp { host, port } => {
            let address = format_address(host, *port);
            let socket = tokio::time::timeout(Duration::from_secs(15), TcpStream::connect((host.as_str(), *port)))
                .await
                .map_err(|_| format!("{address} did not answer"))?
                .map_err(|e| format!("{address}: {e}"))?;
            let _ = socket.set_nodelay(true);
            // Notices a dead peer (sleep, a dropped VPN) where the SSH path has the SSH connection's keepalive.
            let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(10)).with_interval(Duration::from_secs(10));
            let _ = socket2::SockRef::from(&socket).set_tcp_keepalive(&keepalive);
            Ok(Box::new(socket))
        }
        Upstream::Process { program, args } => {
            let mut child = tokio::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("{program}: {e}"))?;
            let stderr = Arc::new(Mutex::new(String::new()));
            let mut err = child.stderr.take().unwrap();
            let collected = stderr.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = err.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    collected.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
            let mut stdout = BufReader::new(child.stdout.take().unwrap());
            let stdin = child.stdin.take().unwrap();
            let failed = |code: Option<i32>, stderr: &Mutex<String>| {
                let stderr = stderr.lock().unwrap();
                let last = stderr.trim().lines().last().unwrap_or("").to_string();
                if last.is_empty() {
                    format!("{program} failed (exit {})", code.map_or("?".into(), |c| c.to_string()))
                } else {
                    format!("{program} failed: {last}")
                }
            };
            // Fail fast if it can't connect (ssh that would need a password, say): wait for its first output, its
            // exit, or a moment.
            tokio::select! {
                ready = stdout.fill_buf() => {
                    if ready.map(|b| b.is_empty()).unwrap_or(true) {
                        let status = child.wait().await.ok();
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        return Err(failed(status.and_then(|s| s.code()), &stderr));
                    }
                }
                status = child.wait() => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    return Err(failed(status.ok().and_then(|s| s.code()), &stderr));
                }
                _ = tokio::time::sleep(Duration::from_millis(1500)) => {}
            }
            Ok(Box::new(Guarded { io: tokio::io::join(stdout, stdin), _guard: Box::new(child) }))
        }
    }
}

// ---- a connection ----------------------------------------------------------------------------------------------

type WebSocket = tokio_tungstenite::WebSocketStream<TcpStream>;

/// Ends a connection quietly (the client went away, or wasn't one of ours): nothing to tell anyone.
const GONE: &str = "";

/// Counts a client connection while it lives.
struct Client<'a>(&'a AtomicUsize);

impl Drop for Client<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn serve(proxy: Arc<Proxy>, socket: TcpStream) {
    proxy.clients.fetch_add(1, Ordering::Relaxed);
    let _client = Client(&proxy.clients);
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    // Anything on this computer can connect to the port: only a client that knows the token gets further than its first
    // message, which has to be small and arrive soon (the WebSocket upgrade included).
    let hello = tokio::time::timeout(HELLO_TIMEOUT, async {
        let expected = format!("/{}", proxy.token);
        let verify = |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            use subtle::ConstantTimeEq;
            if request.uri().path().as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() != 1 {
                return Err(tokio_tungstenite::tungstenite::http::Response::builder().status(403).body(Some("Forbidden".into())).unwrap());
            }
            Ok(response)
        };
        let mut ws = tokio_tungstenite::accept_hdr_async_with_config(socket, verify, Some(config)).await.ok()?;
        let request = read_hello(&mut ws).await?;
        Some((ws, request))
    })
    .await;
    let Ok(Some((mut ws, request))) = hello else { return };
    match connect(&proxy, &mut ws, request).await {
        Ok(()) => {}
        Err(why) if why == GONE => {}
        Err(why) => {
            proxy.log(format!("RDCleanPath failed: {why}"));
            let _ = ws.send(Message::Binary(encode_error().into())).await;
            let _ = ws.close(None).await;
        }
    }
}

/// The RDCleanPath request, once whole: None when the client went away, or sent more than a request can be.
async fn read_hello(ws: &mut WebSocket) -> Option<Request> {
    let mut buf = Vec::new();
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(data))) => {
                if buf.len() + data.len() > MAX_HELLO {
                    return None;
                }
                buf.extend_from_slice(&data);
            }
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => continue,
        }
        if let Some(outer) = read_tlv(&buf, 0) {
            if buf.len() >= outer.end {
                return Some(decode_request(&buf[..outer.end]).unwrap_or_default());
            }
        }
    }
}

/// The upstream handshake, then the relay.
async fn connect(proxy: &Proxy, ws: &mut WebSocket, request: Request) -> Result<(), String> {
    let pcb = request.pcb.filter(|pcb| !pcb.trim().is_empty());
    let (Some(VERSION_1), Some(destination)) = (request.version, request.destination) else {
        proxy.log("RDCleanPath: a malformed request");
        return Err(GONE.into());
    };
    if request.x224.is_none() && pcb.is_none() {
        proxy.log("RDCleanPath: a request with neither X.224 nor a pre-connection blob");
        return Err(GONE.into());
    }
    use subtle::ConstantTimeEq;
    if request.proxy_auth.as_deref().unwrap_or_default().as_bytes().ct_eq(proxy.token.as_bytes()).unwrap_u8() != 1 {
        proxy.log("RDCleanPath: a client without the token");
        return Err(GONE.into());
    }
    proxy.event(ProxyEvent::Failure { message: None });
    let failed = |why: String| {
        proxy.event(ProxyEvent::Failure { message: Some(why.clone()) });
        why
    };

    // Reaching the server, while watching the client: what it sends meanwhile is kept for the server, and its going away
    // ends the attempt at once (the stream being opened goes with it).
    let mut early: Vec<Vec<u8>> = Vec::new();
    let mut early_size = 0;
    let reached = {
        let handshake = handshake(proxy, pcb.as_deref(), request.x224.as_deref());
        tokio::pin!(handshake);
        tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            loop {
                tokio::select! {
                    reached = &mut handshake => break reached,
                    message = ws.next() => match message {
                        Some(Ok(Message::Binary(data))) => {
                            early_size += data.len();
                            if early_size > MAX_EARLY {
                                break Err("the client sent too much before the server was reached".to_string());
                            }
                            early.push(data.to_vec());
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break Err(GONE.to_string()),
                        Some(Ok(_)) => {}
                    },
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err("the server didn't answer in time".into()))
    };
    let (tls, x224) = match reached {
        Ok(reached) => reached,
        Err(why) if why == GONE => return Err(why),
        Err(why) => return Err(failed(why)),
    };
    let chain: Vec<CertificateDer<'static>> = tls.get_ref().1.peer_certificates().map(|c| c.to_vec()).unwrap_or_default();
    let Some(leaf) = chain.first() else {
        return Err(failed("the server sent no certificate".into()));
    };
    // Before the client gets the response it waits for to start CredSSP: no credentials have been sent yet.
    let server_name = proxy.server_name.lock().unwrap().clone();
    let valid = match server_name { Some(ref name) => gateway::valid_for(name, &chain).await, None => false };
    let reason = if valid { String::new() } else { "Not verified for this server by this computer's certificate authorities".into() };
    let without_nla = x224.as_ref().is_some_and(|confirm| !offers_nla(confirm));
    proxy.check_certificate(fingerprint_of(leaf.as_ref()), valid, reason, without_nla).await.map_err(failed)?;
    ws.send(Message::Binary(encode_response(x224.as_deref(), &chain, &destination).into())).await.map_err(|_| GONE.to_string())?;
    proxy.log(format!("RDCleanPath relay up to {destination}"));
    // PROTOCOL_HYBRID_EX in the server's negotiation response: an Early User Authorization Result follows CredSSP. (A
    // Hyper-V VM's console: CredSSP first, then the X.224 exchange, which never asks for HYBRID_EX.)
    let early_auth = x224.as_ref().is_some_and(|x224| x224.len() >= 19 && x224[11] == 0x02 && x224[15..19] == 8u32.to_le_bytes());
    relay(proxy, ws, tls, early_auth, early).await;
    Ok(())
}

/// To the server: its stream, the pre-connection blob and X.224 (whichever the client asked for), the Network Level
/// Authentication check, then TLS. Returns TLS and the server's X.224 confirm.
async fn handshake(proxy: &Proxy, pcb: Option<&str>, x224_request: Option<&[u8]>) -> Result<(tokio_rustls::client::TlsStream<Box<dyn Duplex>>, Option<Vec<u8>>), String> {
    let (request, upstream) = proxy.upstream().await?;
    let mut raw = open_upstream(proxy, request, &upstream).await?;
    let upstream_error = |e: std::io::Error| format!("upstream: {e}");
    if let Some(pcb) = pcb {
        raw.write_all(&preconnection_pdu(pcb)).await.map_err(upstream_error)?;
    }
    let mut x224 = None;
    if let Some(x224_request) = x224_request {
        raw.write_all(x224_request).await.map_err(upstream_error)?;
        raw.flush().await.map_err(upstream_error)?;
        let confirm = read_tpkt(&mut raw).await?;
        if let Some(refused) = no_tls(&confirm) {
            return Err(refused);
        }
        // Before TLS: a server that isn't to get the password gets nothing more at all.
        if !offers_nla(&confirm) {
            proxy.ask(|request| ProxyEvent::WithoutNla { request }).await?;
        }
        x224 = Some(confirm);
    } else {
        raw.flush().await.map_err(upstream_error)?;
    }
    // No SNI is sent, so the name is only a placeholder.
    let name = ServerName::try_from("rdp.invalid").unwrap();
    let tls = tls_connector()?.connect(name, raw).await.map_err(|e| format!("TLS: {e}"))?;
    Ok((tls, x224))
}

async fn relay(proxy: &Proxy, ws: &mut WebSocket, tls: impl Duplex, early_auth: bool, early: Vec<Vec<u8>>) {
    let (from_server, mut to_server) = tokio::io::split(tls);
    let mut from_server = BufReader::with_capacity(64 * 1024, from_server);
    let (mut to_client, mut from_client) = ws.split();
    let mut autologon = proxy.autologon.then(Autologon::default);
    // Each direction waits for the other side to take what it has before reading more (a slow side holds up the one
    // feeding it, with no more than a socket's buffers waiting); a desktop through SSH has Drained in between.
    let upstream = async {
        let mut early = early.into_iter();
        loop {
            let data = match early.next() {
                Some(data) => data.into(),
                None => match from_client.next().await {
                    Some(Ok(Message::Binary(data))) => data,
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                },
            };
            let count = data.len();
            let result = match autologon.as_mut() {
                Some(patcher) => {
                    let out = patcher.feed(&data);
                    if out.is_empty() { Ok(()) } else { to_server.write_all(&out).await }
                }
                None => to_server.write_all(&data).await,
            };
            // TLS may accept plaintext while the encrypted write is still backpressured. In particular, a
            // graphics acknowledgement must leave before waiting for the next client message or server frame.
            if result.is_err() || to_server.flush().await.is_err() {
                break;
            }
            proxy.bytes_out.fetch_add(count as u64, Ordering::Relaxed);
            proxy.event(ProxyEvent::Drained { bytes: count });
        }
        let _ = to_server.shutdown().await;
    };
    let downstream = async {
        let mut framing = ServerFraming { rdp: false, early_auth };
        loop {
            match framing.read(&mut from_server).await {
                // The server's end of the stream ends the desktop: through an SSH host that may be all there is to hear
                // of a server that went away.
                Ok(None) => break,
                Ok(Some(pdu)) => {
                    proxy.bytes_in.fetch_add(pdu.len() as u64, Ordering::Relaxed);
                    if to_client.send(Message::Binary(pdu.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    proxy.log(format!("upstream error: {e}"));
                    break;
                }
            }
        }
        let _ = to_client.close().await;
    };
    // Either side ending ends the connection, and with it the stream to the server.
    tokio::select! {
        _ = upstream => {}
        _ = downstream => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn server_frames_survive_every_read_boundary() {
        let mut long_credssp = vec![0x30, 0x81, 0x80];
        long_credssp.extend_from_slice(&[0; 128]);
        let packets = vec![
            vec![0x30, 3, 2, 1, 6], // CredSSP, short DER length
            long_credssp,
            vec![0, 0, 0, 0], // HYBRID_EX success
            vec![3, 0, 0, 6, 0xaa, 0xbb], // TPKT
            vec![0, 4, 0xcc, 0xdd], // fast-path, short length
            vec![0x80, 0x80, 5, 0xee, 0xff], // fast-path, two-byte length
            vec![3, 0, 0, 5, 0x42],
        ];
        let bytes = packets.concat();
        for split in 1..bytes.len() {
            // Two separately available chunks, including splits within every header and payload; BufReader can
            // also return several packets at once. The WebSocket boundary must be independent of both.
            let (mut tx, rx) = tokio::io::duplex(bytes.len());
            let bytes = bytes.clone();
            let writer = tokio::spawn(async move {
                tx.write_all(&bytes[..split]).await.unwrap();
                tokio::task::yield_now().await;
                tx.write_all(&bytes[split..]).await.unwrap();
            });
            let mut rx = BufReader::new(rx);
            let mut framing = ServerFraming { rdp: false, early_auth: true };
            for expected in &packets {
                assert_eq!(framing.read(&mut rx).await.unwrap().as_ref(), Some(expected), "split {split}");
            }
            assert!(framing.read(&mut rx).await.unwrap().is_none());
            writer.await.unwrap();
        }
    }

    #[tokio::test]
    async fn server_frames_reject_invalid_lengths_and_truncation() {
        for bytes in [
            vec![0x30, 0x80], // indefinite DER
            vec![0x30, 0x85], // overflowing DER length
            vec![0x30, 0x84, 0xff, 0xff, 0xff, 0xff], // excessive allocation
            vec![3, 0, 0, 3], // shorter than its TPKT header
            vec![3, 1, 0, 4], // reserved byte
            vec![0, 1], // shorter than its fast-path header
            vec![0, 0x80, 2],
            vec![3],
            vec![3, 0, 0, 8, 1], // truncated payload
        ] {
            let mut framing = ServerFraming { rdp: bytes[0] != 0x30, early_auth: false };
            assert!(framing.read(&mut bytes.as_slice()).await.is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn request_round_trip() {
        let x224 = vec![3, 0, 0, 19, 14, 0xe0, 0, 0, 0, 0, 0, 1, 0, 8, 0, 3, 0, 0, 0];
        let req = seq(&[
            explicit(0, der_integer(VERSION_1)),
            explicit(2, tlv(0x0c, b"10.0.0.1:3389")),
            explicit(3, tlv(0x0c, b"token")),
            explicit(6, tlv(0x04, &x224)),
        ]);
        let decoded = decode_request(&req).unwrap();
        assert_eq!(decoded.version, Some(VERSION_1));
        assert_eq!(decoded.destination.as_deref(), Some("10.0.0.1:3389"));
        assert_eq!(decoded.proxy_auth.as_deref(), Some("token"));
        assert_eq!(decoded.x224, Some(x224));
        assert!(decode_request(&req[..req.len() - 1]).is_err());
    }

    #[test]
    fn der_encoding() {
        assert_eq!(der_integer(3390), vec![0x02, 0x02, 0x0d, 0x3e]);
        assert_eq!(der_integer(1), vec![0x02, 0x01, 0x01]);
        assert_eq!(der_integer(128), vec![0x02, 0x02, 0x00, 0x80]);
        let long = tlv(0x04, &[0u8; 300]);
        assert_eq!(&long[..4], &[0x04, 0x82, 0x01, 0x2c]);
        let parsed = read_tlv(&long, 0).unwrap();
        assert_eq!((parsed.start, parsed.end), (4, 304));
        assert_eq!(encode_error(), vec![0x30, 0x0f, 0xa0, 0x04, 0x02, 0x02, 0x0d, 0x3e, 0xa1, 0x07, 0x30, 0x05, 0xa0, 0x03, 0x02, 0x01, 0x01]);
    }

    #[test]
    fn negotiation() {
        let mut confirm = vec![3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 1, 0, 0, 0];
        assert!(no_tls(&confirm).is_none());
        confirm[15] = 0;
        assert!(no_tls(&confirm).unwrap().contains("standard RDP security"));
        confirm[11] = 3;
        confirm[15] = 3;
        assert!(no_tls(&confirm).unwrap().contains("no TLS certificate"));
        assert!(no_tls(&confirm[..11]).is_some());
    }

    #[test]
    fn autologon_sets_the_flag_once() {
        // TPKT, X.224 Data, MCS SDrq (0x64), initiator, channel, priority, PER length 1, security header, info packet.
        let mut pdu = vec![3, 0, 0, 0, 0x02, 0xf0, 0x80, 0x64, 0, 1, 0x03, 0xeb, 0x70, 0x10, 0x40, 0x00, 0, 0, 0, 0, 0, 0, 0x33, 0x00, 0, 0];
        let len = pdu.len() as u16;
        pdu[2..4].copy_from_slice(&len.to_be_bytes());
        let mut a = Autologon::default();
        // Split across two messages: nothing goes up until the frame is whole.
        assert!(a.feed(&pdu[..10]).is_empty());
        let out = a.feed(&pdu[10..]);
        assert_eq!(out.len(), pdu.len());
        assert_eq!(u32::from_le_bytes(out[22..26].try_into().unwrap()), 0x33 | 0x08);
        assert!(a.done);
        assert_eq!(a.feed(&[1, 2, 3]), vec![1, 2, 3]);
        // Other TPKT frames before it pass through untouched.
        let mut b = Autologon::default();
        let other = [3u8, 0, 0, 6, 9, 9];
        assert_eq!(b.feed(&other), other.to_vec());
        assert!(!b.done);
    }

    #[test]
    fn fingerprints() {
        assert_eq!(fingerprint_of(b"").len(), 95);
        assert!(fingerprint_of(b"").starts_with("E3:B0:C4:42"));
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;
    use rustls::pki_types::PrivateKeyDer;
    use std::sync::mpsc;

    // GNOME can put these in the same TLS read: the activation's last PDU followed by the request that starts
    // dynamic-channel negotiation. IronRDP must receive them in separate WebSocket messages or loses the latter.
    const FONT_MAP: &[u8] = &[3, 0, 0, 41, 2, 240, 128, 104, 0, 8, 3, 235, 112, 128, 26, 26, 0, 23, 0, 241, 3, 241, 3, 1, 0, 0, 1, 8, 0, 40, 0, 0, 0, 0, 0, 0, 0, 3, 0, 4, 0];
    const DVC_CAPABILITIES: &[u8] = &[3, 0, 0, 27, 2, 240, 128, 104, 0, 8, 3, 238, 112, 128, 12, 4, 0, 0, 0, 3, 0, 0, 0, 80, 0, 1, 0];

    /// An RDP server as far as the proxy sees it: X.224, TLS with a certificate that allows key encipherment only
    /// (like Windows' self-signed one), then coalesced activation/channel PDUs and an echo.
    async fn fake_rdp_server() -> (u16, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["rdp-test".into()]).unwrap();
        params.key_usages = vec![rcgen::KeyUsagePurpose::KeyEncipherment];
        let cert = params.self_signed(&key).unwrap();
        let fingerprint = fingerprint_of(cert.der().as_ref());
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], PrivateKeyDer::Pkcs8(key.serialize_der().into()))
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let request = read_tpkt(&mut socket).await.unwrap();
                    assert_eq!(request[5], 0xe0);
                    // Connection Confirm with RDP_NEG_RSP: PROTOCOL_SSL.
                    socket.write_all(&[3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 1, 0, 0, 0]).await.unwrap();
                    let mut tls = acceptor.accept(socket).await.unwrap();
                    tls.write_all(&[FONT_MAP, DVC_CAPABILITIES].concat()).await.unwrap();
                    let mut buf = [0u8; 1024];
                    loop {
                        match tls.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => if tls.write_all(&buf[..n]).await.is_err() { break },
                        }
                    }
                });
            }
        });
        (port, fingerprint)
    }

    fn events() -> (Channel<ProxyEvent>, mpsc::Receiver<serde_json::Value>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let channel = Channel::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                let _ = tx.lock().unwrap().send(serde_json::from_str(&json).unwrap());
            }
            Ok(())
        });
        (channel, rx)
    }

    fn request(token: &str) -> Vec<u8> {
        // Connection Request with RDP_NEG_REQ asking for TLS.
        let x224 = [3u8, 0, 0, 19, 14, 0xe0, 0, 0, 0, 0, 0, 1, 0, 8, 0, 1, 0, 0, 0];
        seq(&[
            explicit(0, der_integer(VERSION_1)),
            explicit(2, tlv(0x0c, b"rdp-test:3389")),
            explicit(3, tlv(0x0c, token.as_bytes())),
            explicit(6, tlv(0x04, &x224)),
        ])
    }

    /// Connects a client, answers as the page would (the upstream, and `verdict` on the certificate), and returns the
    /// first message the client gets.
    async fn client(proxy: &Arc<Proxy>, port: u16, rx: &mpsc::Receiver<serde_json::Value>, upstream: &serde_json::Value, verdict: Option<&str>) -> (WebSocket, Vec<u8>, Option<String>) {
        let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/{}", proxy.token), socket).await.unwrap();
        let req = request(&proxy.token);
        // In two pieces: the proxy waits for the whole request.
        ws.send(Message::Binary(req[..7].to_vec().into())).await.unwrap();
        ws.send(Message::Binary(req[7..].to_vec().into())).await.unwrap();
        let mut fingerprint = None;
        let reply = loop {
            tokio::select! {
                message = ws.next() => break message.unwrap().unwrap().into_data().to_vec(),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    while let Ok(event) = rx.try_recv() {
                        let answer = match event["event"].as_str() {
                            Some("upstream") => Ok(upstream.clone()),
                            Some("withoutNla") => Ok(serde_json::Value::Null),
                            Some("certificate") => {
                                fingerprint = event["fingerprint"].as_str().map(String::from);
                                verdict.map_or(Ok(serde_json::Value::Null), |why| Err(why.to_string()))
                            }
                            _ => continue,
                        };
                        let request = event["request"].as_u64().unwrap() as u32;
                        proxy.answers.lock().unwrap().remove(&request).unwrap().send(answer).unwrap();
                    }
                }
            }
        };
        (ws, reply, fingerprint)
    }

    async fn relays_through(upstream: serde_json::Value, server_fingerprint: &str) {
        let (channel, rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        // Reuse the proxy after disconnecting: every connection has its own framing state.
        for _ in 0..2 {
            let (mut ws, reply, fingerprint) = client(&proxy, port, &rx, &upstream, None).await;
            assert_eq!(fingerprint.as_deref(), Some(server_fingerprint));
            // The response: version, the server's X.224 confirm, its certificate, the address.
            let outer = read_tlv(&reply, 0).unwrap();
            assert_eq!((outer.tag, outer.end), (0x30, reply.len()));
            assert!(reply.windows(19).any(|w| w == [3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 1, 0, 0, 0]));
            assert!(reply.ends_with(b"rdp-test:3389"));
            // Then the relay, in the clear on this side.
            let activation = ws.next().await.unwrap().unwrap().into_data();
            assert_eq!(&activation[..], FONT_MAP);
            let capabilities = ws.next().await.unwrap().unwrap().into_data();
            assert_eq!(&capabilities[..], DVC_CAPABILITIES);
            let ping = [3, 0, 0, 8, b'p', b'i', b'n', b'g'];
            // The server echoes two separate writes. Partial packets must be joined before delivery to the client.
            ws.send(Message::Binary(ping[..3].to_vec().into())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
            ws.send(Message::Binary(ping[3..].to_vec().into())).await.unwrap();
            let echo = ws.next().await.unwrap().unwrap().into_data();
            assert_eq!(&echo[..], &ping);
            ws.close(None).await.unwrap();
        }
        assert_eq!(proxy.bytes_in.load(Ordering::Relaxed), 2 * (FONT_MAP.len() + DVC_CAPABILITIES.len() + 8) as u64);
        assert_eq!(proxy.bytes_out.load(Ordering::Relaxed), 16);
        listener.abort();
    }

    #[tokio::test]
    async fn relays_over_tcp() {
        let (server, fingerprint) = fake_rdp_server().await;
        relays_through(serde_json::json!({ "kind": "tcp", "host": "127.0.0.1", "port": server }), &fingerprint).await;
    }

    #[tokio::test]
    async fn flushes_a_lone_client_message_before_waiting_for_more() {
        let (channel, _) = events();
        let (proxy, _, unused_listener) = start(None, false, channel).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (upstream, mut peer) = tokio::io::duplex(1024);
        let serving = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            // Like TLS under backpressure: accepting bytes need not put them on the wire until flush.
            relay(&proxy, &mut ws, tokio::io::BufStream::new(upstream), false, vec![]).await;
        });
        let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}"), socket).await.unwrap();
        let packet = [3, 0, 0, 8, b'a', b'c', b'k', b'!'];
        ws.send(Message::Binary(packet.to_vec().into())).await.unwrap();
        let mut received = [0; 8];
        tokio::time::timeout(Duration::from_secs(1), peer.read_exact(&mut received)).await.unwrap().unwrap();
        assert_eq!(received, packet);
        // The server can now send its next frame without another client input waking the writer.
        peer.write_all(FONT_MAP).await.unwrap();
        assert_eq!(&tokio::time::timeout(Duration::from_secs(1), ws.next()).await.unwrap().unwrap().unwrap().into_data()[..], FONT_MAP);
        ws.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), serving).await.unwrap().unwrap();
        unused_listener.abort();
    }

    #[tokio::test]
    async fn client_pdus_survive_coalescing_and_websocket_fragmentation() {
        use tokio_tungstenite::tungstenite::protocol::frame::{coding::{Data, OpCode}, Frame};

        let (server, _) = fake_rdp_server().await;
        let (channel, rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let upstream = serde_json::json!({ "kind": "tcp", "host": "127.0.0.1", "port": server });
        let (mut ws, _, _) = client(&proxy, port, &rx, &upstream, None).await;
        assert_eq!(&ws.next().await.unwrap().unwrap().into_data()[..], FONT_MAP);
        assert_eq!(&ws.next().await.unwrap().unwrap().into_data()[..], DVC_CAPABILITIES);

        // Audio virtual-channel traffic can put several PDUs in one message, larger than a TLS record or a
        // WebSocket frame. Check every byte through the real WebSocket/TLS relay, including continuation frames.
        let packets: Vec<Vec<u8>> = [1600, 48000, 48000].into_iter().map(|len| {
            let mut pdu: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            pdu[..2].copy_from_slice(&[3, 0]);
            pdu[2..4].copy_from_slice(&(len as u16).to_be_bytes());
            pdu
        }).collect();
        let bytes = packets.concat();
        for fragmented in [false, true] {
            if fragmented {
                let chunks: Vec<_> = bytes.chunks(16384).collect();
                for (i, chunk) in chunks.iter().enumerate() {
                    let opcode = OpCode::Data(if i == 0 { Data::Binary } else { Data::Continue });
                    let frame = Frame::message(chunk.to_vec(), opcode, i + 1 == chunks.len());
                    ws.send(Message::Frame(frame)).await.unwrap();
                }
            } else {
                ws.send(Message::Binary(bytes.clone().into())).await.unwrap();
            }
            for packet in &packets {
                let echo = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap().into_data();
                assert_eq!(&echo[..], packet, "fragmented: {fragmented}");
            }
        }
        assert_eq!(proxy.bytes_out.load(Ordering::Relaxed), 2 * bytes.len() as u64);
        ws.close(None).await.unwrap();
        listener.abort();
    }

    #[tokio::test]
    async fn relays_over_a_process() {
        let (server, fingerprint) = fake_rdp_server().await;
        relays_through(serde_json::json!({ "kind": "process", "program": "nc", "args": ["127.0.0.1", server.to_string()] }), &fingerprint).await;
    }

    #[tokio::test]
    async fn refused_certificate_ends_it() {
        let (server, _) = fake_rdp_server().await;
        let (channel, rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let upstream = serde_json::json!({ "kind": "tcp", "host": "127.0.0.1", "port": server });
        let (mut ws, reply, _) = client(&proxy, port, &rx, &upstream, Some("the certificate changed")).await;
        assert_eq!(reply, encode_error());
        assert!(matches!(ws.next().await, Some(Ok(Message::Close(_))) | None));
        let logs: Vec<_> = rx.try_iter().filter(|e| e["event"] == "log").map(|e| e["message"].as_str().unwrap().to_string()).collect();
        assert!(logs.iter().any(|m| m == "RDCleanPath failed: the certificate changed"), "{logs:?}");
        listener.abort();
    }

    #[tokio::test]
    async fn failing_process_says_why() {
        let upstream = Upstream::Process { program: "sh".into(), args: vec!["-c".into(), "echo 'Permission denied (publickey).' >&2; exit 255".into()] };
        let err = open_direct(&upstream, None).await.err().unwrap();
        assert_eq!(err, "sh failed: Permission denied (publickey).");
    }
}

#[cfg(test)]
mod hardening_tests {
    // The proxy's limits and checks from tabby-rdp 0.5.0's review (its test/unit/proxy.ts), and the Hyper-V and Network
    // Level Authentication paths, against made-up servers.
    use super::*;
    use std::sync::mpsc;

    fn events() -> (Channel<ProxyEvent>, mpsc::Receiver<serde_json::Value>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let channel = Channel::new(move |body| {
            if let tauri::ipc::InvokeResponseBody::Json(json) = body {
                let _ = tx.lock().unwrap().send(serde_json::from_str(&json).unwrap());
            }
            Ok(())
        });
        (channel, rx)
    }

    fn request(token: &str, pcb: Option<&str>, x224: bool) -> Vec<u8> {
        let mut fields = vec![explicit(0, der_integer(VERSION_1)), explicit(2, tlv(0x0c, b"rdp-test:3389")), explicit(3, tlv(0x0c, token.as_bytes()))];
        if let Some(pcb) = pcb {
            fields.push(explicit(5, tlv(0x0c, pcb.as_bytes())));
        }
        if x224 {
            fields.push(explicit(6, tlv(0x04, &[3, 0, 0, 19, 14, 0xe0, 0, 0, 0, 0, 0, 1, 0, 8, 0, 11, 0, 0, 0])));
        }
        seq(&fields)
    }

    async fn ws_client(port: u16, token: &str) -> WebSocket {
        let socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/{token}"), socket).await.unwrap().0
    }

    /// Answers the proxy's questions as the page would, until `stop`: the upstream, the certificate (accepted) and the
    /// Network Level Authentication question (`nla`).
    fn answer(proxy: &Arc<Proxy>, rx: &mpsc::Receiver<serde_json::Value>, upstream: &serde_json::Value, nla: Result<(), &str>, seen: &mut Vec<String>) {
        while let Ok(event) = rx.try_recv() {
            let kind = event["event"].as_str().unwrap_or_default().to_owned();
            let answer = match kind.as_str() {
                "upstream" => Ok(upstream.clone()),
                "certificate" => Ok(serde_json::Value::Null),
                "withoutNla" => nla.map(|_| serde_json::Value::Null).map_err(String::from),
                "failure" => {
                    seen.push(format!("failure:{}", event["message"].as_str().unwrap_or("")));
                    continue;
                }
                _ => continue,
            };
            seen.push(kind);
            let request = event["request"].as_u64().unwrap() as u32;
            if let Some(tx) = proxy.answers.lock().unwrap().remove(&request) {
                let _ = tx.send(answer);
            }
        }
    }

    /// A TLS server with a fresh certificate, as `accept(socket)` returns it.
    fn acceptor() -> tokio_rustls::TlsAcceptor {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["rdp-test".into()]).unwrap().self_signed(&key).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions().unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()))
            .unwrap();
        tokio_rustls::TlsAcceptor::from(Arc::new(config))
    }

    /// Whether the client's connection is closed within a second (a close frame, an error or the end).
    async fn closed(ws: &mut WebSocket) -> bool {
        matches!(tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match ws.next().await {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return true,
                    Some(Ok(_)) => {}
                }
            }
        }).await, Ok(true))
    }

    #[tokio::test]
    async fn a_first_message_past_a_requests_size_ends_it_before_any_token() {
        let (channel, rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let mut ws = ws_client(port, &proxy.token).await;
        // A SEQUENCE that claims 1 MB, sent in pieces: never kept past 64 KiB.
        let mut big = vec![0x30, 0x83, 0x10, 0x00, 0x00];
        big.resize(40 * 1024, 0);
        ws.send(Message::Binary(big.clone().into())).await.unwrap();
        let _ = ws.send(Message::Binary(big.into())).await;
        assert!(closed(&mut ws).await);
        assert!(rx.try_iter().all(|e| e["event"] != "upstream"));
        listener.abort();
    }

    #[tokio::test]
    async fn a_wrong_token_or_a_malformed_request_gets_nothing() {
        let (channel, rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        for hello in [request("not the token", None, true), request("", None, false), vec![0x30, 0x03, 0x02, 0x01, 0x05]] {
            let mut ws = ws_client(port, &proxy.token).await;
            ws.send(Message::Binary(hello.into())).await.unwrap();
            assert!(closed(&mut ws).await);
        }
        assert!(rx.try_iter().all(|e| e["event"] != "upstream" && e["event"] != "failure"));
        listener.abort();
    }

    #[tokio::test]
    async fn a_silent_client_is_dropped_after_the_deadline() {
        let (channel, _rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let mut ws = ws_client(port, &proxy.token).await;
        let started = std::time::Instant::now();
        assert!(closed(&mut ws).await);
        assert!(started.elapsed() >= HELLO_TIMEOUT - Duration::from_millis(50));
        // Not even the WebSocket upgrade: the same.
        let mut raw = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), raw.read(&mut buf)).await.unwrap().unwrap_or(0), 0);
        listener.abort();
    }

    #[tokio::test]
    async fn no_more_than_a_few_clients_at_once() {
        let (channel, _rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let mut idle = Vec::new();
        for _ in 0..MAX_CLIENTS {
            idle.push(ws_client(port, &proxy.token).await);
        }
        while proxy.clients.load(Ordering::Relaxed) < MAX_CLIENTS { tokio::task::yield_now().await; }
        let mut extra = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(tokio::time::timeout(Duration::from_secs(1), extra.read(&mut buf)).await.unwrap().unwrap_or(0), 0);
        // Once they're gone (the deadline), there is room again.
        drop(idle);
        tokio::time::timeout(Duration::from_secs(3), async { while proxy.clients.load(Ordering::Relaxed) > 0 { tokio::time::sleep(Duration::from_millis(20)).await } }).await.unwrap();
        let _again = ws_client(port, &proxy.token).await;
        listener.abort();
    }

    #[tokio::test]
    async fn closing_the_proxy_ends_its_clients_idle_ones_too() {
        let (channel, _rx) = events();
        let proxies = Proxies::default();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let id = proxies.insert(Running { owner: "page".into(), generation: proxies.generation("page"), proxy: proxy.clone(), listener }).unwrap();
        let mut ws = ws_client(port, &proxy.token).await;
        proxies.table.lock().unwrap().running.remove(&id);
        assert!(closed(&mut ws).await);
    }

    #[tokio::test]
    async fn a_client_that_leaves_while_the_server_is_reached_takes_the_connection_with_it() {
        // A server that accepts and never answers the X.224 request.
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_port = server.local_addr().unwrap().port();
        let (channel, rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let mut ws = ws_client(port, &proxy.token).await;
        ws.send(Message::Binary(request(&proxy.token, None, true).into())).await.unwrap();
        let upstream = serde_json::json!({ "kind": "tcp", "host": "127.0.0.1", "port": server_port });
        let mut seen = Vec::new();
        let accepted = tokio::spawn(async move { server.accept().await.unwrap().0 });
        while !accepted.is_finished() {
            answer(&proxy, &rx, &upstream, Ok(()), &mut seen);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut socket = accepted.await.unwrap();
        let mut x224 = [0u8; 19];
        socket.read_exact(&mut x224).await.unwrap();
        ws.close(None).await.unwrap();
        drop(ws);
        // The proxy's end of it closes at once, long before its deadline.
        let mut rest = [0u8; 1];
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), socket.read(&mut rest)).await.unwrap().unwrap_or(0), 0);
        listener.abort();
    }

    /// A server that reads the pre-connection PDU (and, with `x224`, a connection request), then does TLS and sends one
    /// PDU. Returns what it read before TLS.
    async fn hyperv_server(x224: bool) -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = acceptor();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut size = [0u8; 4];
            socket.read_exact(&mut size).await.unwrap();
            let mut pcb = size.to_vec();
            pcb.resize(u32::from_le_bytes(size) as usize, 0);
            socket.read_exact(&mut pcb[4..]).await.unwrap();
            if x224 {
                pcb.extend(read_tpkt(&mut socket).await.unwrap());
                socket.write_all(&[3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 2, 0, 0, 0]).await.unwrap();
            }
            let mut tls = acceptor.accept(socket).await.unwrap();
            tls.write_all(&[3, 0, 0, 5, 0x42]).await.unwrap();
            let mut buf = [0u8; 64];
            let _ = tls.read(&mut buf).await;
            pcb
        });
        (port, task)
    }

    async fn through_hyperv(x224: bool) -> (Vec<u8>, Vec<u8>, Vec<String>) {
        let (server, task) = hyperv_server(x224).await;
        let (channel, rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let mut ws = ws_client(port, &proxy.token).await;
        ws.send(Message::Binary(request(&proxy.token, Some("0b1c-guid;EnhancedMode=1"), x224).into())).await.unwrap();
        let upstream = serde_json::json!({ "kind": "tcp", "host": "127.0.0.1", "port": server });
        let mut seen = Vec::new();
        let response = loop {
            tokio::select! {
                message = ws.next() => break message.unwrap().unwrap().into_data().to_vec(),
                _ = tokio::time::sleep(Duration::from_millis(10)) => answer(&proxy, &rx, &upstream, Ok(()), &mut seen),
            }
        };
        let pdu = ws.next().await.unwrap().unwrap().into_data().to_vec();
        assert_eq!(pdu, [3, 0, 0, 5, 0x42]);
        listener.abort();
        (task.await.unwrap(), response, seen)
    }

    #[test]
    fn a_preconnection_pdu_is_version_2_with_the_text_in_utf16() {
        let pdu = preconnection_pdu("ab");
        assert_eq!(pdu, [24, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, b'a', 0, b'b', 0, 0, 0]);
    }

    #[tokio::test]
    async fn a_hyperv_console_gets_the_blob_first_then_tls_at_once() {
        let (before_tls, response, seen) = through_hyperv(false).await;
        assert_eq!(before_tls, preconnection_pdu("0b1c-guid;EnhancedMode=1"));
        // No X.224 in the response (the client does it after CredSSP), and no question about NLA (there was no X.224).
        let outer = read_tlv(&response, 0).unwrap();
        let mut tags = Vec::new();
        let mut p = outer.start;
        while p < outer.end {
            let field = read_tlv(&response, p).unwrap();
            tags.push(field.tag);
            p = field.end;
        }
        assert_eq!(tags, [0xa0, 0xa7, 0xa9]);
        assert!(!seen.contains(&"withoutNla".to_owned()));
    }

    #[tokio::test]
    async fn a_blob_and_an_x224_request_go_both_the_blob_first() {
        let (before_tls, _, _) = through_hyperv(true).await;
        let pcb = preconnection_pdu("0b1c-guid;EnhancedMode=1");
        assert_eq!(&before_tls[..pcb.len()], &pcb[..]);
        assert_eq!(before_tls[pcb.len()], 3); // then the TPKT
    }

    #[test]
    fn what_the_server_chose_for_network_level_authentication() {
        let confirm = |kind: u8, protocol: u8| [3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, kind, 0, 8, 0, protocol, 0, 0, 0];
        assert!(offers_nla(&confirm(2, 2))); // HYBRID
        assert!(offers_nla(&confirm(2, 8))); // HYBRID_EX
        assert!(!offers_nla(&confirm(2, 1))); // TLS only
        assert!(!offers_nla(&confirm(3, 2))); // a failure, not a choice
        assert!(!offers_nla(&confirm(2, 2)[..15]));
    }

    #[tokio::test]
    async fn a_server_without_nla_is_asked_about_before_anything_more_goes_to_it() {
        for allowed in [false, true] {
            // TLS only, as xrdp answers; then whatever comes: TLS's ClientHello, or nothing.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let server_port = listener.local_addr().unwrap().port();
            let acceptor = acceptor();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_tpkt(&mut socket).await.unwrap();
                socket.write_all(&[3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 1, 0, 0, 0]).await.unwrap();
                if allowed {
                    let mut tls = acceptor.accept(socket).await.unwrap();
                    tls.write_all(&[3, 0, 0, 5, 0x42]).await.unwrap();
                    let mut buf = [0u8; 16];
                    let _ = tls.read(&mut buf).await;
                    0
                } else {
                    let mut rest = Vec::new();
                    let _ = socket.read_to_end(&mut rest).await;
                    rest.len()
                }
            });
            let (channel, rx) = events();
            let (proxy, port, listener) = start(None, false, channel).await.unwrap();
            let mut ws = ws_client(port, &proxy.token).await;
            ws.send(Message::Binary(request(&proxy.token, None, true).into())).await.unwrap();
            let upstream = serde_json::json!({ "kind": "tcp", "host": "127.0.0.1", "port": server_port });
            let mut seen = Vec::new();
            let nla = if allowed { Ok(()) } else { Err("the server doesn't use Network Level Authentication") };
            let reply = loop {
                tokio::select! {
                    message = ws.next() => break message.unwrap().unwrap().into_data().to_vec(),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => answer(&proxy, &rx, &upstream, nla, &mut seen),
                }
            };
            answer(&proxy, &rx, &upstream, nla, &mut seen);
            assert!(seen.contains(&"withoutNla".to_owned()));
            if allowed {
                assert_ne!(reply, encode_error());
                assert_eq!(&ws.next().await.unwrap().unwrap().into_data()[..], &[3, 0, 0, 5, 0x42]);
            } else {
                assert_eq!(reply, encode_error());
                assert!(seen.contains(&"failure:the server doesn't use Network Level Authentication".to_owned()), "{seen:?}");
                drop(ws);
                // Not even TLS's first message went to it.
                assert_eq!(tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap(), 0);
            }
            listener.abort();
        }
    }

    #[tokio::test]
    async fn a_server_that_closes_its_side_ends_the_desktop() {
        // It shuts down its writing side and keeps reading: the stream ends one way, and is never "closed".
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server_port = listener.local_addr().unwrap().port();
        let acceptor = acceptor();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_tpkt(&mut socket).await.unwrap();
            socket.write_all(&[3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 2, 0, 0, 0]).await.unwrap();
            let mut tls = acceptor.accept(socket).await.unwrap();
            tls.write_all(&[3, 0, 0, 5, 0x42]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            tls.shutdown().await.unwrap();
            let mut buf = [0u8; 64];
            while tls.read(&mut buf).await.is_ok_and(|n| n > 0) {}
        });
        let (channel, rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let mut ws = ws_client(port, &proxy.token).await;
        ws.send(Message::Binary(request(&proxy.token, None, true).into())).await.unwrap();
        let upstream = serde_json::json!({ "kind": "tcp", "host": "127.0.0.1", "port": server_port });
        let mut seen = Vec::new();
        loop {
            tokio::select! {
                message = ws.next() => { message.unwrap().unwrap(); break }
                _ = tokio::time::sleep(Duration::from_millis(10)) => answer(&proxy, &rx, &upstream, Ok(()), &mut seen),
            }
        }
        assert_eq!(&ws.next().await.unwrap().unwrap().into_data()[..], &[3, 0, 0, 5, 0x42]);
        assert!(closed(&mut ws).await);
        listener.abort();
    }

    #[tokio::test]
    async fn early_client_data_waits_for_the_server_and_too_much_ends_it() {
        let (server, _) = fake_rdp_server_echo().await;
        let (channel, rx) = events();
        let (proxy, port, listener) = start(None, false, channel).await.unwrap();
        let upstream = serde_json::json!({ "kind": "tcp", "host": "127.0.0.1", "port": server });
        let mut ws = ws_client(port, &proxy.token).await;
        ws.send(Message::Binary(request(&proxy.token, None, true).into())).await.unwrap();
        let early = [3u8, 0, 0, 6, b'h', b'i'];
        ws.send(Message::Binary(early.to_vec().into())).await.unwrap();
        let mut seen = Vec::new();
        loop {
            tokio::select! {
                message = ws.next() => { message.unwrap().unwrap(); break }
                _ = tokio::time::sleep(Duration::from_millis(10)) => answer(&proxy, &rx, &upstream, Ok(()), &mut seen),
            }
        }
        assert_eq!(&ws.next().await.unwrap().unwrap().into_data()[..], &early);
        // More than a connection sequence's worth ahead of the server: refused.
        let mut ws = ws_client(port, &proxy.token).await;
        ws.send(Message::Binary(request(&proxy.token, None, true).into())).await.unwrap();
        for _ in 0..5 {
            let _ = ws.send(Message::Binary(vec![0u8; 64 * 1024].into())).await;
        }
        let reply = tokio::time::timeout(Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap().into_data().to_vec();
        assert_eq!(reply, encode_error());
        listener.abort();
    }

    /// A server that echoes what comes after TLS (X.224 confirm with NLA).
    async fn fake_rdp_server_echo() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = acceptor();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_tpkt(&mut socket).await.unwrap();
            socket.write_all(&[3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 2, 0, 0, 0]).await.unwrap();
            let mut tls = acceptor.accept(socket).await.unwrap();
            let mut buf = [0u8; 1024];
            while let Ok(n) = tls.read(&mut buf).await {
                if n == 0 || tls.write_all(&buf[..n]).await.is_err() { break }
            }
        });
        (port, task)
    }

    #[tokio::test]
    async fn an_unread_ssh_desktop_is_closed_rather_than_growing() {
        let (mut server, client) = tokio::io::duplex(1024);
        let mut drained = Drained::new(client, 64 * 1024);
        // The other side sends far more than the cap while nobody reads (it is never held up: the queue takes it).
        let writer = tokio::spawn(async move {
            let chunk = vec![1u8; 4096];
            for _ in 0..64 {
                if server.write_all(&chunk).await.is_err() { break }
            }
            server
        });
        tokio::time::timeout(Duration::from_secs(2), async { while drained.queued.load(Ordering::Relaxed) <= 64 * 1024 { tokio::task::yield_now().await } }).await.unwrap();
        // Read only now: what was queued, then the reason it stopped.
        let mut total = 0;
        let mut buf = vec![0u8; 8192];
        let err = loop {
            match drained.read(&mut buf).await {
                Ok(0) => panic!("ended instead of failing"),
                Ok(n) => total += n,
                Err(e) => break e,
            }
        };
        assert!(err.to_string().contains("isn't being read"), "{err}");
        assert!(total <= 64 * 1024);
        writer.abort();
        // Under the cap it is an ordinary stream, writes included, and its end is the reader's end.
        let (mut server, client) = tokio::io::duplex(1024);
        let mut drained = Drained::new(client, 1 << 20);
        server.write_all(b"pdu").await.unwrap();
        drop(server);
        let mut got = Vec::new();
        drained.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"pdu");
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    /// Whether the port stops accepting within 2 s (a dropped listener closes once its task next runs).
    async fn closes(port: u16) -> bool {
        for _ in 0..200 {
            if TcpStream::connect(("127.0.0.1", port)).await.is_err() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    #[tokio::test]
    async fn reset_closes_only_its_page_and_rejects_a_late_start() {
        let proxies = Proxies::default();
        let generation = proxies.generation("first");
        let (proxy, port, listener) = start(None, false, Channel::new(|_| Ok(()))).await.unwrap();
        let id = proxies.insert(Running { owner: "first".into(), generation, proxy: proxy.clone(), listener }).unwrap();
        let (other, other_port, listener) = start(None, false, Channel::new(|_| Ok(()))).await.unwrap();
        let other_id = proxies.insert(Running { owner: "second".into(), generation: proxies.generation("second"), proxy: other, listener }).unwrap();
        let waiting = tokio::spawn({
            let proxy = proxy.clone();
            async move { proxy.upstream().await }
        });
        while proxy.answers.lock().unwrap().is_empty() { tokio::task::yield_now().await; }
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (mut ws, _) = tokio_tungstenite::client_async(format!("ws://127.0.0.1:{port}/{}", proxy.token), stream).await.unwrap();
        proxies.reset("first");
        assert!(proxies.get(id).is_err());
        assert!(proxies.get(other_id).is_ok());
        assert!(tokio::time::timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap().is_err());
        assert!(proxy.answers.lock().unwrap().is_empty());
        assert!(proxy.upstream().await.is_err());
        assert!(tokio::time::timeout(Duration::from_secs(2), ws.next()).await.is_ok());
        assert!(closes(port).await);
        assert!(TcpStream::connect(("127.0.0.1", other_port)).await.is_ok());
        let (late, late_port, listener) = start(None, false, Channel::new(|_| Ok(()))).await.unwrap();
        assert!(proxies.insert(Running { owner: "first".into(), generation, proxy: late, listener }).is_err());
        assert!(closes(late_port).await);
        proxies.reset("second");
    }
}
