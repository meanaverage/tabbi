// Derived from russh-napi (Apache-2.0, see LICENSE-2.0.txt).
use std::sync::Arc;

use russh::keys::agent::client::{AgentClient, AgentStream};
use serde::Deserialize;
use tauri::ipc::{Channel, InvokeBody, InvokeResponseBody, Request};
use tauri::State;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use super::channel::header_id;
use super::error::{Result, WrappedError};
use super::{Object, Ssh};

/// russh-napi's AgentConnection; `kind`: Pageant = 0, Pipe = 1, Unix = 2.
#[derive(Deserialize, Clone, Debug)]
pub struct AgentConnection {
    pub kind: u32,
    pub path: Option<String>,
}

pub async fn get_agent_client(connection: &AgentConnection) -> std::result::Result<AgentClient<impl AgentStream + Send>, WrappedError> {
    match connection.kind {
        0 => {
            #[cfg(windows)]
            return Ok(AgentClient::connect_pageant().await?.dynamic());
            #[cfg(not(windows))]
            Err(russh::keys::Error::AgentFailure.into())
        }
        1 => {
            #[cfg(windows)]
            return Ok(AgentClient::connect_named_pipe(&connection.path.clone().unwrap_or_default()).await?.dynamic());
            #[cfg(not(windows))]
            Err(russh::keys::Error::AgentFailure.into())
        }
        _ => {
            #[cfg(unix)]
            return Ok(AgentClient::connect_uds(&connection.path.clone().unwrap_or_default()).await?.dynamic());
            #[cfg(not(unix))]
            Err(russh::keys::Error::AgentFailure.into())
        }
    }
}

type Writer = tokio::io::WriteHalf<Box<dyn AgentStream + Send + Unpin>>;

pub struct SshAgentStream {
    writer: Arc<Mutex<Option<Writer>>>,
    reader: tokio::task::AbortHandle,
    closed: tokio::sync::watch::Sender<bool>,
}

impl SshAgentStream {
    fn new(stream: Box<dyn AgentStream + Send + Unpin>, data: Channel<InvokeResponseBody>) -> Self {
        let (mut r, w) = tokio::io::split(stream);
        let writer = Arc::new(Mutex::new(Some(w)));
        let (closed, _) = tokio::sync::watch::channel(false);
        let reader = tokio::spawn({
            let writer = writer.clone();
            let closed = closed.clone();
            async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    match r.read(&mut buf).await {
                        Ok(n) => {
                            if data.send(InvokeResponseBody::Raw(buf[..n].to_vec())).is_err() || n == 0 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                closed.send_replace(true);
                writer.lock().await.take();
            }
        }).abort_handle();
        Self { writer, reader, closed }
    }

    pub(crate) fn close(&self) {
        self.closed.send_replace(true);
        self.reader.abort();
        let writer = self.writer.clone();
        // Any blocked write observes closed and releases this lock. Dropping both halves closes the agent socket.
        tauri::async_runtime::spawn(async move { writer.lock().await.take(); });
    }

    async fn write(&self, bytes: &[u8]) -> Result<()> {
        let mut closed = self.closed.subscribe();
        tokio::select! {
            biased;
            _ = closed.wait_for(|closed| *closed) => Err("Agent stream is closed".into()),
            result = async {
                let mut writer = self.writer.lock().await;
                let writer = writer.as_mut().ok_or("Agent stream is closed")?;
                writer.write_all(bytes).await.map_err(|_| "Failed to send data to channel".to_string())
            } => result,
        }
    }
}

impl Drop for SshAgentStream {
    fn drop(&mut self) {
        self.close();
    }
}

/// A connection to the local agent, for a forwarded agent channel. What the agent says goes to `data` as raw bytes;
/// an empty message means it has closed.
#[tauri::command]
pub async fn ssh_agent_connect(
    ssh: State<'_, Ssh>,
    webview: tauri::Webview,
    connection: AgentConnection,
    data: Channel<InvokeResponseBody>,
) -> Result<u32> {
    let stream = get_agent_client(&connection).await.map_err(String::from)?.into_inner();
    Ok(ssh.add(webview.label(), Object::Agent(Arc::new(SshAgentStream::new(stream, data)))))
}

/// The raw request body is the bytes, the `ssh-id` header says which agent stream.
#[tauri::command]
pub async fn ssh_agent_write(ssh: State<'_, Ssh>, request: Request<'_>) -> Result<()> {
    let agent = ssh.agent(header_id(&request)?)?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw bytes".into());
    };
    agent.write(bytes).await
}

#[tauri::command]
pub async fn ssh_agent_close(ssh: State<'_, Ssh>, id: u32) -> Result<()> {
    if let Some(Object::Agent(agent)) = ssh.remove(id) {
        agent.close();
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn close_cancels_a_blocked_write_and_drops_both_halves() {
        let (stream, mut peer) = tokio::io::duplex(64);
        let agent = Arc::new(SshAgentStream::new(Box::new(stream), Channel::new(|_| Ok(()))));
        let weak_writer = Arc::downgrade(&agent.writer);
        let writing = tokio::spawn({
            let agent = agent.clone();
            async move { agent.write(&[0; 65536]).await }
        });
        let mut first = [0; 1];
        peer.read_exact(&mut first).await.unwrap();
        assert!(!writing.is_finished());
        agent.close();
        assert!(tokio::time::timeout(Duration::from_secs(2), writing).await.unwrap().unwrap().is_err());
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), peer.read_to_end(&mut rest)).await.unwrap().unwrap();
        assert!(rest.len() < 65536);
        drop(agent);
        // The cancelled tasks drop their halves when they next run, which under load can take more than one yield.
        for _ in 0..200 {
            if weak_writer.upgrade().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(weak_writer.upgrade().is_none());
    }

    #[tokio::test]
    async fn release_and_reset_close_an_agent_even_while_a_writer_holds_it() {
        for reset in [false, true] {
            let ssh = Ssh::default();
            let (stream, mut peer) = tokio::io::duplex(64);
            let agent = Arc::new(SshAgentStream::new(Box::new(stream), Channel::new(|_| Ok(()))));
            let id = ssh.add("test", Object::Agent(agent.clone()));
            if reset { ssh.reset("test"); } else { ssh.remove(id).unwrap().close(); }
            let mut byte = [0];
            assert_eq!(tokio::time::timeout(Duration::from_secs(2), peer.read(&mut byte)).await.unwrap().unwrap(), 0);
            assert!(agent.write(b"after close").await.is_err());
        }
    }
}
