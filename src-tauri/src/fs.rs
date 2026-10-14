//! The filesystem calls the page's `fs` module makes (see page/src/node/fs.ts). Errors carry Node's error codes
//! (`ENOENT: …`) so code that checks `error.code` keeps working.
use std::path::PathBuf;

use serde::Serialize;
use tauri::ipc::{InvokeBody, Request, Response};

pub fn node_error(e: std::io::Error, op: &str, path: &str) -> String {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        std::io::ErrorKind::AlreadyExists => "EEXIST",
        std::io::ErrorKind::NotADirectory => "ENOTDIR",
        std::io::ErrorKind::IsADirectory => "EISDIR",
        std::io::ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        _ => "EIO",
    };
    format!("{code}: {e}, {op} '{path}'")
}

fn expand(path: &str) -> PathBuf {
    PathBuf::from(path)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stat {
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime_ms: f64,
    atime_ms: f64,
    ctime_ms: f64,
    birthtime_ms: f64,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
}

fn ms(t: std::io::Result<std::time::SystemTime>) -> f64 {
    t.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// `stat` (or `lstat`), also for the page's synchronous calls (see sync.rs).
pub fn stat(path: &str, lstat: bool) -> Result<Stat, String> {
    let p = expand(path);
    let meta = if lstat { std::fs::symlink_metadata(&p) } else { std::fs::metadata(&p) }
        .map_err(|e| node_error(e, if lstat { "lstat" } else { "stat" }, path))?;
    #[cfg(unix)]
    let (mode, uid, gid) = {
        use std::os::unix::fs::MetadataExt;
        (meta.mode(), meta.uid(), meta.gid())
    };
    #[cfg(not(unix))]
    let (mode, uid, gid) = (if meta.is_dir() { 0o40755 } else { 0o100644 }, 0, 0);
    Ok(Stat {
        size: meta.len(),
        mode,
        uid,
        gid,
        mtime_ms: ms(meta.modified()),
        atime_ms: ms(meta.accessed()),
        ctime_ms: ms(meta.modified()),
        birthtime_ms: ms(meta.created()),
        is_file: meta.is_file(),
        is_directory: meta.is_dir(),
        is_symlink: meta.file_type().is_symlink(),
    })
}

#[tauri::command]
pub async fn fs_stat(path: String, lstat: Option<bool>) -> Result<Stat, String> {
    stat(&path, lstat.unwrap_or(false))
}

#[tauri::command]
pub async fn fs_read(path: String) -> Result<Response, String> {
    tokio::fs::read(expand(&path)).await.map(Response::new).map_err(|e| node_error(e, "open", &path))
}

/// The body is the bytes; headers: `path`, optionally `append: 1` and `mode` (octal).
#[tauri::command]
pub async fn fs_write(request: Request<'_>) -> Result<(), String> {
    let header = |k: &str| request.headers().get(k).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let path = header("path").ok_or("no path")?;
    let path = percent_decode(&path);
    let bytes: Vec<u8> = match request.body() {
        InvokeBody::Raw(b) => b.clone(),
        InvokeBody::Json(v) => v.as_str().unwrap_or_default().as_bytes().to_vec(),
    };
    let p = expand(&path);
    if header("append").is_some() {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&p).await.map_err(|e| node_error(e, "open", &path))?;
        f.write_all(&bytes).await.map_err(|e| node_error(e, "write", &path))?;
    } else {
        tokio::fs::write(&p, &bytes).await.map_err(|e| node_error(e, "open", &path))?;
    }
    #[cfg(unix)]
    if let Some(mode) = header("mode").and_then(|m| u32::from_str_radix(&m, 8).ok()) {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).await;
    }
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirEntry {
    name: String,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
}

#[tauri::command]
pub async fn fs_readdir(path: String) -> Result<Vec<DirEntry>, String> {
    let mut dir = tokio::fs::read_dir(expand(&path)).await.map_err(|e| node_error(e, "scandir", &path))?;
    let mut out = Vec::new();
    while let Some(entry) = dir.next_entry().await.map_err(|e| node_error(e, "scandir", &path))? {
        let ft = entry.file_type().await.ok();
        out.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_file: ft.map(|t| t.is_file()).unwrap_or(false),
            is_directory: ft.map(|t| t.is_dir()).unwrap_or(false),
            is_symlink: ft.map(|t| t.is_symlink()).unwrap_or(false),
        });
    }
    Ok(out)
}

#[tauri::command]
pub async fn fs_realpath(path: String) -> Result<String, String> {
    tokio::fs::canonicalize(expand(&path)).await.map(|p| p.to_string_lossy().into_owned()).map_err(|e| node_error(e, "realpath", &path))
}

#[tauri::command]
pub async fn fs_mkdir(path: String, recursive: Option<bool>) -> Result<(), String> {
    if recursive.unwrap_or(false) {
        tokio::fs::create_dir_all(expand(&path)).await
    } else {
        tokio::fs::create_dir(expand(&path)).await
    }
    .map_err(|e| node_error(e, "mkdir", &path))
}

#[tauri::command]
pub async fn fs_remove(path: String, recursive: Option<bool>, force: Option<bool>) -> Result<(), String> {
    let p = expand(&path);
    let result = match tokio::fs::symlink_metadata(&p).await {
        Ok(m) if m.is_dir() => {
            if recursive.unwrap_or(false) { tokio::fs::remove_dir_all(&p).await } else { tokio::fs::remove_dir(&p).await }
        }
        Ok(_) => tokio::fs::remove_file(&p).await,
        Err(e) => Err(e),
    };
    match result {
        Err(e) if force.unwrap_or(false) && e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r.map_err(|e| node_error(e, "rm", &path)),
    }
}

#[tauri::command]
pub async fn fs_rename(from: String, to: String) -> Result<(), String> {
    tokio::fs::rename(expand(&from), expand(&to)).await.map_err(|e| node_error(e, "rename", &from))
}

#[tauri::command]
pub async fn fs_chmod(path: String, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return tokio::fs::set_permissions(expand(&path), std::fs::Permissions::from_mode(mode)).await.map_err(|e| node_error(e, "chmod", &path));
    }
    #[allow(unreachable_code)]
    {
        let _ = (path, mode);
        Ok(())
    }
}
