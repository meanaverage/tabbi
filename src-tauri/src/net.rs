//! TCP (and Unix domain) sockets for the page's `net` module (page/src/node/net.ts): tabby-ssh's port
//! forwards and X11, and telnet.
//!
//! A socket's input goes to the page as raw frames on a Tauri channel, `[0][bytes]` for data and `[1][JSON]` for
//! the end of input or an error, so the end comes after the last data. Reading stops while more than MAX_UNACKED
//! bytes are unacknowledged by the page, which holds the peer up instead of piling its data up in memory. Errors carry
//! Node's codes (`ECONNREFUSED`…) for code that checks `error.code`.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::ipc::{Channel, InvokeBody, InvokeResponseBody, Request};
use tauri::{Manager, State};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tauri::async_runtime::JoinHandle;

const READ_CHUNK: usize = 64 * 1024;
const MAX_UNACKED: usize = 1024 * 1024;

#[derive(Serialize)]
pub struct NetError {
    code: &'static str,
    message: String,
}

fn node_code(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    #[cfg(unix)]
    if let Some(errno) = e.raw_os_error() {
        match errno {
            libc::ECONNREFUSED => return "ECONNREFUSED",
            libc::ECONNRESET => return "ECONNRESET",
            libc::ETIMEDOUT => return "ETIMEDOUT",
            libc::EHOSTUNREACH => return "EHOSTUNREACH",
            libc::ENETUNREACH => return "ENETUNREACH",
            libc::EADDRINUSE => return "EADDRINUSE",
            libc::EADDRNOTAVAIL => return "EADDRNOTAVAIL",
            libc::EACCES => return "EACCES",
            libc::EPIPE => return "EPIPE",
            libc::ENOENT => return "ENOENT",
            _ => {}
        }
    }
    match e.kind() {
        ConnectionRefused => "ECONNREFUSED",
        ConnectionReset => "ECONNRESET",
        ConnectionAborted => "ECONNABORTED",
        TimedOut => "ETIMEDOUT",
        AddrInUse => "EADDRINUSE",
        AddrNotAvailable => "EADDRNOTAVAIL",
        PermissionDenied => "EACCES",
        BrokenPipe => "EPIPE",
        NotFound => "ENOENT",
        HostUnreachable => "EHOSTUNREACH",
        NetworkUnreachable => "ENETUNREACH",
        // What getaddrinfo's failures come as.
        _ if e.to_string().contains("lookup") => "ENOTFOUND",
        _ => "EIO",
    }
}

/// As Node words it: `connect ECONNREFUSED 127.0.0.1:22`.
fn net_error(e: std::io::Error, syscall: &str, target: &str) -> NetError {
    let code = node_code(&e);
    NetError { code, message: format!("{syscall} {code} {target}") }
}

enum Writer {
    Tcp(tokio::net::tcp::OwnedWriteHalf),
    #[cfg(unix)]
    Unix(tokio::net::unix::OwnedWriteHalf),
}

impl Writer {
    async fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Writer::Tcp(w) => w.write_all(bytes).await,
            #[cfg(unix)]
            Writer::Unix(w) => w.write_all(bytes).await,
        }
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        match self {
            Writer::Tcp(w) => w.shutdown().await,
            #[cfg(unix)]
            Writer::Unix(w) => w.shutdown().await,
        }
    }
}

type Reader = Box<dyn AsyncRead + Send + Unpin>;

struct Socket {
    owner: String,
    closer: socket2::Socket,
    closed: tokio::sync::watch::Sender<bool>,
    writer: tokio::sync::Mutex<Option<Writer>>,
    /// Until the page attaches (accepted connections): then the reader task has it.
    reader: Mutex<Option<Reader>>,
    task: Mutex<Option<JoinHandle<()>>>,
    unacked: Mutex<usize>,
    room: Notify,
}

impl Socket {
    /// Shutdown does not wait for the writer lock, which may be held by a blocked write.
    fn stop(&self) {
        self.closed.send_replace(true);
        let _ = self.closer.shutdown(std::net::Shutdown::Both);
        self.reader.lock().unwrap().take();
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }

    async fn write(&self, bytes: &[u8]) -> Result<(), NetError> {
        let mut closed = self.closed.subscribe();
        tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => Err(NetError { code: "ERR_SOCKET_CLOSED", message: "Socket is closed".into() }),
            result = async {
                let mut writer = self.writer.lock().await;
                match writer.as_mut() {
                    Some(w) => w.write_all(bytes).await.map_err(|e| net_error(e, "write", "")),
                    None => Err(NetError { code: "EPIPE", message: "This socket has been ended by the other party".into() }),
                }
            } => result,
        }
    }

}

struct Server {
    owner: String,
    task: JoinHandle<()>,
}

#[derive(Default)]
pub struct Net {
    next: AtomicU32,
    sockets: Mutex<HashMap<u32, Arc<Socket>>>,
    servers: Mutex<HashMap<u32, Server>>,
}

impl Net {
    fn socket(&self, id: u32) -> Result<Arc<Socket>, NetError> {
        self.sockets
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| NetError { code: "ERR_SOCKET_CLOSED", message: "Socket is closed".into() })
    }

    fn add_socket(&self, socket: Socket) -> u32 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.sockets.lock().unwrap().insert(id, Arc::new(socket));
        id
    }
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
enum SocketEvent {
    End,
    Error { code: &'static str, message: String },
}

fn event_frame(event: &SocketEvent) -> Vec<u8> {
    let mut out = vec![1];
    out.extend_from_slice(&serde_json::to_vec(event).unwrap_or_default());
    out
}

fn start_reading(socket: &Arc<Socket>, data: Channel<InvokeResponseBody>) {
    let mut reader_task = socket.task.lock().unwrap();
    if *socket.closed.borrow() { return; }
    let Some(mut reader) = socket.reader.lock().unwrap().take() else { return };
    let socket_ = socket.clone();
    let task = tauri::async_runtime::spawn(async move {
        let socket = socket_;
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            // Waits while the page is behind.
            loop {
                let notified = socket.room.notified();
                if *socket.unacked.lock().unwrap() < MAX_UNACKED {
                    break;
                }
                notified.await;
            }
            let frame = match reader.read(&mut buf).await {
                Ok(0) => event_frame(&SocketEvent::End),
                Ok(n) => {
                    *socket.unacked.lock().unwrap() += n;
                    let mut frame = Vec::with_capacity(n + 1);
                    frame.push(0);
                    frame.extend_from_slice(&buf[..n]);
                    if data.send(InvokeResponseBody::Raw(frame)).is_err() {
                        return;
                    }
                    continue;
                }
                Err(e) => {
                    let code = node_code(&e);
                    event_frame(&SocketEvent::Error { code, message: format!("read {code}") })
                }
            };
            let _ = data.send(InvokeResponseBody::Raw(frame));
            return;
        }
    });
    *reader_task = Some(task);
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketInfo {
    id: u32,
    local_address: Option<String>,
    local_port: Option<u16>,
    remote_address: Option<String>,
    remote_port: Option<u16>,
    remote_family: Option<&'static str>,
}

fn family(addr: &SocketAddr) -> &'static str {
    if addr.is_ipv4() {
        "IPv4"
    } else {
        "IPv6"
    }
}

fn tcp_socket(owner: &str, stream: TcpStream) -> std::io::Result<(Socket, SocketInfo)> {
    let closer = socket2::SockRef::from(&stream).try_clone()?;
    let _ = stream.set_nodelay(true);
    let local = stream.local_addr().ok();
    let remote = stream.peer_addr().ok();
    let (r, w) = stream.into_split();
    let socket = Socket {
        owner: owner.to_string(),
        closer,
        closed: tokio::sync::watch::channel(false).0,
        writer: tokio::sync::Mutex::new(Some(Writer::Tcp(w))),
        reader: Mutex::new(Some(Box::new(r))),
        task: Mutex::new(None),
        unacked: Mutex::new(0),
        room: Notify::new(),
    };
    let info = SocketInfo {
        id: 0,
        local_address: local.map(|a| a.ip().to_string()),
        local_port: local.map(|a| a.port()),
        remote_address: remote.map(|a| a.ip().to_string()),
        remote_port: remote.map(|a| a.port()),
        remote_family: remote.as_ref().map(family),
    };
    Ok((socket, info))
}

/// Connects to `host`:`port`, or the Unix socket at `path`; input goes to `data` from then on.
#[tauri::command]
pub async fn net_connect(
    net: State<'_, Net>,
    webview: tauri::Webview,
    host: Option<String>,
    port: Option<u16>,
    path: Option<String>,
    data: Channel<InvokeResponseBody>,
) -> Result<SocketInfo, NetError> {
    let (socket, mut info) = if let Some(path) = path {
        #[cfg(unix)]
        {
            let stream = tokio::net::UnixStream::connect(&path).await.map_err(|e| net_error(e, "connect", &path))?;
            let closer = socket2::SockRef::from(&stream).try_clone().map_err(|e| net_error(e, "connect", &path))?;
            let (r, w) = stream.into_split();
            let socket = Socket {
                owner: webview.label().to_string(),
                closer,
                closed: tokio::sync::watch::channel(false).0,
                writer: tokio::sync::Mutex::new(Some(Writer::Unix(w))),
                reader: Mutex::new(Some(Box::new(r))),
                task: Mutex::new(None),
                unacked: Mutex::new(0),
                room: Notify::new(),
            };
            let info = SocketInfo { id: 0, local_address: None, local_port: None, remote_address: None, remote_port: None, remote_family: None };
            (socket, info)
        }
        #[cfg(not(unix))]
        return Err(NetError { code: "ENOTSUP", message: format!("connect ENOTSUP {path}") });
    } else {
        let host = host.unwrap_or_else(|| "localhost".into());
        let port = port.unwrap_or(0);
        let target = format!("{host}:{port}");
        let stream = TcpStream::connect((host.as_str(), port)).await.map_err(|e| net_error(e, "connect", &target))?;
        tcp_socket(webview.label(), stream).map_err(|e| net_error(e, "connect", &target))?
    };
    info.id = net.add_socket(socket);
    start_reading(&net.socket(info.id)?, data);
    Ok(info)
}

/// Starts an accepted connection's input (after the page has made its Socket).
#[tauri::command]
pub fn net_attach(net: State<'_, Net>, id: u32, data: Channel<InvokeResponseBody>) -> Result<(), NetError> {
    start_reading(&net.socket(id)?, data);
    Ok(())
}

/// The raw request body is the bytes, the `net-id` header says which socket.
#[tauri::command]
pub async fn net_write(net: State<'_, Net>, request: Request<'_>) -> Result<(), NetError> {
    let id = request.headers().get("net-id").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
    let socket = net.socket(id)?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err(NetError { code: "EINVAL", message: "expected raw bytes".into() });
    };
    socket.write(bytes).await
}

#[tauri::command]
pub fn net_ack(net: State<'_, Net>, id: u32, length: usize) {
    if let Ok(socket) = net.socket(id) {
        let mut unacked = socket.unacked.lock().unwrap();
        *unacked = unacked.saturating_sub(length);
        socket.room.notify_waiters();
    }
}

/// Ends our side (half-close); input keeps coming until the peer ends too.
#[tauri::command]
pub async fn net_end(net: State<'_, Net>, id: u32) -> Result<(), NetError> {
    let socket = net.socket(id)?;
    let mut writer = socket.writer.lock().await;
    if let Some(mut w) = writer.take() {
        let _ = w.shutdown().await;
    }
    Ok(())
}

#[tauri::command]
pub fn net_destroy(net: State<'_, Net>, id: u32) {
    let socket = net.sockets.lock().unwrap().remove(&id);
    if let Some(socket) = socket {
        socket.stop();
    }
}

#[tauri::command]
pub async fn net_set_no_delay(net: State<'_, Net>, id: u32, no_delay: bool) -> Result<(), NetError> {
    let socket = net.socket(id)?;
    if let Some(Writer::Tcp(w)) = socket.writer.lock().await.as_ref() {
        let _ = w.as_ref().set_nodelay(no_delay);
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum ServerEvent {
    Connection { socket: SocketInfo },
    Error { code: &'static str, message: String },
}

#[derive(Serialize)]
pub struct ServerInfo {
    id: u32,
    address: String,
    port: u16,
    family: &'static str,
}

/// Listens on `host`:`port` (port 0: any free one); connections come as events, each with a socket to attach.
/// Without a host it listens on loopback only (Node would pick every interface): nothing here authenticates
/// the connecting side, so reaching the LAN has to be asked for with an explicit address.
#[tauri::command]
pub async fn net_listen(
    net: State<'_, Net>,
    webview: tauri::Webview,
    host: Option<String>,
    port: u16,
    events: Channel<ServerEvent>,
) -> Result<ServerInfo, NetError> {
    let host = listen_host(host);
    let target = format!("{host}:{port}");
    let listener = TcpListener::bind((host.as_str(), port)).await.map_err(|e| net_error(e, "listen", &target))?;
    let local = listener.local_addr().map_err(|e| net_error(e, "listen", &target))?;
    let id = net.next.fetch_add(1, Ordering::Relaxed) + 1;
    let owner = webview.label().to_string();
    let app = webview.app_handle().clone();
    let task = tauri::async_runtime::spawn({
        let owner = owner.clone();
        async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let Ok((socket, mut info)) = tcp_socket(&owner, stream) else { continue };
                        info.id = app.state::<Net>().add_socket(socket);
                        if events.send(ServerEvent::Connection { socket: info }).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let code = node_code(&e);
                        let _ = events.send(ServerEvent::Error { code, message: format!("accept {code}") });
                    }
                }
            }
        }
    });
    net.servers.lock().unwrap().insert(id, Server { owner, task });
    Ok(ServerInfo { id, address: local.ip().to_string(), port: local.port(), family: family(&local) })
}

fn listen_host(host: Option<String>) -> String {
    host.filter(|host| !host.is_empty()).unwrap_or_else(|| "127.0.0.1".into())
}

/// Stops listening (connections already accepted stay).
#[tauri::command]
pub fn net_close_server(net: State<'_, Net>, id: u32) {
    if let Some(server) = net.servers.lock().unwrap().remove(&id) {
        server.task.abort();
    }
}

/// A (re)loaded page starts with nothing: what the page before it had is closed.
#[tauri::command]
pub fn net_reset(net: State<'_, Net>, webview: tauri::Webview) {
    let label = webview.label();
    let servers: Vec<Server> = {
        let mut servers = net.servers.lock().unwrap();
        let ids: Vec<u32> = servers.iter().filter(|(_, s)| s.owner == label).map(|(id, _)| *id).collect();
        ids.into_iter().filter_map(|id| servers.remove(&id)).collect()
    };
    for server in servers {
        server.task.abort();
    }
    let sockets: Vec<Arc<Socket>> = {
        let mut sockets = net.sockets.lock().unwrap();
        let ids: Vec<u32> = sockets.iter().filter(|(_, s)| s.owner == label).map(|(id, _)| *id).collect();
        ids.into_iter().filter_map(|id| sockets.remove(&id)).collect()
    };
    for socket in sockets {
        socket.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn listeners_default_to_loopback_and_keep_explicit_hosts() {
        assert_eq!(listen_host(None), "127.0.0.1");
        assert_eq!(listen_host(Some(String::new())), "127.0.0.1");
        assert_eq!(listen_host(Some("0.0.0.0".into())), "0.0.0.0");
        assert_eq!(listen_host(Some("::1".into())), "::1");
    }

    #[tokio::test]
    async fn destroy_cancels_blocked_and_queued_writes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        socket2::SockRef::from(&stream).set_send_buffer_size(4096).unwrap();
        let (socket, _) = tcp_socket("test", stream).unwrap();
        let socket = Arc::new(socket);
        let weak = Arc::downgrade(&socket);
        let write = tokio::spawn({
            let socket = socket.clone();
            async move { socket.write(&vec![0; 8 * 1024 * 1024]).await.is_err() }
        });
        let mut byte = [0];
        peer.read_exact(&mut byte).await.unwrap();
        let queued = tokio::spawn({
            let socket = socket.clone();
            async move { socket.write(b"queued").await.is_err() }
        });
        assert!(!write.is_finished());
        socket.stop();
        assert!(tokio::time::timeout(Duration::from_secs(2), write).await.unwrap().unwrap());
        assert!(tokio::time::timeout(Duration::from_secs(2), queued).await.unwrap().unwrap());
        // The peer observes EOF while we still hold Socket, independently of dropping the writer.
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), peer.read_to_end(&mut rest)).await.unwrap().unwrap();
        assert!(rest.len() < 8 * 1024 * 1024);
        drop(socket);
        assert!(weak.upgrade().is_none());
    }
}
