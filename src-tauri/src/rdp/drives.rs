//! Shared folders as drives on the remote desktop (MS-RDPEFS drive redirection), ported from tabby-rdp's drives.ts.
//! IronRDP's web client asks a JavaScript file system (its patch 15); the page's SharedDrives forwards each call here.
//!
//! Every operation runs on a blocking thread, one request at a time per drive (a slow disk or network folder slows its
//! drive, not the window or the other drives). The server's paths are relative to the drive, with backslashes. Each
//! resolves inside its folder, also once symbolic links are followed: a path that leaves the folder, by `..` or through
//! a link, doesn't exist as far as the remote is concerned. Only regular files and folders are served (no devices,
//! pipes or sockets, which could block or do things on being opened). Read-only folders refuse anything that writes.
//! Errors go back as `CODE: message` with Node's errno names, which IronRDP maps to NT statuses.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use tauri::ipc::{Request, Response};
use tauri::State;

/// One read's worth: the server asks for 64 KiB or so; the proxy takes messages up to 32 MiB.
const READ_LIMIT: usize = 4 * 1024 * 1024;

/// An error as IronRDP's file system expects it: a Node-style code (ENOENT, EACCES, …) and words.
#[derive(Debug)]
struct DriveError {
    code: &'static str,
    message: String,
}

impl DriveError {
    fn new(code: &'static str, message: impl Into<String>) -> Self { Self { code, message: message.into() } }
}

impl From<DriveError> for String {
    fn from(e: DriveError) -> String { format!("{}: {}", e.code, e.message) }
}

/// The errno name for an I/O error, as Node would give it.
fn code_of(e: &std::io::Error) -> &'static str {
    #[cfg(unix)]
    if let Some(errno) = e.raw_os_error() {
        return match errno {
            libc::ENOENT => "ENOENT",
            libc::EEXIST => "EEXIST",
            libc::EACCES => "EACCES",
            libc::EPERM => "EPERM",
            libc::EROFS => "EROFS",
            libc::ENOTEMPTY => "ENOTEMPTY",
            libc::EISDIR => "EISDIR",
            libc::ENOTDIR => "ENOTDIR",
            libc::ENOSPC => "ENOSPC",
            libc::EBUSY => "EBUSY",
            libc::EINVAL => "EINVAL",
            libc::ENAMETOOLONG => "ENAMETOOLONG",
            libc::EBADF => "EBADF",
            // O_NOFOLLOW met a link that was put in the file's place: not a file to serve.
            libc::ELOOP => "EACCES",
            _ => "EIO",
        };
    }
    match e.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::DirectoryNotEmpty => "ENOTEMPTY",
        ErrorKind::IsADirectory => "EISDIR",
        ErrorKind::NotADirectory => "ENOTDIR",
        ErrorKind::StorageFull => "ENOSPC",
        ErrorKind::ResourceBusy => "EBUSY",
        ErrorKind::InvalidInput | ErrorKind::InvalidFilename => "EINVAL",
        ErrorKind::ReadOnlyFilesystem => "EROFS",
        _ => "EIO",
    }
}

fn io(context: &str) -> impl Fn(std::io::Error) -> DriveError + '_ {
    move |e| DriveError::new(code_of(&e), format!("{context}: {e}"))
}

/// A folder shared as a drive (`remoteDesktop.sharedFolders`): RDPDR device id, path, the name the remote shows.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSpec {
    id: u32,
    path: String,
    name: String,
    #[serde(default)]
    read_only: bool,
}

struct Drive {
    spec: FolderSpec,
    /// One request at a time.
    lock: tokio::sync::Mutex<()>,
}

struct OpenFile {
    file: Arc<File>,
    device: u32,
}

struct DriveSession {
    owner: String,
    drives: HashMap<u32, Arc<Drive>>,
    files: Mutex<HashMap<u32, OpenFile>>,
    next_handle: AtomicU32,
}

/// The connections' shared drives, by the id rdp_drives_open gave out.
#[derive(Default)]
pub struct Drives {
    next: AtomicU32,
    sessions: Mutex<HashMap<u32, Arc<DriveSession>>>,
}

impl Drives {
    fn session(&self, id: u32) -> Result<Arc<DriveSession>, DriveError> {
        self.sessions.lock().unwrap().get(&id).cloned().ok_or_else(|| DriveError::new("EBADF", "the shared drives are closed"))
    }

    /// The page has gone (reloaded, closed): whatever it left open is closed.
    pub(crate) fn reset(&self, owner: &str) {
        self.sessions.lock().unwrap().retain(|_, session| session.owner != owner);
    }
}

impl DriveSession {
    fn drive(&self, device: u32) -> Result<Arc<Drive>, DriveError> {
        self.drives.get(&device).cloned().ok_or_else(|| DriveError::new("ENOENT", format!("no drive {device}")))
    }

    fn file(&self, handle: u32) -> Result<(Arc<File>, Arc<Drive>), DriveError> {
        let files = self.files.lock().unwrap();
        let open = files.get(&handle).ok_or_else(|| DriveError::new("EBADF", format!("no open file {handle}")))?;
        Ok((open.file.clone(), self.drive(open.device)?))
    }
}

/// Runs `f` on a blocking thread while holding the drive's lock: one request at a time per drive.
async fn on_drive<T: Send + 'static>(drive: &Drive, f: impl FnOnce() -> Result<T, DriveError> + Send + 'static) -> Result<T, String> {
    let _turn = drive.lock.lock().await;
    tokio::task::spawn_blocking(f).await.map_err(|e| format!("EIO: {e}"))?.map_err(String::from)
}

/// Where a server path is on this computer, once checked to be inside its shared folder.
#[derive(Debug)]
struct Place {
    /// The path to work on: links resolved up to (or, when following, including) the last component.
    local: PathBuf,
    /// The shared folder itself.
    root: bool,
}

fn inside(real: &Path, root: &Path) -> bool { real.starts_with(root) }

/// The place of `remote` in `folder`. `..` doesn't resolve, and neither does a path whose links lead out of the shared
/// folder (ENOENT either way). With `follow`, the last component's link is followed too (reading, listing, opening);
/// without, it is left alone (removing or renaming a link works on the link, not its target). What is there, if
/// anything, is for the caller to find out.
fn locate(folder: &FolderSpec, remote: &str, follow: bool) -> Result<Place, DriveError> {
    let mut parts = Vec::new();
    for part in remote.split(['\\', '/']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains('\0') {
            return Err(DriveError::new("ENOENT", format!("{remote}: not inside {}", folder.name)));
        }
        parts.push(part);
    }
    // The folder's own real path (it may be reached through a link itself, as /tmp is on macOS).
    let root = std::fs::canonicalize(&folder.path).map_err(io(&folder.name))?;
    let Some((last, folders)) = parts.split_last() else {
        return Ok(Place { local: root, root: true });
    };
    let outside = || DriveError::new("ENOENT", format!("{remote}: leads out of {}", folder.name));
    // The folder it is in, links followed: there, and inside.
    let parent = std::fs::canonicalize(folders.iter().fold(root.clone(), |path, part| path.join(part))).map_err(io(remote))?;
    if !inside(&parent, &root) {
        return Err(outside());
    }
    let local = parent.join(last);
    if !follow {
        return Ok(Place { local, root: false });
    }
    match std::fs::canonicalize(&local) {
        Ok(real) if inside(&real, &root) => Ok(Place { root: real == root, local: real }),
        Ok(_) => Err(outside()),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Nothing there. A link to nothing isn't a place to make something: it would be made where the link points.
            if std::fs::symlink_metadata(&local).is_ok() {
                return Err(outside());
            }
            Ok(Place { local, root: false })
        }
        Err(e) => Err(io(remote)(e)),
    }
}

fn writable(folder: &FolderSpec, what: &str) -> Result<(), DriveError> {
    if folder.read_only {
        return Err(DriveError::new("EACCES", format!("{} is shared read-only: {what}", folder.name)));
    }
    Ok(())
}

/// What a shared drive's stat returns (iron-remote-desktop-rdp's `DriveStat`); times in ms since 1970.
#[derive(Serialize, Debug)]
pub struct Stat {
    dir: bool,
    size: u64,
    created: f64,
    accessed: f64,
    modified: f64,
    readonly: bool,
}

fn millis(time: std::io::Result<std::time::SystemTime>) -> f64 {
    time.ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

fn stat(folder: &FolderSpec, remote: &str) -> Result<Option<Stat>, DriveError> {
    let place = match locate(folder, remote, true) {
        Ok(place) => place,
        Err(e) if e.code == "ENOENT" || e.code == "ENOTDIR" => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = match std::fs::metadata(&place.local) {
        Ok(meta) => meta,
        Err(e) if matches!(code_of(&e), "ENOENT" | "ENOTDIR") => return Ok(None),
        Err(e) => return Err(io(remote)(e)),
    };
    // Devices, pipes and sockets aren't shown.
    if !meta.is_file() && !meta.is_dir() {
        return Ok(None);
    }
    #[cfg(unix)]
    let read_only = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o200 == 0
    };
    #[cfg(not(unix))]
    let read_only = meta.permissions().readonly();
    Ok(Some(Stat {
        dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        created: millis(meta.created().or_else(|_| meta.modified())),
        accessed: millis(meta.accessed()),
        modified: millis(meta.modified()),
        readonly: folder.read_only || read_only,
    }))
}

fn list(folder: &FolderSpec, remote: &str) -> Result<Vec<String>, DriveError> {
    let place = locate(folder, remote, true)?;
    let entries = std::fs::read_dir(&place.local).map_err(io(remote))?;
    // Names that aren't Unicode can't be named back by the server: left out.
    Ok(entries.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect())
}

#[derive(Deserialize, Clone, Copy)]
pub struct OpenMode {
    write: bool,
    create: bool,
    exclusive: bool,
    truncate: bool,
}

fn open(folder: &FolderSpec, remote: &str, mode: OpenMode) -> Result<File, DriveError> {
    let place = locate(folder, remote, true)?;
    let writes = mode.write || mode.create || mode.truncate;
    if writes {
        writable(folder, &format!("can't write {remote}"))?;
    }
    if let Ok(existing) = std::fs::metadata(&place.local) {
        if !existing.is_file() {
            return Err(DriveError::new(if existing.is_dir() { "EISDIR" } else { "EACCES" }, format!("{remote}: not a file")));
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(writes);
    if mode.create && mode.exclusive {
        options.create_new(true);
    } else if mode.create {
        options.create(true);
    }
    options.truncate(mode.truncate);
    // The path was resolved just now: a link (or a pipe) put in its place meanwhile isn't followed (or waited on).
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).mode(0o644);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(&place.local).map_err(io(remote))?;
    if !file.metadata().map_err(io(remote))?.is_file() {
        return Err(DriveError::new("EACCES", format!("{remote}: not a file")));
    }
    Ok(file)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}
#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}
#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}
#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

fn read(file: &File, offset: u64, length: usize) -> Result<Vec<u8>, DriveError> {
    let mut buf = vec![0u8; length.min(READ_LIMIT)];
    let mut done = 0;
    // A read can come back short before the end of the file: fill what was asked.
    while done < buf.len() {
        match read_at(file, &mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(io("read")(e)),
        }
    }
    buf.truncate(done);
    Ok(buf)
}

fn write(file: &File, offset: u64, data: &[u8]) -> Result<usize, DriveError> {
    let mut done = 0;
    while done < data.len() {
        match write_at(file, &data[done..], offset + done as u64) {
            Ok(0) => return Err(DriveError::new("EIO", "write: nothing written")),
            Ok(n) => done += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(io("write")(e)),
        }
    }
    Ok(done)
}

fn mkdir(folder: &FolderSpec, remote: &str) -> Result<(), DriveError> {
    let place = locate(folder, remote, true)?;
    writable(folder, &format!("can't make {remote}"))?;
    std::fs::create_dir(&place.local).map_err(io(remote))
}

fn remove(folder: &FolderSpec, remote: &str) -> Result<(), DriveError> {
    let place = locate(folder, remote, false)?;
    writable(folder, &format!("can't remove {remote}"))?;
    if place.root {
        return Err(DriveError::new("EACCES", format!("{}: the shared folder itself stays", folder.name)));
    }
    // Directories only when empty, as on Windows (ENOTEMPTY otherwise); the server removes contents first. A link is
    // removed itself, whatever it points to.
    if std::fs::symlink_metadata(&place.local).map_err(io(remote))?.is_dir() {
        std::fs::remove_dir(&place.local).map_err(io(remote))
    } else {
        std::fs::remove_file(&place.local).map_err(io(remote))
    }
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(not(unix))]
fn same_file(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool { false }

fn rename(folder: &FolderSpec, from: &str, to: &str, replace: bool) -> Result<(), DriveError> {
    let source = locate(folder, from, false)?;
    let target = locate(folder, to, false)?;
    writable(folder, &format!("can't rename {from}"))?;
    if source.root || target.root {
        return Err(DriveError::new("EACCES", format!("{}: the shared folder itself stays", folder.name)));
    }
    let moving = std::fs::symlink_metadata(&source.local).map_err(io(from))?;
    // Something else in the way: only when asked to replace it. The same file under another spelling of its name (a
    // case-insensitive file system) is a plain rename.
    if let Ok(existing) = std::fs::symlink_metadata(&target.local) {
        if !same_file(&existing, &moving) {
            if !replace {
                return Err(DriveError::new("EEXIST", format!("{to} exists")));
            }
            if existing.is_dir() || moving.is_dir() {
                return Err(DriveError::new("EACCES", format!("{to}: folders aren't replaced")));
            }
        }
    }
    std::fs::rename(&source.local, &target.local).map_err(io(to))
}

#[derive(Serialize, Debug)]
pub struct Volume {
    total: f64,
    free: f64,
    label: String,
}

fn volume(folder: &FolderSpec) -> Result<Volume, DriveError> {
    let root = std::fs::canonicalize(&folder.path).map_err(io(&folder.name))?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(root.as_os_str().as_bytes()).map_err(|_| DriveError::new("EINVAL", "a path with a NUL"))?;
        // SAFETY: statvfs fills the struct it is given for the zero-terminated path.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut s) } != 0 {
            return Err(io(&folder.name)(std::io::Error::last_os_error()));
        }
        let block = s.f_frsize as f64;
        Ok(Volume { total: block * s.f_blocks as f64, free: block * s.f_bavail as f64, label: folder.name.clone() })
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Ok(Volume { total: 0.0, free: 0.0, label: folder.name.clone() })
    }
}

// ---- commands ------------------------------------------------------------------------------------------------------
// `drives`: the id rdp_drives_open gave out; `device`: a drive's RDPDR id; `path`: the server's, relative to the drive.

/// Shares these folders for one connection; returns the id the other commands take.
#[tauri::command]
pub fn rdp_drives_open(webview: tauri::Webview, table: State<'_, Drives>, folders: Vec<FolderSpec>) -> Result<u32, String> {
    let mut shared = HashMap::new();
    for spec in folders {
        if !Path::new(&spec.path).is_absolute() || spec.id == 0 {
            return Err(format!("EINVAL: not a folder to share: {}", spec.path));
        }
        shared.insert(spec.id, Arc::new(Drive { spec, lock: tokio::sync::Mutex::new(()) }));
    }
    let id = table.next.fetch_add(1, Ordering::Relaxed) + 1;
    let session = DriveSession { owner: webview.label().to_owned(), drives: shared, files: Mutex::default(), next_handle: AtomicU32::new(0) };
    table.sessions.lock().unwrap().insert(id, Arc::new(session));
    Ok(id)
}

/// The connection ended: whatever the server left open is closed.
#[tauri::command]
pub fn rdp_drives_close(table: State<'_, Drives>, drives: u32) {
    table.sessions.lock().unwrap().remove(&drives);
}

fn drive_of(table: &Drives, drives: u32, device: u32) -> Result<(Arc<DriveSession>, Arc<Drive>), String> {
    let session = table.session(drives)?;
    let drive = session.drive(device)?;
    Ok((session, drive))
}

#[tauri::command]
pub async fn rdp_drive_stat(table: State<'_, Drives>, drives: u32, device: u32, path: String) -> Result<Option<Stat>, String> {
    let (_, drive) = drive_of(&table, drives, device)?;
    let spec = drive.spec.clone();
    on_drive(&drive, move || stat(&spec, &path)).await
}

#[tauri::command]
pub async fn rdp_drive_list(table: State<'_, Drives>, drives: u32, device: u32, path: String) -> Result<Vec<String>, String> {
    let (_, drive) = drive_of(&table, drives, device)?;
    let spec = drive.spec.clone();
    on_drive(&drive, move || list(&spec, &path)).await
}

#[tauri::command]
pub async fn rdp_drive_open(table: State<'_, Drives>, drives: u32, device: u32, path: String, mode: OpenMode) -> Result<u32, String> {
    let (session, drive) = drive_of(&table, drives, device)?;
    let spec = drive.spec.clone();
    let file = on_drive(&drive, move || open(&spec, &path, mode)).await?;
    let handle = session.next_handle.fetch_add(1, Ordering::Relaxed) + 1;
    session.files.lock().unwrap().insert(handle, OpenFile { file: Arc::new(file), device });
    Ok(handle)
}

#[tauri::command]
pub async fn rdp_drive_read(table: State<'_, Drives>, drives: u32, handle: u32, offset: u64, length: usize) -> Result<Response, String> {
    let (file, drive) = table.session(drives)?.file(handle)?;
    let data = on_drive(&drive, move || read(&file, offset, length)).await?;
    Ok(Response::new(data))
}

/// The bytes as the request's body; `drives`, `handle` and `offset` in its headers.
#[tauri::command]
pub async fn rdp_drive_write(table: State<'_, Drives>, request: Request<'_>) -> Result<usize, String> {
    let header = |name: &str| -> Result<u64, String> {
        request.headers().get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).ok_or_else(|| format!("EINVAL: no {name}"))
    };
    let (drives, handle, offset) = (header("drives")? as u32, header("handle")? as u32, header("offset")?);
    let tauri::ipc::InvokeBody::Raw(data) = request.body() else { return Err("EINVAL: expected the bytes as the body".into()) };
    let data = data.clone();
    let (file, drive) = table.session(drives)?.file(handle)?;
    let spec = drive.spec.clone();
    on_drive(&drive, move || {
        writable(&spec, "can't write")?;
        write(&file, offset, &data)
    })
    .await
}

#[tauri::command]
pub async fn rdp_drive_truncate(table: State<'_, Drives>, drives: u32, handle: u32, size: u64) -> Result<(), String> {
    let (file, drive) = table.session(drives)?.file(handle)?;
    let spec = drive.spec.clone();
    on_drive(&drive, move || {
        writable(&spec, "can't resize")?;
        file.set_len(size).map_err(io("resize"))
    })
    .await
}

#[tauri::command]
pub async fn rdp_drive_close(table: State<'_, Drives>, drives: u32, handle: u32) -> Result<(), String> {
    let session = table.session(drives)?;
    let open = session.files.lock().unwrap().remove(&handle);
    if let Some(open) = open {
        // Closed on the drive's turn, after whatever it is doing with the file.
        let drive = session.drive(open.device)?;
        on_drive(&drive, move || {
            drop(open);
            Ok(())
        })
        .await?;
    }
    Ok(())
}

#[tauri::command]
pub async fn rdp_drive_mkdir(table: State<'_, Drives>, drives: u32, device: u32, path: String) -> Result<(), String> {
    let (_, drive) = drive_of(&table, drives, device)?;
    let spec = drive.spec.clone();
    on_drive(&drive, move || mkdir(&spec, &path)).await
}

#[tauri::command]
pub async fn rdp_drive_remove(table: State<'_, Drives>, drives: u32, device: u32, path: String) -> Result<(), String> {
    let (_, drive) = drive_of(&table, drives, device)?;
    let spec = drive.spec.clone();
    on_drive(&drive, move || remove(&spec, &path)).await
}

#[tauri::command]
pub async fn rdp_drive_rename(table: State<'_, Drives>, drives: u32, device: u32, from: String, to: String, replace: bool) -> Result<(), String> {
    let (_, drive) = drive_of(&table, drives, device)?;
    let spec = drive.spec.clone();
    on_drive(&drive, move || rename(&spec, &from, &to, replace)).await
}

#[tauri::command]
pub async fn rdp_drive_volume(table: State<'_, Drives>, drives: u32, device: u32) -> Result<Volume, String> {
    let (_, drive) = drive_of(&table, drives, device)?;
    let spec = drive.spec.clone();
    on_drive(&drive, move || volume(&spec)).await
}

#[cfg(test)]
#[path = "drives_tests.rs"]
mod tests;
