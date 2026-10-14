// Derived from russh-napi (Apache-2.0, see LICENSE-2.0.txt).
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use serde::{Deserialize, Serialize};
use tauri::ipc::{InvokeBody, Request};
use tauri::State;
use tokio::sync::oneshot;
use tokio::sync::Mutex;

use super::error::{wrap, Result};
use super::outbox::Outbox;
use super::sftp::SftpChannel;
use super::{Object, Ssh};

type ChannelType = russh::Channel<russh::client::Msg>;

/// A channel that's open but not in use yet: the page makes it a session channel, an SFTP one or a transport.
pub struct NewSshChannel {
    inner: Mutex<Option<ChannelType>>,
    pub(crate) outbox: Arc<Outbox>,
}

impl NewSshChannel {
    pub fn new(ch: ChannelType, outbox: Arc<Outbox>) -> Self {
        NewSshChannel { inner: Mutex::new(Some(ch)), outbox }
    }

    pub async fn take(&self) -> Option<ChannelType> {
        self.inner.lock().await.take()
    }
}

/// A channel in use. Its data reaches the page through the client's handler, so what arrives here is drained by a
/// waiter task; an operation takes the channel from the waiter and hands it back after.
pub struct SshChannel {
    waiter: Mutex<Option<ChannelWaiter>>,
    outbox: Arc<Outbox>,
}

impl SshChannel {
    fn new(ch: ChannelType, outbox: Arc<Outbox>) -> Self {
        SshChannel { waiter: Mutex::new(Some(ChannelWaiter::new(ch))), outbox }
    }
}

struct ChannelWaiter(oneshot::Receiver<ChannelType>, oneshot::Sender<()>);

impl ChannelWaiter {
    async fn inner_waiter(ch: &mut ChannelType) {
        while ch.wait().await.is_some() {}
    }

    async fn waiter(mut ch: ChannelType, cancel_rx: oneshot::Receiver<()>, return_tx: oneshot::Sender<ChannelType>) {
        tokio::select! {
            _ = Self::inner_waiter(&mut ch) => {}
            _ = cancel_rx => {}
        }
        let _ = return_tx.send(ch);
    }

    pub fn new(ch: ChannelType) -> Self {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (return_tx, return_rx) = oneshot::channel();
        tokio::spawn(Self::waiter(ch, cancel_rx, return_tx));
        Self(return_rx, cancel_tx)
    }

    pub async fn take(self) -> ChannelType {
        let _ = self.1.send(());
        self.0.await.expect("channel waiter did not return a channel")
    }
}

impl SshChannel {
    async fn take_expect<F, O>(&self, f: F) -> Result<O>
    where
        F: AsyncFnOnce(&mut ChannelType) -> Result<O>,
    {
        let mut lock = self.waiter.lock().await;
        let inner = lock.take().expect("channel is already consumed");
        let mut ch = inner.take().await;
        let out = f(&mut ch).await;
        *lock = Some(ChannelWaiter::new(ch));
        out
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activated {
    id: u32,
    channel_id: u32,
}

#[tauri::command]
pub async fn ssh_channel_activate(ssh: State<'_, Ssh>, id: u32) -> Result<Activated> {
    let new_channel = ssh.new_channel(id)?;
    let owner = ssh.owner_of(id);
    let Some(ch) = new_channel.take().await else {
        return Err("Channel is already consumed".into());
    };
    ssh.remove(id);
    let channel_id: u32 = ch.id().into();
    let id = ssh.add(&owner, Object::Channel(Arc::new(SshChannel::new(ch, new_channel.outbox.clone()))));
    Ok(Activated { id, channel_id })
}

#[tauri::command]
pub async fn ssh_channel_activate_sftp(ssh: State<'_, Ssh>, id: u32) -> Result<Activated> {
    let new_channel = ssh.new_channel(id)?;
    let owner = ssh.owner_of(id);
    let Some(ch) = new_channel.take().await else {
        return Err("Channel is already consumed".into());
    };
    ssh.remove(id);
    let channel_id: u32 = ch.id().into();
    // SFTP's bytes are read here; the page needn't see them as channel data too.
    new_channel.outbox.mark_stream(channel_id);
    ch.request_subsystem(true, "sftp").await.map_err(wrap)?;
    let sftp = SftpSession::new(ch.into_stream()).await.map_err(wrap)?;
    let id = ssh.add(&owner, Object::Sftp(Arc::new(SftpChannel::new(sftp))));
    Ok(Activated { id, channel_id })
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChannelRequest {
    #[serde(rename_all = "camelCase")]
    Pty { term: String, col_width: u32, row_height: u32, pix_width: u32, pix_height: u32 },
    Shell,
    Exec { command: String },
    #[serde(rename_all = "camelCase")]
    X11 { single_connection: bool, x11_protocol: String, x11_cookie: String, screen: u32 },
    AgentForwarding,
    #[serde(rename_all = "camelCase")]
    WindowChange { col_width: u32, row_height: u32, pix_width: u32, pix_height: u32 },
    Eof,
    Close,
}

#[tauri::command]
pub async fn ssh_channel_request(ssh: State<'_, Ssh>, id: u32, request: ChannelRequest) -> Result<()> {
    let channel = ssh.channel(id)?;
    if matches!(request, ChannelRequest::X11 { .. }) {
        channel.outbox.allow_x11();
    }
    channel
        .take_expect(async move |handle| {
            match request {
                ChannelRequest::Pty { term, col_width, row_height, pix_width, pix_height } => {
                    handle.request_pty(false, &term, col_width, row_height, pix_width, pix_height, &[]).await
                }
                ChannelRequest::Shell => handle.request_shell(true).await,
                ChannelRequest::Exec { command } => handle.exec(true, command).await,
                ChannelRequest::X11 { single_connection, x11_protocol, x11_cookie, screen } => {
                    handle.request_x11(false, single_connection, &x11_protocol, &x11_cookie, screen).await
                }
                ChannelRequest::AgentForwarding => handle.agent_forward(false).await,
                ChannelRequest::WindowChange { col_width, row_height, pix_width, pix_height } => {
                    handle.window_change(col_width, row_height, pix_width, pix_height).await
                }
                ChannelRequest::Eof => handle.eof().await,
                ChannelRequest::Close => handle.close().await,
            }
            .map_err(wrap)
        })
        .await
}

/// Data for a channel: the raw request body is the bytes, the `ssh-id` header says which channel.
#[tauri::command]
pub async fn ssh_channel_data(ssh: State<'_, Ssh>, request: Request<'_>) -> Result<()> {
    let id = header_id(&request)?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw bytes".into());
    };
    let bytes = bytes.clone();
    let channel = ssh.channel(id)?;
    channel
        .take_expect(async move |handle| handle.data_bytes(bytes).await.map_err(|_| "Failed to send data to channel".to_string()))
        .await
}

pub(crate) fn header_id(request: &Request<'_>) -> Result<u32> {
    request
        .headers()
        .get("ssh-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| "no ssh-id".to_string())
}
