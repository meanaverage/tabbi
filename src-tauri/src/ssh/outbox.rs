//! What a client sends the page (russh-napi's callbacks), as one ordered stream of raw frames on a Tauri channel:
//!
//! - `[0][channel u32][bytes]`: channel data
//! - `[1][channel u32][ext u32][bytes]`: extended data (stderr)
//! - `[2][JSON]`: an event (eof, close, disconnect, banner, channel opens by the server, server key check)
//!
//! One channel keeps data and events in order (a channel's data before its eof and close). Data waits while more than
//! MAX_UNACKED bytes are unacknowledged by the page, which holds up the connection (its TCP window) instead of piling
//! output up in memory, and frames sent in quick succession go out in batches, as terminal output does (pty.rs).
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};
use tokio::sync::{mpsc, oneshot, Notify};

const MAX_UNACKED: usize = 1024 * 1024;
const BATCH_INTERVAL: Duration = Duration::from_millis(4);
const MAX_FRAME: usize = 256 * 1024;

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum Event {
    Eof { channel: u32 },
    Close { channel: u32 },
    Disconnect { error: Option<String> },
    Banner { banner: String },
    #[serde(rename_all = "camelCase")]
    ServerKey { id: u32, key: super::key::PublicKeyInfo },
    #[serde(rename_all = "camelCase")]
    X11Open { channel: u32, originator_address: String, originator_port: u32 },
    #[serde(rename_all = "camelCase")]
    TcpipOpen { channel: u32, connected_address: String, connected_port: u32, originator_address: String, originator_port: u32 },
    AgentOpen { channel: u32 },
}

enum Frame {
    Data { channel: u32, ext: Option<u32>, bytes: Vec<u8> },
    Event(Vec<u8>),
}

impl Frame {
    fn encode(self) -> Vec<u8> {
        match self {
            Frame::Data { channel, ext: None, bytes } => {
                let mut out = Vec::with_capacity(5 + bytes.len());
                out.push(0);
                out.extend_from_slice(&channel.to_be_bytes());
                out.extend_from_slice(&bytes);
                out
            }
            Frame::Data { channel, ext: Some(ext), bytes } => {
                let mut out = Vec::with_capacity(9 + bytes.len());
                out.push(1);
                out.extend_from_slice(&channel.to_be_bytes());
                out.extend_from_slice(&ext.to_be_bytes());
                out.extend_from_slice(&bytes);
                out
            }
            Frame::Event(mut json) => {
                json.insert(0, 2);
                json
            }
        }
    }
}

pub struct Outbox {
    tx: mpsc::UnboundedSender<Frame>,
    unacked: Mutex<usize>,
    room: Notify,
    closed: AtomicBool,
    /// The page is gone: nothing more goes to it.
    detached: Arc<AtomicBool>,
    /// Channels read as byte streams here (SFTP, a jump host's transport): the page doesn't get their data.
    streams: Mutex<HashSet<u32>>,
    /// The next client channel open and its routing decision (opens are serialized by the client handle).
    channel_open: Mutex<Option<(bool, oneshot::Sender<()>)>>,
    /// A session channel asked for X11 forwarding: until then the server may not open X11 channels.
    x11: AtomicBool,
}

impl Outbox {
    pub fn new(channel: Channel<InvokeResponseBody>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let detached = Arc::new(AtomicBool::new(false));
        tauri::async_runtime::spawn(pump(rx, channel, detached.clone()));
        Outbox { tx, unacked: Mutex::new(0), room: Notify::new(), closed: AtomicBool::new(false), detached, streams: Mutex::default(), channel_open: Mutex::default(), x11: AtomicBool::new(false) }
    }

    pub fn begin_channel_open(&self, native: bool) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        *self.channel_open.lock().unwrap() = Some((native, tx));
        rx
    }

    pub fn cancel_channel_open(&self) {
        self.channel_open.lock().unwrap().take();
    }

    /// Called by russh before it can dispatch the first data packet, even for a server that sends immediately.
    pub fn channel_opened(&self, channel: u32) {
        if let Some((native, ready)) = self.channel_open.lock().unwrap().take() {
            if native { self.mark_stream(channel); }
            let _ = ready.send(());
        }
    }

    pub fn allow_x11(&self) {
        self.x11.store(true, Ordering::Relaxed);
    }

    pub fn x11_allowed(&self) -> bool {
        self.x11.load(Ordering::Relaxed)
    }

    pub fn mark_stream(&self, channel: u32) {
        self.streams.lock().unwrap().insert(channel);
    }

    pub async fn data(&self, channel: u32, ext: Option<u32>, bytes: &[u8]) {
        if self.streams.lock().unwrap().contains(&channel) {
            return;
        }
        loop {
            let notified = self.room.notified();
            if *self.unacked.lock().unwrap() < MAX_UNACKED || self.closed.load(Ordering::Relaxed) {
                break;
            }
            notified.await;
        }
        *self.unacked.lock().unwrap() += bytes.len();
        let _ = self.tx.send(Frame::Data { channel, ext, bytes: bytes.to_vec() });
    }

    pub fn event(&self, event: Event) {
        if let Event::Close { channel } = &event {
            self.streams.lock().unwrap().remove(channel);
        }
        if let Ok(json) = serde_json::to_vec(&event) {
            let _ = self.tx.send(Frame::Event(json));
        }
    }

    pub fn ack(&self, length: usize) {
        let mut unacked = self.unacked.lock().unwrap();
        *unacked = unacked.saturating_sub(length);
        self.room.notify_waiters();
    }

    /// Nobody will acknowledge anything any more: data stops waiting for room.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.room.notify_waiters();
    }

    /// The page that was listening is gone (released or reloaded): what's left isn't sent.
    pub fn detach(&self) {
        self.detached.store(true, Ordering::Relaxed);
        self.close();
    }
}

async fn pump(mut rx: mpsc::UnboundedReceiver<Frame>, channel: Channel<InvokeResponseBody>, detached: Arc<AtomicBool>) {
    let mut last_send = tokio::time::Instant::now() - BATCH_INTERVAL;
    while let Some(first) = rx.recv().await {
        // While frames keep coming, gather them for a moment instead of sending each on its own.
        tokio::time::sleep_until(last_send + BATCH_INTERVAL).await;
        let mut frames = vec![first];
        while let Ok(frame) = rx.try_recv() {
            frames.push(frame);
        }
        for frame in coalesce(frames) {
            if detached.load(Ordering::Relaxed) || channel.send(InvokeResponseBody::Raw(frame.encode())).is_err() {
                return;
            }
        }
        last_send = tokio::time::Instant::now();
    }
}

/// Joins consecutive data frames of the same channel (and stream).
fn coalesce(frames: Vec<Frame>) -> Vec<Frame> {
    let mut out: Vec<Frame> = Vec::with_capacity(frames.len());
    for frame in frames {
        if let (Some(Frame::Data { channel: c1, ext: e1, bytes: b1 }), Frame::Data { channel, ext, bytes }) = (out.last_mut(), &frame) {
            if c1 == channel && e1 == ext && b1.len() + bytes.len() <= MAX_FRAME {
                b1.extend_from_slice(bytes);
                continue;
            }
        }
        out.push(frame);
    }
    out
}
