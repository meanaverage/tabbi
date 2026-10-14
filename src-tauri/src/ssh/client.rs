// Derived from russh-napi (Apache-2.0, see LICENSE-2.0.txt).
use std::borrow::Cow;
use std::convert::TryFrom;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use log::debug;
use russh::client::{AuthResult, DisconnectReason};
use russh::keys::agent::AgentIdentity;
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::{ChannelId, MethodSet};
use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Manager, State};
use tokio::sync::{oneshot, Mutex};

use super::agent::{get_agent_client, AgentConnection};
use super::channel::NewSshChannel;
use super::error::{wrap, Result, WrappedError};
use super::key::{hash_algorithm, public_key_from_page, PublicKeyInfo};
use super::outbox::{Event, Outbox};
use super::{Object, Ssh};

pub struct SSHClientHandler {
    app: AppHandle,
    /// Whose it is: channels the server opens get handles too.
    owner: String,
    outbox: Arc<Outbox>,
}

impl SSHClientHandler {
    fn new_channel(&self, channel: russh::Channel<russh::client::Msg>) -> u32 {
        let new_channel = NewSshChannel::new(channel, self.outbox.clone());
        self.app.state::<Ssh>().add(&self.owner, Object::NewChannel(Arc::new(new_channel)))
    }
}

impl russh::client::Handler for SSHClientHandler {
    type Error = WrappedError;

    /// Asks the page (tabby-ssh's known hosts check, which may prompt the user) and waits for its answer.
    async fn check_server_key(&mut self, server_public_key: &russh::keys::PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        // todo handle certs
        let ssh = self.app.state::<Ssh>();
        let id = ssh.reserve();
        let (tx, rx) = oneshot::channel();
        ssh.answers.lock().unwrap().insert(id, tx);
        self.outbox.event(Event::ServerKey { id, key: PublicKeyInfo::from(&server_public_key.public_key()) });
        Ok(rx.await.unwrap_or(false))
    }

    async fn channel_open_confirmation(&mut self, channel: ChannelId, _: u32, _: u32, _: &mut russh::client::Session) -> std::result::Result<(), Self::Error> {
        self.outbox.channel_opened(channel.into());
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], _session: &mut russh::client::Session) -> std::result::Result<(), Self::Error> {
        self.outbox.data(channel.into(), None, data).await;
        Ok(())
    }

    async fn extended_data(
        &mut self,
        channel: ChannelId,
        ext: u32,
        data: &[u8],
        _session: &mut russh::client::Session,
    ) -> std::result::Result<(), Self::Error> {
        self.outbox.data(channel.into(), Some(ext), data).await;
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, _session: &mut russh::client::Session) -> std::result::Result<(), Self::Error> {
        self.outbox.event(Event::Eof { channel: channel.into() });
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut russh::client::Session) -> std::result::Result<(), Self::Error> {
        self.outbox.event(Event::Close { channel: channel.into() });
        Ok(())
    }

    async fn disconnected(&mut self, reason: DisconnectReason<Self::Error>) -> std::result::Result<(), Self::Error> {
        self.outbox.event(Event::Disconnect {
            error: match reason {
                DisconnectReason::Error(e) => Some(String::from(e)),
                DisconnectReason::ReceivedDisconnect(_) => None,
            },
        });
        self.outbox.close();
        Ok(())
    }

    async fn server_channel_open_x11(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        originator_address: &str,
        originator_port: u32,
        reply: russh::ChannelOpenHandleInner<russh::client::Msg>,
        _session: &mut russh::client::Session,
    ) -> std::result::Result<(), Self::Error> {
        if !self.outbox.x11_allowed() {
            debug!("Rejecting unsolicited X11 channel: X11 forwarding was not requested");
            reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
            return Ok(());
        }
        reply.accept().await;
        let channel = self.new_channel(channel);
        self.outbox.event(Event::X11Open { channel, originator_address: originator_address.into(), originator_port });
        Ok(())
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        reply: russh::ChannelOpenHandleInner<russh::client::Msg>,
        _session: &mut russh::client::Session,
    ) -> std::result::Result<(), Self::Error> {
        reply.accept().await;
        let channel = self.new_channel(channel);
        self.outbox.event(Event::TcpipOpen {
            channel,
            connected_address: connected_address.into(),
            connected_port,
            originator_address: originator_address.into(),
            originator_port,
        });
        Ok(())
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        reply: russh::ChannelOpenHandleInner<russh::client::Msg>,
        _session: &mut russh::client::Session,
    ) -> std::result::Result<(), Self::Error> {
        reply.accept().await;
        let channel = self.new_channel(channel);
        self.outbox.event(Event::AgentOpen { channel });
        Ok(())
    }

    async fn auth_banner(&mut self, banner: &str, _session: &mut russh::client::Session) -> std::result::Result<(), Self::Error> {
        self.outbox.event(Event::Banner { banner: banner.into() });
        Ok(())
    }
}

/// The page's answer to a server key check (`ServerKey` event).
#[tauri::command]
pub fn ssh_answer_server_key(ssh: State<'_, Ssh>, id: u32, accept: bool) {
    if let Some(tx) = ssh.answers.lock().unwrap().remove(&id) {
        let _ = tx.send(accept);
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct KeyboardInteractiveAuthenticationPrompt {
    pub prompt: String,
    pub echo: bool,
}

impl From<russh::client::Prompt> for KeyboardInteractiveAuthenticationPrompt {
    fn from(p: russh::client::Prompt) -> Self {
        KeyboardInteractiveAuthenticationPrompt { prompt: p.prompt, echo: p.echo }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyboardInteractiveAuthenticationState {
    pub state: String,
    pub partial_success: bool,
    pub name: Option<String>,
    pub instructions: Option<String>,
    pub remaining_methods: Vec<String>,
    prompts: Option<Vec<KeyboardInteractiveAuthenticationPrompt>>,
}

impl From<russh::client::KeyboardInteractiveAuthResponse> for KeyboardInteractiveAuthenticationState {
    fn from(r: russh::client::KeyboardInteractiveAuthResponse) -> Self {
        match r {
            russh::client::KeyboardInteractiveAuthResponse::Success => KeyboardInteractiveAuthenticationState {
                state: "success".into(),
                remaining_methods: vec![],
                partial_success: false,
                instructions: None,
                prompts: None,
                name: None,
            },
            russh::client::KeyboardInteractiveAuthResponse::Failure { remaining_methods, partial_success } => {
                KeyboardInteractiveAuthenticationState {
                    state: "failure".into(),
                    partial_success,
                    instructions: None,
                    remaining_methods: remaining_methods.iter().map(|x| x.into()).collect(),
                    prompts: None,
                    name: None,
                }
            }
            russh::client::KeyboardInteractiveAuthResponse::InfoRequest { name, instructions, prompts } => {
                KeyboardInteractiveAuthenticationState {
                    state: "infoRequest".to_string(),
                    name: Some(name),
                    partial_success: false,
                    remaining_methods: vec![],
                    instructions: Some(instructions),
                    prompts: Some(prompts.into_iter().map(Into::into).collect()),
                }
            }
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshAuthResult {
    pub success: bool,
    pub partial_success: bool,
    pub remaining_methods: Vec<String>,
}

impl From<AuthResult> for SshAuthResult {
    fn from(r: AuthResult) -> Self {
        match r {
            AuthResult::Success => SshAuthResult { partial_success: false, remaining_methods: vec![], success: true },
            AuthResult::Failure { partial_success, remaining_methods } => SshAuthResult {
                success: false,
                partial_success,
                remaining_methods: remaining_methods.iter().map(|x| x.into()).collect(),
            },
        }
    }
}

pub(crate) enum StreamTo {
    Tcp { host: String, port: u16 },
    Exec { command: String },
}

pub struct SshClient {
    handle: Arc<Mutex<russh::client::Handle<SSHClientHandler>>>,
    outbox: Arc<Outbox>,
}

async fn disconnect_handle<H: russh::client::Handler>(handle: &russh::client::Handle<H>) -> Result<()> {
    match handle.disconnect(russh::Disconnect::ByApplication, "", "").await {
        // The peer or another teardown callback may already have closed the transport.
        Ok(()) | Err(russh::Error::SendError) => Ok(()),
        Err(error) => Err(wrap(error)),
    }
}

impl SshClient {
    /// A stream to host:port as the server sees it (direct-tcpip), or to a command's stdin and stdout there (its stderr
    /// is dropped), read and written in Rust without the page: for the RDP proxy (src/rdp.rs).
    pub(crate) async fn open_stream(&self, to: StreamTo) -> Result<russh::ChannelStream<russh::client::Msg>> {
        open_native_stream(self.handle.clone(), self.outbox.clone(), to).await
    }

    /// Closes the connection (its page has gone).
    pub(crate) fn shut_down(&self) {
        self.outbox.detach();
        let handle = self.handle.clone();
        tauri::async_runtime::spawn(async move {
            let _ = handle.lock().await.disconnect(russh::Disconnect::ByApplication, "", "").await;
        });
    }
}

/// Keep the open and its routing decision together even if the RDP connection is cancelled mid-open.
async fn open_native_stream<H: russh::client::Handler + 'static>(
    handle: Arc<Mutex<russh::client::Handle<H>>>, outbox: Arc<Outbox>, to: StreamTo,
) -> Result<russh::ChannelStream<russh::client::Msg>> {
    tokio::spawn(async move {
        let handle = handle.lock().await;
        let marked = outbox.begin_channel_open(true);
        let channel = match &to {
            StreamTo::Tcp { host, port } => handle.channel_open_direct_tcpip(host, *port as u32, "127.0.0.1", 0).await,
            StreamTo::Exec { .. } => handle.channel_open_session().await,
        };
        let channel = finish_channel_open(&outbox, marked, channel).await?;
        if let StreamTo::Exec { command } = to {
            channel.exec(true, command).await.map_err(|e| format!("SSH: {e}"))?;
        }
        Ok(channel.into_stream())
    }).await.map_err(|e| format!("SSH: {e}"))?
}

async fn finish_channel_open(
    outbox: &Outbox, marked: oneshot::Receiver<()>, channel: std::result::Result<russh::Channel<russh::client::Msg>, russh::Error>,
) -> Result<russh::Channel<russh::client::Msg>> {
    let channel = match channel {
        Ok(channel) => channel,
        Err(e) => {
            outbox.cancel_channel_open();
            return Err(format!("SSH: {e}"));
        }
    };
    // russh wakes channel_open before calling channel_open_confirmation. Fence every open before the next one,
    // including page channels, so a late confirmation cannot consume the following native channel's decision.
    marked.await.map_err(|_| "SSH: channel open cancelled".to_string())?;
    Ok(channel)
}

#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "camelCase")]
pub enum AuthMethod {
    None,
    Password { password: String },
    #[serde(rename_all = "camelCase")]
    Publickey { key: u32, hash_algorithm: Option<u32> },
    KeyboardInteractiveStart,
    KeyboardInteractiveRespond { responses: Vec<String> },
    /// `specificKey`: `<algorithm> <base64>`.
    #[serde(rename_all = "camelCase")]
    Agent { connection: AgentConnection, specific_key: Option<String> },
}

/// All the ways to authenticate; keyboard-interactive ones return its state, the others an SshAuthResult.
#[tauri::command]
pub async fn ssh_authenticate(ssh: State<'_, Ssh>, id: u32, username: String, method: AuthMethod) -> Result<serde_json::Value> {
    let client = ssh.client(id)?;
    let mut handle = client.handle.lock().await;
    let result: SshAuthResult = match method {
        AuthMethod::None => handle.authenticate_none(username).await.map_err(wrap)?.into(),
        AuthMethod::Password { password } => handle.authenticate_password(username, password).await.map_err(wrap)?.into(),
        AuthMethod::Publickey { key, hash_algorithm: hash } => {
            let key = ssh.key(key)?;
            let hash_algorithm = match hash {
                Some(x) => hash_algorithm(x),
                None => handle.best_supported_rsa_hash().await.map_err(wrap)?.flatten(),
            };
            handle
                .authenticate_publickey(username, PrivateKeyWithHashAlg::new(key, hash_algorithm))
                .await
                .map_err(wrap)?
                .into()
        }
        AuthMethod::KeyboardInteractiveStart => {
            let state: KeyboardInteractiveAuthenticationState =
                handle.authenticate_keyboard_interactive_start(username, None).await.map_err(wrap)?.into();
            return serde_json::to_value(state).map_err(|e| e.to_string());
        }
        AuthMethod::KeyboardInteractiveRespond { responses } => {
            let state: KeyboardInteractiveAuthenticationState =
                handle.authenticate_keyboard_interactive_respond(responses).await.map_err(wrap)?.into();
            return serde_json::to_value(state).map_err(|e| e.to_string());
        }
        AuthMethod::Agent { connection, specific_key } => {
            authenticate_agent(&mut handle, username, &connection, specific_key.as_deref()).await?
        }
    };
    serde_json::to_value(result).map_err(|e| e.to_string())
}

async fn authenticate_agent(
    handle: &mut russh::client::Handle<SSHClientHandler>,
    username: String,
    connection: &AgentConnection,
    specific_key: Option<&str>,
) -> Result<SshAuthResult> {
    let mut agent = get_agent_client(connection).await.map_err(String::from)?;

    let keys = match specific_key {
        Some(k) => {
            let k = public_key_from_page(k)?;
            // Offered only if the agent holds it, as OpenSSH does. A key the agent can't sign with gets as far as the
            // server's "go ahead, sign" before the signing fails, and russh (0.63 and 0.64 alike) then leaves the
            // connection waiting for that signature, so every later login method on it fails with `Inconsistent`
            // (Tabby issue #11743: a key file with a .pub next to it, not in the agent, profile on Auto). Skipped,
            // this is a plain failure and Tabby goes on to the key file itself.
            let held = agent.request_identities().await.map_err(wrap)?.iter().any(|identity| match identity {
                AgentIdentity::PublicKey { key, .. } => key.key_data() == k.key_data(),
                AgentIdentity::Certificate { certificate, .. } => certificate.public_key() == k.key_data(),
            });
            if !held {
                debug!("The agent doesn't hold {:?}; not offering it", k);
                return Ok(AuthResult::Failure { remaining_methods: MethodSet::empty(), partial_success: false }.into());
            }
            debug!("Trying specified key {:?}", k);
            vec![k.into()]
        }
        None => agent.request_identities().await.map_err(wrap)?,
    };

    let mut last_auth_result = AuthResult::Failure { remaining_methods: MethodSet::empty(), partial_success: false };

    let best_hash = handle.best_supported_rsa_hash().await.map_err(wrap)?.flatten();

    for key in keys {
        debug!("Trying key {key:?}");
        let result = match key {
            AgentIdentity::PublicKey { key, .. } => handle.authenticate_publickey_with(&username, key.clone(), best_hash, &mut agent).await,
            AgentIdentity::Certificate { certificate, .. } => {
                handle.authenticate_certificate_with(&username, certificate, best_hash, &mut agent).await
            }
        };
        let ret = result.map_err(wrap)?;
        if ret.success() {
            return Ok(ret.into());
        }
        last_auth_result = ret;
    }

    Ok(last_auth_result.into())
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum ClientOp {
    ChannelOpenSession,
    TcpipForward { address: String, port: u32 },
    CancelTcpipForward { address: String, port: u32 },
    #[serde(rename_all = "camelCase")]
    ChannelOpenDirectTcpip { address: String, port: u32, originator_address: String, originator_port: u32 },
    Disconnect,
}

/// Channels, forwards and disconnecting; channel opens return the new channel's id, `tcpipForward` the port.
#[tauri::command]
pub async fn ssh_client(ssh: State<'_, Ssh>, id: u32, op: ClientOp) -> Result<Option<u32>> {
    let client = ssh.client(id)?;
    let owner = ssh.owner_of(id);
    let new_channel = |ch: russh::Channel<russh::client::Msg>| {
        let new_channel = NewSshChannel::new(ch, client.outbox.clone());
        ssh.add(&owner, Object::NewChannel(Arc::new(new_channel)))
    };
    let handle = client.handle.lock().await;
    match op {
        ClientOp::ChannelOpenSession => {
            let marked = client.outbox.begin_channel_open(false);
            let ch = handle.channel_open_session().await;
            let ch = finish_channel_open(&client.outbox, marked, ch).await?;
            Ok(Some(new_channel(ch)))
        }
        ClientOp::TcpipForward { address, port } => Ok(Some(handle.tcpip_forward(address, port).await.map_err(wrap)?)),
        ClientOp::CancelTcpipForward { address, port } => {
            handle.cancel_tcpip_forward(address, port).await.map_err(wrap)?;
            Ok(None)
        }
        ClientOp::ChannelOpenDirectTcpip { address, port, originator_address, originator_port } => {
            let marked = client.outbox.begin_channel_open(false);
            let ch = handle.channel_open_direct_tcpip(address, port, originator_address, originator_port).await;
            let ch = finish_channel_open(&client.outbox, marked, ch).await?;
            Ok(Some(new_channel(ch)))
        }
        ClientOp::Disconnect => {
            client.outbox.close();
            disconnect_handle(&handle).await?;
            Ok(None)
        }
    }
}

/// The page has handled `length` bytes of this client's data.
#[tauri::command]
pub fn ssh_ack(ssh: State<'_, Ssh>, id: u32, length: usize) {
    if let Ok(client) = ssh.client(id) {
        client.outbox.ack(length);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectOptions {
    cipher_algos: Option<Vec<String>>,
    kex_algos: Option<Vec<String>>,
    key_algos: Option<Vec<String>>,
    mac_algos: Option<Vec<String>>,
    compression_algos: Option<Vec<String>>,
    connection_timeout_seconds: Option<u32>,
    keepalive_interval_seconds: Option<u32>,
    keepalive_max: u32,
}

/// Connects over a transport; everything the client says from then on (including the server key check, before this
/// returns) comes as frames on `events` (see outbox.rs).
#[tauri::command]
pub async fn ssh_connect(
    ssh: State<'_, Ssh>,
    app: AppHandle,
    webview: tauri::Webview,
    transport: u32,
    options: ConnectOptions,
    events: Channel<InvokeResponseBody>,
) -> Result<u32> {
    let transport_handle = ssh.transport(transport)?;
    debug!("connecting to {transport_handle:?}");

    let id = ssh.reserve();
    let outbox = Arc::new(Outbox::new(events));
    let handler = SSHClientHandler { app, owner: webview.label().to_string(), outbox: outbox.clone() };

    let mut preferred = russh::Preferred::DEFAULT.clone();
    if let Some(cipher_algos) = options.cipher_algos {
        preferred.cipher = cipher_algos.into_iter().filter_map(|x| russh::cipher::Name::try_from(&x[..]).ok()).collect();
    }
    if let Some(kex_algos) = options.kex_algos {
        preferred.kex = kex_algos
            .into_iter()
            .filter_map(|x| russh::kex::Name::try_from(&x[..]).ok())
            .chain([russh::kex::EXTENSION_SUPPORT_AS_CLIENT, russh::kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT])
            .collect();
    }
    if let Some(key_algos) = options.key_algos {
        preferred.key = Cow::Owned(key_algos.into_iter().filter_map(|x| russh::keys::Algorithm::from_str(&x[..]).ok()).collect());
    }
    if let Some(mac_algos) = options.mac_algos {
        preferred.mac = mac_algos.into_iter().filter_map(|x| russh::mac::Name::try_from(&x[..]).ok()).collect();
    }
    if let Some(compression_algos) = options.compression_algos {
        preferred.compression =
            compression_algos.into_iter().filter_map(|x| russh::compression::Name::try_from(&x[..]).ok()).collect();
    }

    let cfg = russh::client::Config {
        preferred,
        keepalive_interval: options.keepalive_interval_seconds.map(|x| Duration::from_secs(x as u64)),
        keepalive_max: options.keepalive_max as usize,
        ..Default::default()
    };

    let Some(stream) = transport_handle.take().await else {
        return Err("Transport already used".into());
    };
    ssh.remove(transport);

    let connection_fut = russh::client::connect_stream(Arc::new(cfg), stream, handler);
    let handle = if let Some(connection_timeout_seconds) = options.connection_timeout_seconds {
        tokio::time::timeout(Duration::from_secs(connection_timeout_seconds as u64), connection_fut)
            .await
            .map_err(|_| "Connection timeout".to_string())?
    } else {
        connection_fut.await
    }
    .map_err(String::from)?;

    let client = SshClient { handle: Arc::new(Mutex::new(handle)), outbox };
    ssh.insert(id, webview.label(), Object::Client(Arc::new(client)));
    Ok(id)
}


/// Debug regression probe: consume a native stream without routing its payload through the page.
#[cfg(debug_assertions)]
#[tauri::command]
pub async fn ssh_debug_read_stream(ssh: State<'_, Ssh>, id: u32, exec: bool) -> Result<usize> {
    use tokio::io::AsyncReadExt;
    let to = if exec { StreamTo::Exec { command: "fixture".into() } } else { StreamTo::Tcp { host: "localhost".into(), port: 9 } };
    let mut stream = ssh.client(id)?.open_stream(to).await?;
    let mut total = 0;
    let mut buf = [0; 32768];
    loop {
        let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 { return Ok(total); }
        total += n;
    }
}

#[cfg(not(debug_assertions))]
#[tauri::command]
pub async fn ssh_debug_read_stream(_ssh: State<'_, Ssh>, _id: u32, _exec: bool) -> Result<usize> {
    Err("debug build required".into())
}

#[cfg(test)]
#[path = "../../examples/ssh-stream-fixture.rs"]
mod fixture;

#[cfg(test)]
mod stream_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;

    struct Handler(Arc<Outbox>);
    impl russh::client::Handler for Handler {
        type Error = russh::Error;
        async fn check_server_key(&mut self, _: &russh::keys::PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> { Ok(true) }
        async fn channel_open_confirmation(&mut self, id: ChannelId, _: u32, _: u32, _: &mut russh::client::Session) -> std::result::Result<(), Self::Error> {
            tokio::task::yield_now().await;
            self.0.channel_opened(id.into());
            Ok(())
        }
        async fn data(&mut self, id: ChannelId, bytes: &[u8], _: &mut russh::client::Session) -> std::result::Result<(), Self::Error> {
            self.0.data(id.into(), None, bytes).await;
            self.0.ack(bytes.len());
            Ok(())
        }
        async fn extended_data(&mut self, id: ChannelId, ext: u32, bytes: &[u8], _: &mut russh::client::Session) -> std::result::Result<(), Self::Error> {
            self.0.data(id.into(), Some(ext), bytes).await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn disconnect_after_transport_closed_is_idempotent() {
        let (port, server) = fixture::start().await;
        let outbox = Arc::new(Outbox::new(Channel::new(|_| Ok(()))));
        let client = russh::client::connect(Arc::new(Default::default()), ("127.0.0.1", port), Handler(outbox)).await.unwrap();
        disconnect_handle(&client).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !client.is_closed() { tokio::task::yield_now().await; }
        }).await.unwrap();
        disconnect_handle(&client).await.unwrap();
        disconnect_handle(&client).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn native_tcp_and_exec_never_send_payload_to_the_page() {
        let (port, server) = fixture::start().await;
        let delivered = Arc::new(AtomicUsize::new(0));
        let outbox = Arc::new(Outbox::new(Channel::new({
            let delivered = delivered.clone();
            move |_| { delivered.fetch_add(1, Ordering::Relaxed); Ok(()) }
        })));
        let mut client = russh::client::connect(Arc::new(Default::default()), ("127.0.0.1", port), Handler(outbox.clone())).await.unwrap();
        assert!(client.authenticate_none("fixture").await.unwrap().success());
        let client = Arc::new(Mutex::new(client));
        for exec in [false, true, false, true] {
            let to = if exec { StreamTo::Exec { command: "fixture".into() } } else { StreamTo::Tcp { host: "localhost".into(), port: 9 } };
            let mut stream = open_native_stream(client.clone(), outbox.clone(), to).await.unwrap();
            let total = tokio::time::timeout(Duration::from_secs(5), tokio::io::copy(&mut stream, &mut tokio::io::sink())).await.unwrap().unwrap();
            assert_eq!(total as usize, fixture::BYTES);
            assert_eq!(delivered.load(Ordering::Relaxed), 0);
        }
        // A page open immediately followed by a native open must keep their routing decisions separate.
        let channel = {
            let handle = client.lock().await;
            let marked = outbox.begin_channel_open(false);
            finish_channel_open(&outbox, marked, handle.channel_open_session().await).await.unwrap()
        };
        let mut native = open_native_stream(client.clone(), outbox.clone(), StreamTo::Tcp { host: "localhost".into(), port: 9 }).await.unwrap();
        let total = tokio::time::timeout(Duration::from_secs(5), tokio::io::copy(&mut native, &mut tokio::io::sink())).await.unwrap().unwrap();
        assert_eq!(total as usize, fixture::BYTES);
        assert_eq!(delivered.load(Ordering::Relaxed), 0);
        channel.exec(true, "fixture").await.unwrap();
        let mut stream = channel.into_stream();
        let mut first = [0; 1];
        stream.read_exact(&mut first).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(delivered.load(Ordering::Relaxed) > 0);
        outbox.detach();
        server.abort();
    }
}
