//! Loopback-only SSH peer for native-stream and channel-policy regressions. Its key exists only in memory.
use std::sync::Arc;
use russh::{server, Channel, ChannelId};
use tokio::net::TcpListener;

pub const BYTES: usize = 4 * 1024 * 1024;
#[derive(Default)]
pub struct Peer(std::collections::HashMap<ChannelId, Channel<server::Msg>>);

impl server::Handler for Peer {
    type Error = russh::Error;
    async fn auth_none(&mut self, _: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }
    async fn channel_open_session(&mut self, channel: Channel<server::Msg>, reply: server::ChannelOpenHandle, _: &mut server::Session) -> Result<(), Self::Error> {
        self.0.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn channel_open_direct_tcpip(&mut self, channel: Channel<server::Msg>, _: &str, _: u32, _: &str, _: u32, reply: server::ChannelOpenHandle, _: &mut server::Session) -> Result<(), Self::Error> {
        reply.accept().await;
        // No client request precedes this data: it may arrive with the open confirmation.
        flood(channel);
        Ok(())
    }
    async fn shell_request(&mut self, channel: ChannelId, session: &mut server::Session) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        session.data(channel, &b"fixture$ "[..])?;
        Ok(())
    }
    /// Typing "x11" in the shell makes the peer open an X11 channel back, whether or not the client asked for
    /// forwarding, and report in the shell what the client did with it.
    async fn data(&mut self, channel: ChannelId, data: &[u8], session: &mut server::Session) -> Result<(), Self::Error> {
        if !data.windows(3).any(|w| w == b"x11") { return Ok(()); }
        let handle = session.handle();
        tokio::spawn(async move {
            let report = match handle.channel_open_x11("127.0.0.1", 6010).await {
                Ok(mut x11) => {
                    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                        while let Some(msg) = x11.wait().await {
                            if matches!(msg, russh::ChannelMsg::Close | russh::ChannelMsg::Eof) { break; }
                        }
                    }).await.is_ok();
                    if closed { "x11-accepted-then-closed" } else { "x11-accepted-and-open" }
                }
                Err(_) => "x11-rejected",
            };
            let _ = handle.data(channel, format!("{report}\r\nfixture$ ").into_bytes()).await;
        });
        Ok(())
    }
    async fn exec_request(&mut self, channel: ChannelId, _: &[u8], session: &mut server::Session) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        let channel = self.0.remove(&channel).unwrap();
        channel.extended_data(1, &b"native stderr"[..]).await?;
        flood(channel);
        Ok(())
    }
}

fn flood(channel: Channel<server::Msg>) {
    tokio::spawn(async move {
        for _ in 0..BYTES / 32768 {
            if channel.data(&vec![b'x'; 32768][..]).await.is_err() { return; }
        }
        let _ = channel.eof().await;
    });
}

pub async fn start() -> (u16, tokio::task::JoinHandle<()>) {
    use ring::rand::SecureRandom;
    let mut seed = [0; 32];
    ring::rand::SystemRandom::new().fill(&mut seed).unwrap();
    let key = russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&seed);
    let config = Arc::new(server::Config {
        keys: vec![russh::keys::PrivateKey::from(key)],
        auth_rejection_time: std::time::Duration::ZERO,
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let mut sessions = tokio::task::JoinSet::new();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let config = config.clone();
            sessions.spawn(async move {
                if let Ok(session) = server::run_stream(config, socket, Peer::default()).await {
                    let _ = session.await;
                }
            });
            while sessions.try_join_next().is_some() {}
        }
    });
    (port, task)
}

#[allow(dead_code)]
#[tokio::main]
async fn main() {
    let (port, _task) = start().await;
    println!("{port}");
    std::future::pending::<()>().await;
}
