//! A stream to an RDP server through a Remote Desktop Gateway ([MS-TSGU], its WebSocket transport, which Windows Server
//! 2012 R2 and later speak), ported from tabby-rdp's gateway.ts: HTTPS to the gateway, signed in to with NTLMv2 bound
//! to the gateway's TLS certificate (ntlm.rs), then a tunnel and a channel to the server, and the RDP connection's
//! bytes inside data packets. What comes out is a plain stream to the server, which the proxy uses like a TCP
//! connection: X.224, TLS and the server's own sign-in go through it unchanged.
//!
//! Devolutions' `ironrdp-mstsgu` (0.0.1) was tried first and not used: it signs in with Basic only (no NTLM, so no
//! channel binding, which gateways require by default), connects its own TCP socket (no SSH host in between) and
//! decides on the certificate itself (no remembering on first use). Not here either: the older RPC-over-HTTP transport,
//! Kerberos, smart cards, pluggable authentication cookies and the UDP side channel.
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;

use super::{fingerprint_of, ntlm, AnyCertificate, Duplex};

#[derive(Clone, Debug, Deserialize)]
pub struct Address {
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Deserialize)]
pub struct Account {
    /// `user`, `DOMAIN\user` or `user@domain`.
    pub username: String,
    #[serde(default)]
    pub domain: Option<String>,
    pub password: String,
}

/// What the connection asks of whoever opened it (the page, through the proxy's events).
pub(super) trait Hooks: Send + Sync {
    fn log(&self, message: String);
    /// The gateway's certificate, before the account is used: its SHA-256 fingerprint (`AB:CD:…`) and whether it is
    /// valid for the gateway's name by this computer's certificate authorities. Err with the reason to refuse it.
    fn check_certificate(&self, fingerprint: String, valid: bool) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>;
}

#[derive(Debug)]
pub(super) enum Error {
    /// The gateway refused the account (its HTTP sign-in, or its connection authorization policy).
    Refused(String),
    Failed(String),
}

impl From<String> for Error {
    fn from(message: String) -> Self { Error::Failed(message) }
}

// ---- packets ([MS-TSGU] 2.2.10) ------------------------------------------------------------------------------------

const PKT_HANDSHAKE_REQUEST: u16 = 0x01;
const PKT_HANDSHAKE_RESPONSE: u16 = 0x02;
const PKT_TUNNEL_CREATE: u16 = 0x04;
const PKT_TUNNEL_RESPONSE: u16 = 0x05;
const PKT_TUNNEL_AUTH: u16 = 0x06;
const PKT_TUNNEL_AUTH_RESPONSE: u16 = 0x07;
const PKT_CHANNEL_CREATE: u16 = 0x08;
const PKT_CHANNEL_RESPONSE: u16 = 0x09;
const PKT_DATA: u16 = 0x0a;
const PKT_SERVICE_MESSAGE: u16 = 0x0b;
const PKT_KEEPALIVE: u16 = 0x0d;
const PKT_CLOSE_CHANNEL: u16 = 0x10;
const PKT_CLOSE_CHANNEL_RESPONSE: u16 = 0x11;

/// This client shows (that is: logs) a consent message the gateway has for its users.
const CAPABILITY_MESSAGING_CONSENT_SIGN: u32 = 0x4;
/// A packet's largest payload: a 16-bit length, kept well under it.
const MAX_DATA: usize = 32 * 1024;
/// What an incoming packet may be at most (a data packet's 64 KiB, and room for a certificate in the tunnel response).
const MAX_PACKET: usize = 256 * 1024;
/// How long each step of setting the tunnel up may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(20);

fn packet(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&((8 + body.len()) as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// A length-prefixed, zero-terminated UTF-16 string, as the tunnel and channel packets carry names.
fn name(text: &str) -> Vec<u8> {
    let value: Vec<u8> = text.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
    [&(value.len() as u16).to_le_bytes()[..], &value].concat()
}

fn handshake_request() -> Vec<u8> {
    // Version 1.0, client version 0, no extended authentication (the HTTP sign-in is the authentication).
    packet(PKT_HANDSHAKE_REQUEST, &[1, 0, 0, 0, 0, 0])
}

fn tunnel_create() -> Vec<u8> {
    let mut body = [0u8; 8];
    body[..4].copy_from_slice(&CAPABILITY_MESSAGING_CONSENT_SIGN.to_le_bytes());
    packet(PKT_TUNNEL_CREATE, &body)
}

fn tunnel_auth(client_name: &str) -> Vec<u8> {
    packet(PKT_TUNNEL_AUTH, &[&[0u8, 0][..], &name(client_name)].concat())
}

fn channel_create(host: &str, port: u16) -> Vec<u8> {
    let mut head = vec![1, 0]; // one resource, no alternative names
    head.extend_from_slice(&port.to_le_bytes());
    head.extend_from_slice(&3u16.to_le_bytes()); // protocol: RDP over TCP
    packet(PKT_CHANNEL_CREATE, &[head, name(host)].concat())
}

fn data_packet(data: &[u8]) -> Vec<u8> {
    packet(PKT_DATA, &[&(data.len() as u16).to_le_bytes()[..], data].concat())
}

/// What a gateway's error code means, in words for the user (the code itself goes to the log).
fn gateway_error(code: u32, target: &str) -> String {
    match code & 0xffff {
        0x59da => format!("the gateway doesn't let this account connect to {target} (its resource authorization policy)"),
        0x59db | 0x59ed => "the gateway doesn't let this account connect (its connection authorization policy)".into(),
        0x59dd => format!("the gateway couldn't reach {target}"),
        0x59e6 => "the gateway has no connections left to give".into(),
        0x59e8 | 0x59e9 | 0x59f9 => "the gateway asks for a way of signing in that isn't supported here (a smart card or a sign-in page)".into(),
        0x59ee => "the gateway has no certificate set up".into(),
        _ => format!("the gateway refused the connection (0x{code:x})"),
    }
}

/// Splits a byte stream into the gateway's packets (8 bytes of header: type, reserved, total length).
#[derive(Default)]
struct PacketReader {
    pending: Vec<u8>,
}

impl PacketReader {
    /// Adds what came; returns the packets now whole, as (type, body).
    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<(u16, Vec<u8>)>, String> {
        self.pending.extend_from_slice(chunk);
        let mut packets = Vec::new();
        let mut at = 0;
        while self.pending.len() - at >= 8 {
            let length = u32::from_le_bytes(self.pending[at + 4..at + 8].try_into().unwrap()) as usize;
            if !(8..=MAX_PACKET).contains(&length) {
                return Err(format!("a gateway packet of {length} bytes"));
            }
            if self.pending.len() - at < length {
                break;
            }
            let kind = u16::from_le_bytes([self.pending[at], self.pending[at + 1]]);
            packets.push((kind, self.pending[at + 8..at + length].to_vec()));
            at += length;
        }
        self.pending.drain(..at);
        Ok(packets)
    }
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4).map(|v| u32::from_le_bytes(v.try_into().unwrap()))
}

// ---- TLS -----------------------------------------------------------------------------------------------------------

fn tls_connector(sni: bool) -> Result<tokio_rustls::TlsConnector, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // Any certificate gets as far as the page's decision (valid, remembered, or a change to ask about); the handshake's
    // signatures are still verified, so the gateway holds that certificate's key.
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    // A name is sent (SNI), an address isn't; either is what the certificate is checked for.
    config.enable_sni = sni;
    Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
}

/// Whether this computer's certificate authorities vouch for `chain` as `host`'s (the system's verifier: the keychain on
/// macOS, the certificate store on Windows, the system bundle on Linux). Run off the async threads: the system may look
/// up revocation over the network.
pub(super) async fn valid_for(host: &str, chain: &[CertificateDer<'static>]) -> bool {
    let Ok(name) = ServerName::try_from(host.to_owned()) else { return false };
    let chain = chain.to_vec();
    tokio::task::spawn_blocking(move || {
        use rustls::client::danger::ServerCertVerifier;
        let Some((leaf, intermediates)) = chain.split_first() else { return false };
        let Ok(verifier) = rustls_platform_verifier::Verifier::new(Arc::new(rustls::crypto::ring::default_provider())) else { return false };
        verifier.verify_server_cert(leaf, intermediates, &name, &[], UnixTime::now()).is_ok()
    })
    .await
    .unwrap_or(false)
}

/// What ties a sign-in to the TLS connection it is made over (RFC 5929's tls-server-end-point): the server's
/// certificate, hashed with its signature's hash, or SHA-256 where that is MD5, SHA-1 or none. A gateway checks it by
/// default (extended protection): a sign-in relayed from another connection doesn't have it.
pub(super) fn end_point_binding(certificate: &[u8]) -> Vec<u8> {
    // Certificate ::= SEQUENCE { tbsCertificate SEQUENCE, signatureAlgorithm SEQUENCE, signature }: the second member.
    fn member(der: &[u8], at: usize) -> Option<(usize, usize)> {
        let mut length = *der.get(at + 1)? as usize;
        let mut start = at + 2;
        if length & 0x80 != 0 {
            let bytes = length & 0x7f;
            if bytes > 4 {
                return None;
            }
            length = der.get(start..start + bytes)?.iter().fold(0, |n, b| n * 256 + *b as usize);
            start += bytes;
        }
        (start + length <= der.len()).then_some((start, start + length))
    }
    let algorithm = (|| {
        let (outer, _) = member(certificate, 0)?;
        let (_, tbs_end) = member(certificate, outer)?;
        let (_, end) = member(certificate, tbs_end)?;
        Some(&certificate[tbs_end..end])
    })()
    .unwrap_or_default();
    let has = |oid: &[u8]| algorithm.windows(oid.len()).any(|w| w == oid);
    // sha384/sha512 with RSA, ECDSA, or as RSASSA-PSS's hash.
    const SHA384: [&[u8]; 3] = [&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c], &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03], &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02]];
    const SHA512: [&[u8]; 3] = [&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d], &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04], &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03]];
    let hash = if SHA384.iter().any(|oid| has(oid)) {
        &ring::digest::SHA384
    } else if SHA512.iter().any(|oid| has(oid)) {
        &ring::digest::SHA512
    } else {
        &ring::digest::SHA256
    };
    [&b"tls-server-end-point:"[..], ring::digest::digest(hash, certificate).as_ref()].concat()
}

// ---- the HTTP upgrade, with NTLM -----------------------------------------------------------------------------------

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Response {
    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers.iter().filter(move |(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// One HTTP response's status and headers; its body (a 401's page) is read and dropped. What follows it stays in
/// `rest` for whoever reads next.
async fn read_response(stream: &mut (impl AsyncRead + Unpin), rest: &mut Vec<u8>) -> Result<Response, String> {
    let mut buf = std::mem::take(rest);
    let (response, mut body) = loop {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut parsed = httparse::Response::new(&mut headers);
        match parsed.parse(&buf) {
            Ok(httparse::Status::Complete(end)) => {
                let status = parsed.code.unwrap_or(0);
                let headers: Vec<(String, String)> = parsed.headers.iter().map(|h| (h.name.to_owned(), String::from_utf8_lossy(h.value).trim().to_owned())).collect();
                let response = Response { status, headers };
                // A switch of protocols has no body; other answers say how long theirs is.
                let length = if status == 101 { 0 } else { response.all("content-length").next().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) };
                if length > 1 << 20 {
                    return Err("the gateway's answer is too large".into());
                }
                break (response, (buf.split_off(end), length));
            }
            Ok(httparse::Status::Partial) if buf.len() < 64 * 1024 => {}
            _ => return Err("the gateway's answer isn't HTTP".into()),
        }
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).await.map_err(|e| format!("the gateway: {e}"))?;
        if n == 0 {
            return Err("the gateway closed the connection".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    while body.0.len() < body.1 {
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).await.map_err(|e| format!("the gateway: {e}"))?;
        if n == 0 {
            return Err("the gateway closed the connection".into());
        }
        body.0.extend_from_slice(&chunk[..n]);
    }
    *rest = body.0.split_off(body.1);
    Ok(response)
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; N];
    ring::rand::SystemRandom::new().fill(&mut bytes).expect("randomness");
    bytes
}

/// Upgrades the connection to the gateway's WebSocket, signing in with NTLM on the way. Returns what the gateway sent
/// past its answer, if anything.
async fn upgrade(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), gateway: &Address, account: &Account, certificate: &[u8], hooks: &dyn Hooks) -> Result<Vec<u8>, Error> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let key = b64.encode(random_bytes::<16>());
    let connection = format!("{{{}}}", uuid::Uuid::new_v4().to_string().to_uppercase());
    let host = if gateway.host.contains(':') { format!("[{}]", gateway.host) } else { gateway.host.clone() };
    let host = if gateway.port == 443 { host } else { format!("{host}:{}", gateway.port) };
    let request = |authorization: &[u8]| {
        format!(
            "RDG_OUT_DATA /remoteDesktopGateway/ HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\nRDG-Connection-Id: {connection}\r\nAuthorization: NTLM {}\r\n\r\n",
            b64.encode(authorization)
        )
    };
    let io = |e: std::io::Error| Error::Failed(format!("the gateway: {e}"));
    let step = |what: &'static str| move |_| Error::Failed(format!("the gateway didn't answer in time ({what})"));
    let negotiate = ntlm::negotiate_message();
    stream.write_all(request(&negotiate).as_bytes()).await.map_err(io)?;
    let mut rest = Vec::new();
    let first = tokio::time::timeout(STEP_TIMEOUT, read_response(stream, &mut rest)).await.map_err(step("sign-in"))??;
    if first.status != 401 {
        return Err(Error::Failed(if first.status == 101 {
            "the gateway let the connection in without a sign-in".into()
        } else {
            format!("the gateway answered {} to the sign-in", first.status)
        }));
    }
    let offered: Vec<&str> = first.all("www-authenticate").collect();
    let challenge = offered.iter().find_map(|v| {
        let (scheme, token) = v.split_once(' ')?;
        (scheme.eq_ignore_ascii_case("NTLM") && !token.trim().is_empty()).then(|| token.trim())
    });
    let Some(challenge) = challenge else {
        return Err(Error::Failed(if offered.iter().any(|v| v.split(' ').next().is_some_and(|s| s.eq_ignore_ascii_case("NTLM"))) {
            "the gateway didn't take up the NTLM sign-in".into()
        } else {
            let schemes: Vec<&str> = offered.iter().filter_map(|v| v.split(' ').next()).collect();
            format!("the gateway doesn't offer an NTLM sign-in (it offers: {})", if schemes.is_empty() { "nothing".into() } else { schemes.join(", ") })
        }));
    };
    let challenge = b64.decode(challenge).map_err(|_| Error::Failed("the gateway's NTLM challenge isn't base64".into()))?;
    let binding = end_point_binding(certificate);
    let target_name = format!("HTTP/{}", gateway.host);
    let authenticate = ntlm::authenticate_message(&negotiate, &challenge, &ntlm::Account {
        username: &account.username,
        domain: account.domain.as_deref().unwrap_or(""),
        password: &account.password,
        workstation: "",
        channel_binding: Some(&binding),
        target_name: &target_name,
    })?;
    stream.write_all(request(&authenticate).as_bytes()).await.map_err(io)?;
    let second = tokio::time::timeout(STEP_TIMEOUT, read_response(stream, &mut rest)).await.map_err(step("sign-in"))??;
    if second.status == 401 || second.status == 403 {
        return Err(Error::Refused("The gateway refused the sign-in.".into()));
    }
    if second.status != 101 {
        // An older gateway takes the request for its HTTP transport (two connections, one each way), not spoken here.
        return Err(Error::Failed(if second.status == 200 {
            "the gateway doesn't take WebSocket connections (Windows Server 2012 R2 and later do)".into()
        } else {
            format!("the gateway answered {} after the sign-in", second.status)
        }));
    }
    let accept = b64.encode(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes()));
    if second.all("sec-websocket-accept").next() != Some(accept.as_str()) {
        return Err(Error::Failed("the gateway's WebSocket answer doesn't match the request".into()));
    }
    hooks.log(format!("gateway: signed in to {}", gateway.host));
    Ok(rest)
}

/// A stream with some bytes already read from it put back in front.
struct Prefixed<T> {
    prefix: Vec<u8>,
    at: usize,
    io: T,
}

impl<T: AsyncRead + Unpin> AsyncRead for Prefixed<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        if self.at < self.prefix.len() {
            let n = buf.remaining().min(self.prefix.len() - self.at);
            let at = self.at;
            buf.put_slice(&self.prefix[at..at + n]);
            self.at += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Prefixed<T> {
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

/// This computer's name, as the gateway's logs will have it.
fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname writes at most that, zero-terminated when it fits.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
            if !name.is_empty() {
                return name;
            }
        }
    }
    #[cfg(windows)]
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        return name;
    }
    "tabbz".into()
}

// ---- the tunnel ----------------------------------------------------------------------------------------------------

type GatewaySocket = tokio_tungstenite::WebSocketStream<Prefixed<tokio_rustls::client::TlsStream<Box<dyn Duplex>>>>;

/// Sends a packet and waits for the answer of the given type: its body. Data, keepalives and the gateway's messages
/// can't come yet; anything else of another type is skipped.
async fn exchange(ws: &mut GatewaySocket, packets: &mut PacketReader, send: Vec<u8>, expect: u16, step: &str, hooks: &dyn Hooks) -> Result<Vec<u8>, Error> {
    ws.send(Message::Binary(send.into())).await.map_err(|e| Error::Failed(format!("the gateway: {e}")))?;
    tokio::time::timeout(STEP_TIMEOUT, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Binary(data))) => {
                    for (kind, body) in packets.feed(&data)? {
                        if kind == expect {
                            return Ok(body);
                        }
                        if kind == PKT_SERVICE_MESSAGE {
                            hooks.log(format!("gateway: its administrator's message: {}", service_message(&body)));
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => return Err(Error::Failed("the gateway closed the connection".into())),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(Error::Failed(format!("the gateway: {e}"))),
            }
        }
    })
    .await
    .map_err(|_| Error::Failed(format!("the gateway didn't answer in time ({step})")))?
}

fn service_message(body: &[u8]) -> String {
    let text = body.get(2..).unwrap_or_default();
    let units: Vec<u16> = text.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').chars().take(300).collect()
}

/// Opens a stream to `target` (as the gateway sees it) through the gateway, over `raw` (a stream to the gateway, direct
/// or through an SSH host). The gateway's certificate is put to `hooks` before the account is used.
pub(super) async fn connect(raw: Box<dyn Duplex>, gateway: &Address, account: &Account, target: &Address, client_name: &str, hooks: &dyn Hooks) -> Result<Box<dyn Duplex>, Error> {
    let target_name = format!("{}:{}", target.host, target.port);
    let named = gateway.host.parse::<std::net::IpAddr>().is_err();
    let server_name = ServerName::try_from(gateway.host.clone()).map_err(|_| Error::Failed(format!("\"{}\" isn't a gateway's name", gateway.host)))?;
    let mut tls = tokio::time::timeout(STEP_TIMEOUT, tls_connector(named)?.connect(server_name, raw))
        .await
        .map_err(|_| Error::Failed("the gateway didn't answer in time (TLS)".into()))?
        .map_err(|e| Error::Failed(format!("the gateway's TLS: {e}")))?;
    let chain: Vec<CertificateDer<'static>> = tls.get_ref().1.peer_certificates().map(|c| c.to_vec()).unwrap_or_default();
    let Some(leaf) = chain.first().cloned() else {
        return Err(Error::Failed("the gateway sent no certificate".into()));
    };
    let valid = valid_for(&gateway.host, &chain).await;
    hooks.check_certificate(fingerprint_of(leaf.as_ref()), valid).await?;
    let rest = upgrade(&mut tls, gateway, account, leaf.as_ref(), hooks).await?;

    // From here on: WebSocket frames, the gateway's packets inside.
    let config = WebSocketConfig::default().max_message_size(Some(MAX_PACKET * 4)).max_frame_size(Some(MAX_PACKET * 4));
    let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(Prefixed { prefix: rest, at: 0, io: tls }, Role::Client, Some(config)).await;
    let mut packets = PacketReader::default();
    let refused = |code: u32, step: &str| {
        hooks.log(format!("gateway: {step} refused: 0x{code:x}"));
        let message = gateway_error(code, &target_name);
        // Not being allowed in by policy is the account's doing, like a refused sign-in.
        if matches!(code & 0xffff, 0x59db | 0x59ed) {
            let mut chars = message.chars();
            Error::Refused(chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str() + ".").unwrap_or_default())
        } else {
            Error::Failed(message)
        }
    };
    let handshake = exchange(&mut ws, &mut packets, handshake_request(), PKT_HANDSHAKE_RESPONSE, "handshake", hooks).await?;
    match u32_at(&handshake, 0) {
        Some(0) => {}
        code => return Err(refused(code.unwrap_or(0), "the handshake")),
    }
    // The tunnel: server version (2), status (4), fields present (2), reserved (2), then what the fields say.
    let tunnel = exchange(&mut ws, &mut packets, tunnel_create(), PKT_TUNNEL_RESPONSE, "tunnel", hooks).await?;
    match (tunnel.len() >= 10).then(|| u32_at(&tunnel, 2)).flatten() {
        Some(0) => {}
        code => return Err(refused(code.unwrap_or(0), "the tunnel")),
    }
    let client = if client_name.trim().is_empty() { host_name() } else { client_name.to_owned() };
    let authorized = exchange(&mut ws, &mut packets, tunnel_auth(&client), PKT_TUNNEL_AUTH_RESPONSE, "authorization", hooks).await?;
    match u32_at(&authorized, 0) {
        Some(0) => {}
        code => return Err(refused(code.unwrap_or(0), "the authorization")),
    }
    let channel = exchange(&mut ws, &mut packets, channel_create(&target.host, target.port), PKT_CHANNEL_RESPONSE, "channel", hooks).await?;
    match u32_at(&channel, 0) {
        Some(0) => {}
        code => return Err(refused(code.unwrap_or(0), "the channel")),
    }
    hooks.log(format!("gateway: channel to {target_name} through {}", gateway.host));

    // The proxy reads and writes one end; a task carries it to and from the gateway's data packets. The pipe's buffer
    // is the most that waits: a reader that stops holds up the gateway connection, not memory.
    let (ours, theirs) = tokio::io::duplex(256 * 1024);
    tokio::spawn(pump(ws, packets, theirs));
    Ok(Box::new(ours))
}

async fn pump(ws: GatewaySocket, mut packets: PacketReader, pipe: tokio::io::DuplexStream) {
    let (mut from_proxy, mut to_proxy) = tokio::io::split(pipe);
    let (mut sink, mut stream) = ws.split();
    let (close_tx, mut close_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let to_gateway = async {
        let mut buf = vec![0u8; MAX_DATA];
        loop {
            tokio::select! {
                read = from_proxy.read(&mut buf) => match read {
                    Ok(0) | Err(_) => {
                        // The connection ends from this side: say so, as a client does.
                        let _ = sink.send(Message::Binary(packet(PKT_CLOSE_CHANNEL, &[0; 4]).into())).await;
                        break;
                    }
                    Ok(n) => if sink.send(Message::Binary(data_packet(&buf[..n]).into())).await.is_err() { break },
                },
                Some(answer) = close_rx.recv() => {
                    // The gateway closed the channel (its answer to send), or the connection to it ended (none).
                    if !answer.is_empty() {
                        let _ = sink.send(Message::Binary(answer.into())).await;
                    }
                    break;
                }
            }
        }
        let _ = sink.close().await;
    };
    let from_gateway = async {
        'read: while let Some(message) = stream.next().await {
            let data = match message {
                Ok(Message::Binary(data)) => data,
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => continue,
            };
            let Ok(found) = packets.feed(&data) else { break };
            for (kind, body) in found {
                match kind {
                    PKT_DATA => {
                        let length = body.get(..2).map_or(0, |l| u16::from_le_bytes([l[0], l[1]]) as usize);
                        if to_proxy.write_all(body.get(2..2 + length).unwrap_or_default()).await.is_err() {
                            break 'read;
                        }
                    }
                    // Nothing to answer: the gateway only checks the line.
                    PKT_KEEPALIVE | PKT_SERVICE_MESSAGE => {}
                    PKT_CLOSE_CHANNEL => {
                        let _ = close_tx.send(packet(PKT_CLOSE_CHANNEL_RESPONSE, &[0; 8]));
                        break 'read;
                    }
                    _ => {}
                }
            }
        }
        // The end of what the desktop sends: the proxy then ends the desktop.
        let _ = to_proxy.shutdown().await;
    };
    tokio::pin!(to_gateway, from_gateway);
    // Whichever side ends first, the other gets a moment to finish (an answer to send, the gateway's last packets).
    tokio::select! {
        _ = &mut to_gateway => {
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut from_gateway).await;
        }
        _ = &mut from_gateway => {
            let _ = close_tx.send(Vec::new());
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut to_gateway).await;
        }
    }
}

#[cfg(test)]
mod tests;
