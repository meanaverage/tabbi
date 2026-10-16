//! File handles used by the built-in RDP plugin. Handles belong to one page and close on reload/window close.
use std::{collections::HashMap, fs::{File, OpenOptions}, io::{Read, Seek, SeekFrom, Write}, sync::{Arc, Mutex}};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{json, Value};
use std::os::unix::fs::OpenOptionsExt;

#[derive(Default)]
struct Table { next: u32, generations: HashMap<String, u64>, files: HashMap<u32, (String, File)>, temp_dirs: Vec<(String, tempfile::TempDir)> }
#[derive(Clone, Default)]
pub struct Files(Arc<Mutex<Table>>);
impl Files {
    pub fn reset(&self, owner: &str) {
        let mut t = self.0.lock().unwrap();
        *t.generations.entry(owner.into()).or_default() += 1;
        t.files.retain(|_, (o, _)| o != owner);
        t.temp_dirs.retain(|(o, _)| o != owner);
    }
    fn generation(&self, owner: &str) -> u64 { *self.0.lock().unwrap().generations.entry(owner.into()).or_default() }
    pub fn call(&self, owner: &str, generation: u64, a: Value) -> Result<Value, String> {
        let mut t = self.0.lock().unwrap();
        if t.generations.get(owner).copied().unwrap_or_default() != generation { return Err("EBADF: page has gone".into()) }
        let op = a["op"].as_str().unwrap_or("");
        let path = a["path"].as_str().unwrap_or("");
        let error = |e| crate::fs::node_error(e, op, path);
        match op {
            "open" => {
                if t.files.values().filter(|(o, _)| o == owner).count() >= 128 { return Err("EMFILE: too many open files".into()) }
                let mut options = OpenOptions::new();
                if let Some(flags) = a["flags"].as_str() {
                    match flags {
                        "r" => { options.read(true); },
                        "wx" => { options.write(true).create_new(true); },
                        "w" => { options.write(true).create(true).truncate(true); },
                        "a" => { options.write(true).create(true).append(true); },
                        _ => return Err("EINVAL: unsupported file open flags".into()),
                    }
                } else {
                    // Node flags exposed by our fs shim (Linux values, translated to this platform).
                    let flags = a["flags"].as_u64().ok_or("EINVAL: missing file flags")?;
                    if flags & !(0o400000 | 0o4000) != 0 { return Err("EINVAL: unsupported numeric open flags".into()) }
                    options.read(true).custom_flags(if flags & 0o400000 != 0 { libc::O_NOFOLLOW } else { 0 } | if flags & 0o4000 != 0 { libc::O_NONBLOCK } else { 0 });
                }
                options.mode(a["mode"].as_u64().unwrap_or(0o666) as u32);
                let file = options.open(path).map_err(error)?;
                t.next += 1; let id = t.next;
                t.files.insert(id, (owner.into(), file));
                Ok(json!(id))
            },
            "mkdtemp" => {
                let prefix = std::path::Path::new(path);
                let dir = tempfile::Builder::new().prefix(prefix.file_name().unwrap_or_default().to_string_lossy().as_ref())
                    .tempdir_in(prefix.parent().unwrap_or(std::path::Path::new("."))).map_err(error)?;
                let path = dir.path().to_string_lossy().into_owned();
                t.temp_dirs.push((owner.into(), dir));
                Ok(json!(path))
            },
            "mkdir" => { std::fs::create_dir(path).map_err(error)?; Ok(Value::Null) },
            "copy" => {
                let to = a["to"].as_str().ok_or("EINVAL: no destination")?;
                let mut input = File::open(path).map_err(error)?;
                let mut out = OpenOptions::new().write(true).create_new(true).open(to).map_err(error)?;
                if let Err(e) = std::io::copy(&mut input, &mut out) { drop(out); let _ = std::fs::remove_file(to); return Err(error(e)) }
                Ok(Value::Null)
            },
            _ => {
                let id = a["handle"].as_u64().ok_or("EBADF: missing handle")? as u32;
                let (o, f) = t.files.get_mut(&id).ok_or("EBADF: closed file")?;
                if o != owner { return Err("EBADF: file belongs to another page".into()) }
                match op {
                    "close" => { t.files.remove(&id); Ok(Value::Null) },
                    "stat" => Ok(serde_json::to_value(crate::fs::metadata_stat(&f.metadata().map_err(error)?)).unwrap()),
                    "read" => {
                        let n = a["length"].as_u64().ok_or("EINVAL: missing length")?;
                        if n > 16 * 1024 * 1024 { return Err("EINVAL: read exceeds 16 MiB".into()) }
                        if let Some(offset) = a["offset"].as_u64() { f.seek(SeekFrom::Start(offset)).map_err(error)?; }
                        let mut data = vec![0; n as usize]; let read = f.read(&mut data).map_err(error)?;
                        Ok(json!(STANDARD.encode(&data[..read])))
                    },
                    "write" => {
                        let data = a["data"].as_str().ok_or("EINVAL: missing data")?;
                        if data.len() > 24 * 1024 * 1024 { return Err("EINVAL: write exceeds 16 MiB".into()) }
                        let data = STANDARD.decode(data).map_err(|_| "EINVAL: invalid data")?;
                        f.write_all(&data).map_err(error)?; Ok(Value::Null)
                    },
                    _ => Err("EINVAL: unknown file operation".into()),
                }
            }
        }
    }
    pub fn sync_call(&self, owner: &str, a: Value) -> Result<Value, String> { self.call(owner, self.generation(owner), a) }
}
#[tauri::command]
pub async fn files_call(webview: tauri::Webview, files: tauri::State<'_, Files>, args: Value) -> Result<Value, String> {
    let files = files.inner().clone(); let owner = webview.label().to_string(); let generation = files.generation(&owner);
    tokio::task::spawn_blocking(move || files.call(&owner, generation, args)).await.map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exclusive_create_identity_no_follow_and_owned_cleanup() {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("download");
        let f = Files::default();
        let call = |a| f.sync_call("one", a);
        let id = call(json!({"op":"open","path":path,"flags":"wx","mode":384})).unwrap();
        call(json!({"op":"write","handle":id,"data":STANDARD.encode(b"hello")})).unwrap();
        let stat = call(json!({"op":"stat","handle":id})).unwrap();
        assert_eq!(stat["size"],5); assert!(stat["ino"].as_u64().unwrap() > 0);
        assert!(call(json!({"op":"open","path":path,"flags":"wx"})).unwrap_err().starts_with("EEXIST"));
        assert!(f.sync_call("two",json!({"op":"close","handle":id})).is_err());
        f.reset("two"); assert!(call(json!({"op":"stat","handle":id})).is_ok());
        f.reset("one"); assert!(call(json!({"op":"stat","handle":id})).is_err());
        let link = dir.path().join("link"); std::os::unix::fs::symlink(&path,&link).unwrap();
        assert!(call(json!({"op":"open","path":link,"flags":0o400000})).is_err());
        let copy = dir.path().join("copy");
        call(json!({"op":"copy","path":path,"to":copy})).unwrap();
        assert!(call(json!({"op":"copy","path":path,"to":copy})).is_err());
        assert_eq!(std::fs::read(copy).unwrap(),b"hello");
        assert!(f.call("one",0,json!({"op":"open","path":path,"flags":"r"})).is_err());
    }
}
