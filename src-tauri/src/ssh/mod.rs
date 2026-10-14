//! SSH for the page: the native half of the `russh` npm package that tabby-ssh uses, as Tauri commands. The page's
//! `russh` module (page/src/node/russh.native.ts stands in for its native part) is the package's own JS wrapper over a shim of its native API.
//!
//! Derived from russh-napi (the package's napi binding, by Eugeny; Apache-2.0, see LICENSE-2.0.txt): the russh calls
//! are the same, but napi objects are handles here (ids in one table), and napi callbacks are frames on a Tauri
//! channel per client (see outbox.rs). A handle lives as its napi object would: until the page releases it (its JS
//! object was garbage collected, or it's been consumed or closed), or the page reloads (`ssh_reset`). So using what's
//! left of a disconnected client fails as it did with napi, with russh's own error.
pub mod agent;
pub mod channel;
pub mod client;
mod error;
pub mod key;
mod outbox;
pub mod sftp;
pub mod transport;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tauri::State;
use tokio::sync::oneshot;

pub(crate) enum Object {
    Transport(Arc<transport::SshTransport>),
    Client(Arc<client::SshClient>),
    NewChannel(Arc<channel::NewSshChannel>),
    Channel(Arc<channel::SshChannel>),
    Sftp(Arc<sftp::SftpChannel>),
    File(Arc<sftp::SftpFile>),
    Key(Arc<russh::keys::PrivateKey>),
    Agent(Arc<agent::SshAgentStream>),
}

impl Object {
    fn close(&self) {
        match self {
            Object::Client(client) => client.shut_down(),
            Object::Agent(agent) => agent.close(),
            _ => {}
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Object::Transport(_) => "transport",
            Object::Client(_) => "client",
            Object::NewChannel(_) => "new channel",
            Object::Channel(_) => "channel",
            Object::Sftp(_) => "SFTP channel",
            Object::File(_) => "SFTP file",
            Object::Key(_) => "key",
            Object::Agent(_) => "agent stream",
        }
    }
}

struct Entry {
    /// The webview that made it.
    owner: String,
    object: Object,
}

#[derive(Default)]
pub struct Ssh {
    next: AtomicU32,
    objects: Mutex<HashMap<u32, Entry>>,
    /// Server key checks waiting for the page's answer.
    answers: Mutex<HashMap<u32, oneshot::Sender<bool>>>,
}

macro_rules! getter {
    ($name:ident, $variant:ident, $ty:ty) => {
        pub(crate) fn $name(&self, id: u32) -> Result<Arc<$ty>, String> {
            match self.objects.lock().unwrap().get(&id).map(|e| &e.object) {
                Some(Object::$variant(x)) => Ok(x.clone()),
                Some(other) => Err(format!("SSH object {id} is a {}", other.kind())),
                None => Err(format!("No SSH object {id} (already closed or consumed)")),
            }
        }
    };
}

impl Ssh {
    pub(crate) fn reserve(&self) -> u32 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub(crate) fn insert(&self, id: u32, owner: &str, object: Object) {
        self.objects.lock().unwrap().insert(id, Entry { owner: owner.to_string(), object });
    }

    pub(crate) fn add(&self, owner: &str, object: Object) -> u32 {
        let id = self.reserve();
        self.insert(id, owner, object);
        id
    }

    pub(crate) fn remove(&self, id: u32) -> Option<Object> {
        self.objects.lock().unwrap().remove(&id).map(|e| e.object)
    }

    pub(crate) fn owner_of(&self, id: u32) -> String {
        self.objects.lock().unwrap().get(&id).map(|e| e.owner.clone()).unwrap_or_default()
    }

    getter!(transport, Transport, transport::SshTransport);
    getter!(client, Client, client::SshClient);
    getter!(new_channel, NewChannel, channel::NewSshChannel);
    getter!(channel, Channel, channel::SshChannel);
    getter!(sftp, Sftp, sftp::SftpChannel);
    getter!(file, File, sftp::SftpFile);
    getter!(key, Key, russh::keys::PrivateKey);
    getter!(agent, Agent, agent::SshAgentStream);
}

/// The page is done with an object (its JS wrapper was garbage collected).
#[tauri::command]
pub fn ssh_release(ssh: State<'_, Ssh>, id: u32) {
    if let Some(object) = ssh.remove(id) {
        object.close();
    }
}

/// A (re)loaded page starts with nothing: what the page before it had is dropped and its connections closed.
#[tauri::command]
pub fn ssh_reset(ssh: State<'_, Ssh>, webview: tauri::Webview) {
    ssh.reset(webview.label());
}

impl Ssh {
    fn reset(&self, owner: &str) {
        let removed: Vec<Entry> = {
            let mut objects = self.objects.lock().unwrap();
            let ids: Vec<u32> = objects.iter().filter(|(_, e)| e.owner == owner).map(|(id, _)| *id).collect();
            ids.into_iter().filter_map(|id| objects.remove(&id)).collect()
        };
        for entry in &removed {
            entry.object.close();
        }
    }
}
