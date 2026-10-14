// Derived from russh-napi (Apache-2.0, see LICENSE-2.0.txt).
use std::sync::Arc;

use russh_sftp::client::fs::DirEntry;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags};
use serde::{Deserialize, Serialize};
use tauri::ipc::{InvokeBody, Request, Response};
use tauri::State;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use super::channel::header_id;
use super::error::{wrap, Result};
use super::{Object, Ssh};

#[derive(Serialize)]
pub struct SftpFileMetadata {
    size: String, // u64
    uid: Option<u32>,
    user: Option<String>,
    gid: Option<u32>,
    group: Option<String>,
    permissions: Option<u32>,
    atime: Option<u32>,
    mtime: Option<u32>,
    #[serde(rename = "type")]
    kind: u32,
}

/// russh-napi's SftpFileType: Directory = 0, File = 1, Symlink = 2, Other = 3.
fn file_type(value: FileType) -> u32 {
    match value {
        FileType::Dir => 0,
        FileType::File => 1,
        FileType::Symlink => 2,
        FileType::Other => 3,
    }
}

impl From<&FileAttributes> for SftpFileMetadata {
    fn from(value: &FileAttributes) -> Self {
        Self {
            size: format!("{}", value.size.unwrap_or(0)),
            uid: value.uid,
            user: value.user.clone(),
            gid: value.gid,
            group: value.group.clone(),
            permissions: value.permissions,
            atime: value.atime,
            mtime: value.mtime,
            kind: file_type(value.file_type()),
        }
    }
}

#[derive(Serialize)]
pub struct SftpDirEntry {
    name: String,
    #[serde(rename = "type")]
    kind: u32,
    metadata: SftpFileMetadata,
}

impl From<DirEntry> for SftpDirEntry {
    fn from(entry: DirEntry) -> Self {
        SftpDirEntry {
            name: entry.file_name(),
            kind: file_type(entry.file_type()),
            metadata: SftpFileMetadata::from(&entry.metadata()),
        }
    }
}

pub struct SftpFile {
    handle: Mutex<russh_sftp::client::fs::File>,
}

pub struct SftpChannel {
    handle: Mutex<SftpSession>,
}

impl SftpChannel {
    pub fn new(ch: SftpSession) -> Self {
        SftpChannel { handle: Mutex::new(ch) }
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum SftpOp {
    ReadDir { path: String },
    Stat { path: String },
    CreateDir { path: String },
    Readlink { path: String },
    Rename { src: String, dst: String },
    Chmod { path: String, mode: u32 },
    RemoveDir { path: String },
    RemoveFile { path: String },
    Close,
    Open { path: String, mode: u32 },
}

#[tauri::command]
pub async fn ssh_sftp(ssh: State<'_, Ssh>, id: u32, op: SftpOp) -> Result<serde_json::Value> {
    let sftp = ssh.sftp(id)?;
    let handle = sftp.handle.lock().await;
    match op {
        SftpOp::ReadDir { path } => {
            let entries: Vec<SftpDirEntry> = handle.read_dir(path).await.map_err(wrap)?.map(Into::into).collect();
            json(&entries)
        }
        SftpOp::Stat { path } => json(&SftpFileMetadata::from(&handle.metadata(path).await.map_err(wrap)?)),
        SftpOp::CreateDir { path } => json(&handle.create_dir(path).await.map_err(wrap)?),
        SftpOp::Readlink { path } => json(&handle.read_link(path).await.map_err(wrap)?),
        SftpOp::Rename { src, dst } => json(&handle.rename(src, dst).await.map_err(wrap)?),
        SftpOp::Chmod { path, mode } => {
            let mut metadata = handle.metadata(&path).await.map_err(wrap)?;
            metadata.permissions = Some(mode);
            json(&handle.set_metadata(path, metadata).await.map_err(wrap)?)
        }
        SftpOp::RemoveDir { path } => json(&handle.remove_dir(path).await.map_err(wrap)?),
        SftpOp::RemoveFile { path } => json(&handle.remove_file(path).await.map_err(wrap)?),
        SftpOp::Close => {
            let result = handle.close().await.map_err(wrap);
            drop(handle);
            ssh.remove(id);
            json(&result?)
        }
        SftpOp::Open { path, mode } => {
            let flags = OpenFlags::from_bits(mode).ok_or("incorrect mode")?;
            let f = handle.open_with_flags(path, flags).await.map_err(wrap)?;
            let owner = ssh.owner_of(id);
            json(&ssh.add(&owner, Object::File(Arc::new(SftpFile { handle: Mutex::new(f) }))))
        }
    }
}

fn json<T: Serialize>(value: &T) -> Result<serde_json::Value> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn ssh_sftp_file_read(ssh: State<'_, Ssh>, id: u32, n: u32) -> Result<Response> {
    let file = ssh.file(id)?;
    let mut handle = file.handle.lock().await;
    let mut buf = Vec::with_capacity(n as usize);
    let len = handle.read_buf(&mut buf).await.map_err(wrap)?;
    buf.truncate(len);
    Ok(Response::new(buf))
}

/// The raw request body is the bytes, the `ssh-id` header says which file.
#[tauri::command]
pub async fn ssh_sftp_file_write(ssh: State<'_, Ssh>, request: Request<'_>) -> Result<()> {
    let file = ssh.file(header_id(&request)?)?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw bytes".into());
    };
    let mut handle = file.handle.lock().await;
    handle.write_all(bytes).await.map_err(wrap)
}

#[tauri::command]
pub async fn ssh_sftp_file_flush(ssh: State<'_, Ssh>, id: u32) -> Result<()> {
    let file = ssh.file(id)?;
    let mut handle = file.handle.lock().await;
    handle.flush().await.map_err(wrap)
}

#[tauri::command]
pub async fn ssh_sftp_file_shutdown(ssh: State<'_, Ssh>, id: u32) -> Result<()> {
    let file = ssh.file(id)?;
    let result = file.handle.lock().await.shutdown().await.map_err(wrap);
    ssh.remove(id);
    result
}
