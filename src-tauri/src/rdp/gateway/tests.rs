// The RD Gateway client (gateway.rs) against a gateway made up here: TLS with a self-signed certificate, the NTLM
// sign-in (whose proof and channel binding it checks), the WebSocket upgrade and the tunnel's packets. Ported from the
// ideas of tabby-rdp's test/unit/gateway.ts.
use super::*;
use std::sync::Mutex as StdMutex;
use rustls::pki_types::PrivateKeyDer;
use tokio::net::{TcpListener, TcpStream};

const USER: &str = "tester";
const DOMAIN: &str = "LAB";
const PASSWORD: &str = "right password";
const CHALLENGE: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

/// What the made-up gateway does at each step.
#[derive(Clone, Default)]
struct Script {
    /// Error code for the tunnel authorization (0: allowed).
    authorization: u32,
    /// Error code for the channel (0: opened).
    channel: u32,
    /// Closes the channel itself after echoing this many data packets.
    close_after: Option<usize>,
}

struct Seen {
    requests: usize,
    channel: Option<(String, u16)>,
    client_name: Option<String>,
}

fn utf16_text(b: &[u8]) -> String { String::from_utf16_lossy(&b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()) }

fn challenge_message() -> Vec<u8> {
    let mut info = Vec::new();
    for (id, value) in [(2u16, "LAB".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>()), (1, "GW".encode_utf16().flat_map(u16::to_le_bytes).collect())] {
        info.extend_from_slice(&id.to_le_bytes());
        info.extend_from_slice(&(value.len() as u16).to_le_bytes());
        info.extend_from_slice(&value);
    }
    info.extend_from_slice(&[7, 0, 8, 0]);
    info.extend_from_slice(&[0, 0x90, 0xd3, 0x36, 0xb7, 0x34, 0xc3, 0x01]);
    info.extend_from_slice(&[0; 4]);
    let mut message = vec![0u8; 56];
    message[..8].copy_from_slice(b"NTLMSSP\0");
    message[8..12].copy_from_slice(&2u32.to_le_bytes());
    message[20..24].copy_from_slice(&0xe28a_8215u32.to_le_bytes());
    message[24..32].copy_from_slice(&CHALLENGE);
    message[40..42].copy_from_slice(&(info.len() as u16).to_le_bytes());
    message[42..44].copy_from_slice(&(info.len() as u16).to_le_bytes());
    message[44..48].copy_from_slice(&56u32.to_le_bytes());
    message.extend_from_slice(&info);
    message
}

/// Checks an authenticate message: the proof for the right password, and the channel binding of this certificate.
fn signed_in(message: &[u8], certificate: &[u8]) -> bool {
    let field = |i: usize| {
        let length = u16::from_le_bytes([message[12 + i * 8], message[13 + i * 8]]) as usize;
        let at = u32::from_le_bytes(message[16 + i * 8..20 + i * 8].try_into().unwrap()) as usize;
        &message[at..at + length]
    };
    let (nt, domain, user) = (field(1), utf16_text(field(2)), utf16_text(field(3)));
    let hash = ntlm::ntlm_v2_hash(&user, &domain, PASSWORD);
    if ntlm::hmac_md5(&hash, &[&CHALLENGE, &nt[16..]]) != nt[..16] {
        return false;
    }
    // The blob: 28 bytes, then the target information's pairs.
    let info = &nt[16 + 28..];
    let mut at = 0;
    while at + 4 <= info.len() {
        let (id, length) = (u16::from_le_bytes([info[at], info[at + 1]]), u16::from_le_bytes([info[at + 2], info[at + 3]]) as usize);
        if id == 10 {
            return info[at + 4..at + 4 + length] == ntlm::channel_binding_hash(&end_point_binding(certificate));
        }
        at += 4 + length;
    }
    false
}

async fn read_request(tls: &mut (impl AsyncRead + Unpin)) -> Option<(String, Vec<(String, String)>)> {
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let n = tls.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut request = httparse::Request::new(&mut headers);
        if let Ok(httparse::Status::Complete(_)) = request.parse(&buf) {
            let headers = request.headers.iter().map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).into_owned())).collect();
            return Some((request.method.unwrap_or("").to_owned(), headers));
        }
    }
}

async fn gateway(script: Script) -> (u16, Vec<u8>, Arc<StdMutex<Seen>>) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["gateway.test".into()]).unwrap().self_signed(&key).unwrap();
    let der = cert.der().to_vec();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], PrivateKeyDer::Pkcs8(key.serialize_der().into()))
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(StdMutex::new(Seen { requests: 0, channel: None, client_name: None }));
    let (certificate, record) = (der.clone(), seen.clone());
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { break };
            let (acceptor, certificate, record, script) = (acceptor.clone(), certificate.clone(), record.clone(), script.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(socket).await else { return };
                let b64 = base64::engine::general_purpose::STANDARD;
                let Some((method, _)) = read_request(&mut tls).await else { return };
                assert_eq!(method, "RDG_OUT_DATA");
                record.lock().unwrap().requests += 1;
                let body = "<html>sign in</html>";
                let first = format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Negotiate\r\nWWW-Authenticate: NTLM {}\r\nContent-Length: {}\r\n\r\n{body}", b64.encode(challenge_message()), body.len());
                tls.write_all(first.as_bytes()).await.unwrap();
                let Some((_, headers)) = read_request(&mut tls).await else { return };
                record.lock().unwrap().requests += 1;
                let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()).unwrap_or_default();
                let authenticate = b64.decode(header("authorization").trim_start_matches("NTLM ")).unwrap();
                if !signed_in(&authenticate, &certificate) {
                    tls.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n").await.unwrap();
                    return;
                }
                let accept = b64.encode(ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, format!("{}258EAFA5-E914-47DA-95CA-C5AB0DC85B11", header("sec-websocket-key")).as_bytes()));
                tls.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
                let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(tls, Role::Server, None).await;
                let mut packets = PacketReader::default();
                let mut echoed = 0;
                while let Some(Ok(message)) = ws.next().await {
                    let Message::Binary(data) = message else { continue };
                    for (kind, body) in packets.feed(&data).unwrap() {
                        let answer = match kind {
                            PKT_HANDSHAKE_REQUEST => packet(PKT_HANDSHAKE_RESPONSE, &[0, 0, 0, 0, 1, 0, 0, 0, 0, 0]),
                            PKT_TUNNEL_CREATE => packet(PKT_TUNNEL_RESPONSE, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                            PKT_TUNNEL_AUTH => {
                                let length = u16::from_le_bytes([body[2], body[3]]) as usize;
                                record.lock().unwrap().client_name = Some(utf16_text(&body[4..4 + length]).trim_end_matches('\0').to_owned());
                                packet(PKT_TUNNEL_AUTH_RESPONSE, &[&script.authorization.to_le_bytes()[..], &[0; 8]].concat())
                            }
                            PKT_CHANNEL_CREATE => {
                                let port = u16::from_le_bytes([body[2], body[3]]);
                                let length = u16::from_le_bytes([body[6], body[7]]) as usize;
                                record.lock().unwrap().channel = Some((utf16_text(&body[8..8 + length]).trim_end_matches('\0').to_owned(), port));
                                packet(PKT_CHANNEL_RESPONSE, &[&script.channel.to_le_bytes()[..], &[0; 8]].concat())
                            }
                            PKT_DATA => {
                                // An echo, upper-cased, with a keepalive and a message from the administrator in between.
                                let payload: Vec<u8> = body[2..].iter().map(u8::to_ascii_uppercase).collect();
                                let _ = ws.send(Message::Binary(packet(PKT_KEEPALIVE, &[]).into())).await;
                                let mut both = data_packet(&payload);
                                both.extend(packet(PKT_SERVICE_MESSAGE, &[&[0u8, 0][..], &"hi".encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>()].concat()));
                                echoed += 1;
                                if script.close_after == Some(echoed) {
                                    both.extend(packet(PKT_CLOSE_CHANNEL, &[0; 4]));
                                }
                                both
                            }
                            PKT_CLOSE_CHANNEL => {
                                let _ = ws.send(Message::Binary(packet(PKT_CLOSE_CHANNEL_RESPONSE, &[0; 8]).into())).await;
                                let _ = ws.close(None).await;
                                return;
                            }
                            _ => continue,
                        };
                        // Split across two frames: packets are a stream of their own.
                        let (a, b) = answer.split_at(answer.len() / 2);
                        let _ = ws.send(Message::Binary(a.to_vec().into())).await;
                        let _ = ws.send(Message::Binary(b.to_vec().into())).await;
                    }
                }
            });
        }
    });
    (port, der, seen)
}

#[derive(Default)]
struct TestHooks {
    refuse: Option<String>,
    checked: StdMutex<Vec<(String, bool)>>,
    logs: StdMutex<Vec<String>>,
}

impl Hooks for TestHooks {
    fn log(&self, message: String) { self.logs.lock().unwrap().push(message); }
    fn check_certificate(&self, fingerprint: String, valid: bool) -> Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        self.checked.lock().unwrap().push((fingerprint, valid));
        let refuse = self.refuse.clone();
        Box::pin(async move { refuse.map_or(Ok(()), Err) })
    }
}

async fn through(port: u16, password: &str, hooks: &TestHooks) -> Result<Box<dyn Duplex>, Error> {
    let raw: Box<dyn Duplex> = Box::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
    let gateway = Address { host: "localhost".into(), port };
    let account = Account { username: format!("{DOMAIN}\\{USER}"), domain: None, password: password.into() };
    let target = Address { host: "desktop.lab".into(), port: 3389 };
    connect(raw, &gateway, &account, &target, "here", hooks).await
}

#[tokio::test]
async fn signs_in_bound_to_the_certificate_and_tunnels_both_ways() {
    let (port, der, seen) = gateway(Script::default()).await;
    let hooks = TestHooks::default();
    let mut stream = through(port, PASSWORD, &hooks).await.unwrap();
    let checked = hooks.checked.lock().unwrap().clone();
    assert_eq!(checked, [(fingerprint_of(&der), false)]); // self-signed: not valid for its name, so remembered
    assert_eq!(seen.lock().unwrap().channel, Some(("desktop.lab".into(), 3389)));
    assert_eq!(seen.lock().unwrap().client_name.as_deref(), Some("here"));
    for word in [&b"hello"[..], &vec![b'x'; 100_000]] {
        stream.write_all(word).await.unwrap();
        let mut back = vec![0u8; word.len()];
        stream.read_exact(&mut back).await.unwrap();
        assert_eq!(back, word.to_ascii_uppercase());
    }
    drop(stream);
    let logs = hooks.logs.lock().unwrap().clone();
    assert!(logs.iter().any(|m| m == "gateway: signed in to localhost"), "{logs:?}");
}

#[tokio::test]
async fn a_wrong_password_is_the_gateways_refusal() {
    let (port, _, _) = gateway(Script::default()).await;
    match through(port, "wrong", &TestHooks::default()).await {
        Err(Error::Refused(message)) => assert_eq!(message, "The gateway refused the sign-in."),
        other => panic!("{:?}", other.err()),
    }
}

#[tokio::test]
async fn a_refused_certificate_stops_before_the_sign_in() {
    let (port, _, seen) = gateway(Script::default()).await;
    let hooks = TestHooks { refuse: Some("the gateway's certificate changed".into()), ..Default::default() };
    match through(port, PASSWORD, &hooks).await {
        Err(Error::Failed(message)) => assert_eq!(message, "the gateway's certificate changed"),
        other => panic!("{:?}", other.err()),
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(seen.lock().unwrap().requests, 0);
}

#[tokio::test]
async fn policy_refusals_say_which() {
    let (port, _, _) = gateway(Script { authorization: 0x8007_59db, ..Default::default() }).await;
    match through(port, PASSWORD, &TestHooks::default()).await {
        Err(Error::Refused(message)) => assert_eq!(message, "The gateway doesn't let this account connect (its connection authorization policy)."),
        other => panic!("{:?}", other.err()),
    }
    let (port, _, _) = gateway(Script { channel: 0x8007_59dd, ..Default::default() }).await;
    match through(port, PASSWORD, &TestHooks::default()).await {
        Err(Error::Failed(message)) => assert_eq!(message, "the gateway couldn't reach desktop.lab:3389"),
        other => panic!("{:?}", other.err()),
    }
}

#[tokio::test]
async fn the_gateway_closing_the_channel_ends_the_stream() {
    let (port, _, _) = gateway(Script { close_after: Some(1), ..Default::default() }).await;
    let mut stream = through(port, PASSWORD, &TestHooks::default()).await.unwrap();
    stream.write_all(b"bye").await.unwrap();
    let mut back = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut back)).await.unwrap().unwrap();
    assert_eq!(back, b"BYE");
}

#[test]
fn packets_and_names() {
    assert_eq!(name("ab"), [6, 0, b'a', 0, b'b', 0, 0, 0]);
    assert_eq!(data_packet(b"xy"), [0x0a, 0, 0, 0, 12, 0, 0, 0, 2, 0, b'x', b'y']);
    let mut reader = PacketReader::default();
    let both = [packet(PKT_KEEPALIVE, &[]), data_packet(b"z")].concat();
    assert!(reader.feed(&both[..5]).unwrap().is_empty());
    let found = reader.feed(&both[5..]).unwrap();
    assert_eq!(found.iter().map(|(k, _)| *k).collect::<Vec<_>>(), [PKT_KEEPALIVE, PKT_DATA]);
    assert!(PacketReader::default().feed(&[1, 0, 0, 0, 4, 0, 0, 0]).is_err()); // shorter than its header
    assert!(PacketReader::default().feed(&[1, 0, 0, 0, 0, 0, 0x10, 0]).is_err()); // larger than any packet
    assert_eq!(gateway_error(0x8007_59da, "d:1"), "the gateway doesn't let this account connect to d:1 (its resource authorization policy)");
    assert_eq!(gateway_error(0x1234, "d:1"), "the gateway refused the connection (0x1234)");
}

#[test]
fn the_end_point_binding_follows_the_signature_hash() {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let p384 = rcgen::CertificateParams::new(vec!["g".into()]).unwrap().self_signed(&key).unwrap();
    let binding = end_point_binding(p384.der());
    assert_eq!(&binding[..21], b"tls-server-end-point:");
    assert_eq!(&binding[21..], ring::digest::digest(&ring::digest::SHA384, p384.der()).as_ref());
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let p256 = rcgen::CertificateParams::new(vec!["g".into()]).unwrap().self_signed(&key).unwrap();
    assert_eq!(&end_point_binding(p256.der())[21..], ring::digest::digest(&ring::digest::SHA256, p256.der()).as_ref());
    // Not a certificate: SHA-256 of whatever it is.
    assert_eq!(end_point_binding(b"junk").len(), 21 + 32);
}
