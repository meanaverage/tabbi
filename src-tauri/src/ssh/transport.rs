// Derived from russh-napi (Apache-2.0, see LICENSE-2.0.txt).
use std::fmt::Debug;
use std::io::IoSlice;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_http_proxy::http_connect_tokio;
use delegate::delegate;
use serde::Deserialize;
use tauri::State;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::sync::Mutex;
use tokio_socks::tcp::Socks5Stream;

use super::error::{wrap, Result};
use super::{Object, Ssh};

#[derive(Debug)]
pub struct SshTransport(Mutex<Option<SshTransportInner>>);

pub(crate) enum SshTransportInner {
    Socket(TcpStream),
    Command(Child),
    SshChannel(russh::ChannelStream<russh::client::Msg>),
    SocksProxy(tokio_socks::tcp::socks5::Socks5Stream<TcpStream>),
}

impl Debug for SshTransportInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshTransportInner::Socket(_) => f.debug_struct("Socket").finish(),
            SshTransportInner::Command(_) => f.debug_struct("Command").finish(),
            SshTransportInner::SshChannel(_) => f.debug_struct("SshChannel").finish(),
            SshTransportInner::SocksProxy(_) => f.debug_struct("SocksProxy").finish(),
        }
    }
}

impl Drop for SshTransportInner {
    fn drop(&mut self) {
        match self {
            SshTransportInner::Socket(socket) => {
                let _ = futures::executor::block_on(socket.shutdown());
            }
            SshTransportInner::Command(child) => {
                // (russh-napi blocks on `kill().await`, which can stall a runtime thread; this only signals)
                let _ = child.start_kill();
            }
            SshTransportInner::SshChannel(_) | SshTransportInner::SocksProxy(_) => {
                // just drop the stream
            }
        }
    }
}

impl SshTransport {
    pub(crate) async fn take(&self) -> Option<SshTransportInner> {
        self.0.lock().await.take()
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TransportSpec {
    Socket { address: String },
    Command { command: String, args: Vec<String> },
    #[serde(rename_all = "camelCase")]
    SshChannel { channel: u32 },
    #[serde(rename_all = "camelCase")]
    SocksProxy { proxy_host: String, proxy_port: u16, target_host: String, target_port: u16 },
    #[serde(rename_all = "camelCase")]
    HttpProxy { proxy_host: String, proxy_port: u16, target_host: String, target_port: u16 },
}

#[tauri::command]
pub async fn ssh_transport_new(ssh: State<'_, Ssh>, webview: tauri::Webview, spec: TransportSpec) -> Result<u32> {
    let inner = match spec {
        TransportSpec::Socket { address } => {
            let socket = TcpStream::connect(address.clone()).await.map_err(|e| e.to_string())?;
            socket.set_nodelay(true).map_err(|e| e.to_string())?;
            SshTransportInner::Socket(socket)
        }
        TransportSpec::Command { command, args } => {
            let child = tokio::process::Command::new(command)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| e.to_string())?;
            SshTransportInner::Command(child)
        }
        TransportSpec::SshChannel { channel } => {
            let new_channel = ssh.new_channel(channel)?;
            let Some(handle) = new_channel.take().await else {
                return Err("Channel is already consumed".into());
            };
            ssh.remove(channel);
            // The inner connection's bytes are read here; the page needn't see them as channel data too.
            new_channel.outbox.mark_stream(handle.id().into());
            SshTransportInner::SshChannel(handle.into_stream())
        }
        TransportSpec::SocksProxy { proxy_host, proxy_port, target_host, target_port } => {
            let stream = Socks5Stream::connect((proxy_host.as_ref(), proxy_port), (target_host, target_port)).await.map_err(wrap)?;
            SshTransportInner::SocksProxy(stream)
        }
        TransportSpec::HttpProxy { proxy_host, proxy_port, target_host, target_port } => {
            let mut socket = TcpStream::connect((proxy_host, proxy_port)).await.map_err(|e| e.to_string())?;
            socket.set_nodelay(true).map_err(|e| e.to_string())?;
            http_connect_tokio(&mut socket, target_host.as_str(), target_port).await.map_err(wrap)?;
            SshTransportInner::Socket(socket)
        }
    };
    let transport = SshTransport(Mutex::new(Some(inner)));
    Ok(ssh.add(webview.label(), Object::Transport(Arc::new(transport))))
}

impl AsyncRead for SshTransportInner {
    delegate! {
        to match self.get_mut() {
            SshTransportInner::Socket(stream) => Pin::new(stream),
            SshTransportInner::Command(child) => Pin::new(child.stdout.as_mut().unwrap()),
            SshTransportInner::SshChannel(stream) => Pin::new(stream),
            SshTransportInner::SocksProxy(stream) => Pin::new(stream),
        } {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<std::result::Result<(), std::io::Error>>;
        }
    }
}

impl AsyncWrite for SshTransportInner {
    delegate! {
        to match self.get_mut() {
            SshTransportInner::Socket(stream) => Pin::new(stream),
            SshTransportInner::Command(child) => Pin::new(child.stdin.as_mut().unwrap()),
            SshTransportInner::SshChannel(stream) => Pin::new(stream),
            SshTransportInner::SocksProxy(stream) => Pin::new(stream),
        } {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<std::result::Result<usize, std::io::Error>>;

            fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::result::Result<(), std::io::Error>>;

            fn poll_write_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &[IoSlice<'_>],
            ) -> Poll<std::result::Result<usize, std::io::Error>>;

            fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::result::Result<(), std::io::Error>>;
        }
    }

    delegate! {
        to match self {
            SshTransportInner::Socket(stream) => Pin::new(stream),
            SshTransportInner::Command(child) => Pin::new(child.stdin.as_ref().unwrap()),
            SshTransportInner::SshChannel(stream) => Pin::new(stream),
            SshTransportInner::SocksProxy(stream) => Pin::new(stream),
        } {
            fn is_write_vectored(&self) -> bool;
        }
    }
}
