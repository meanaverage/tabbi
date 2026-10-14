//! Local terminals: a PTY per shell (portable-pty), its output streamed to the page as raw bytes over a Tauri channel.
//!
//! Like the Electron build's main process: output is sent in whole UTF-8 characters, and at most MAX_UNACKED bytes are
//! in flight until the page acknowledges them (then the reader stops reading, which blocks the program writing to the
//! terminal instead of piling its output up in memory). PTYs outlive a page reload; `pty_attach` gives one to the new
//! page.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody, Request};
use tauri::State;

const READ_CHUNK: usize = 64 * 1024;
const MAX_UNACKED: usize = 512 * 1024;

#[derive(Clone, Serialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum PtyEvent {
    Exit { code: u32 },
    Close,
}

struct Output {
    data: Option<Channel<InvokeResponseBody>>,
    events: Option<Channel<PtyEvent>>,
}

struct Flow {
    unacked: Mutex<usize>,
    wake: Condvar,
}

pub struct Pty {
    pid: u32,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output: Mutex<Output>,
    flow: Flow,
    exited: AtomicBool,
}

#[derive(Default)]
pub struct Ptys(Mutex<HashMap<String, Arc<Pty>>>);

impl Ptys {
    fn get(&self, id: &str) -> Result<Arc<Pty>, String> {
        self.0.lock().unwrap().get(id).cloned().ok_or_else(|| format!("no terminal {id}"))
    }
}

impl Pty {
    fn send_data(&self, bytes: Vec<u8>) {
        let len = bytes.len();
        let channel = self.output.lock().unwrap().data.clone();
        if let Some(channel) = channel {
            if channel.send(InvokeResponseBody::Raw(bytes)).is_ok() {
                *self.flow.unacked.lock().unwrap() += len;
            }
        }
    }

    fn send_event(&self, event: PtyEvent) {
        let channel = self.output.lock().unwrap().events.clone();
        if let Some(channel) = channel {
            let _ = channel.send(event);
        }
    }

    /// Blocks the reader while too much output is unacknowledged (or nobody is attached to take it).
    fn wait_for_room(&self) {
        let mut unacked = self.flow.unacked.lock().unwrap();
        while *unacked > MAX_UNACKED && !self.exited.load(Ordering::Relaxed) {
            let (guard, _) = self.flow.wake.wait_timeout(unacked, Duration::from_millis(500)).unwrap();
            unacked = guard;
        }
    }
}

/// The length of `buf`'s longest prefix that doesn't end inside a UTF-8 character.
fn utf8_boundary(buf: &[u8]) -> usize {
    let len = buf.len();
    for back in 1..=3.min(len) {
        let b = buf[len - back];
        if b & 0b1100_0000 == 0b1000_0000 {
            continue; // continuation byte: look further back for the lead byte
        }
        let needed = if b & 0b1110_0000 == 0b1100_0000 {
            2
        } else if b & 0b1111_0000 == 0b1110_0000 {
            3
        } else if b & 0b1111_1000 == 0b1111_0000 {
            4
        } else {
            1
        };
        return if needed > back { len - back } else { len };
    }
    len
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spawned {
    id: String,
    pid: u32,
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub fn pty_spawn(
    ptys: State<'_, Ptys>,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: HashMap<String, String>,
    cols: u16,
    rows: u16,
    data: Channel<InvokeResponseBody>,
    events: Channel<PtyEvent>,
) -> Result<Spawned, String> {
    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;
    let mut cmd = CommandBuilder::new(&command);
    cmd.args(&args);
    cmd.env_clear();
    for (k, v) in &env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &cwd {
        cmd.cwd(cwd);
    }
    let mut child = pair.slave.spawn_command(cmd).map_err(|e| match &cwd {
        Some(cwd) => format!("{e} (working directory: {cwd})"),
        None => e.to_string(),
    })?;
    // The shell has its end now; ours closing makes reads end once the shell and its children are gone.
    drop(pair.slave);

    let pid = child.process_id().unwrap_or(0);
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let id = uuid::Uuid::new_v4().to_string();
    let pty = Arc::new(Pty {
        pid,
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        killer: Mutex::new(child.clone_killer()),
        output: Mutex::new(Output { data: Some(data), events: Some(events) }),
        flow: Flow { unacked: Mutex::new(0), wake: Condvar::new() },
        exited: AtomicBool::new(false),
    });
    ptys.0.lock().unwrap().insert(id.clone(), pty.clone());

    let drained = Arc::new((Mutex::new(false), Condvar::new()));
    {
        let pty = pty.clone();
        let drained = drained.clone();
        std::thread::Builder::new().name(format!("pty-read-{pid}")).spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            let mut pending: Vec<u8> = Vec::new();
            loop {
                pty.wait_for_room();
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        pending.extend_from_slice(&buf[..n]);
                        let cut = utf8_boundary(&pending);
                        if cut > 0 {
                            let rest = pending.split_off(cut);
                            pty.send_data(std::mem::replace(&mut pending, rest));
                        }
                    }
                }
            }
            if !pending.is_empty() {
                pty.send_data(pending);
            }
            let (lock, cvar) = &*drained;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }).map_err(|e| e.to_string())?;
    }
    {
        let pty = pty.clone();
        let ptys = ptys.0.lock().unwrap().len(); // (for the thread name only)
        std::thread::Builder::new().name(format!("pty-wait-{pid}-{ptys}")).spawn(move || {
            let code = child.wait().map(|s| s.exit_code()).unwrap_or(1);
            // Output the shell wrote just before exiting is still being read: give it a moment to arrive first.
            let (lock, cvar) = &*drained;
            let _ = cvar.wait_timeout_while(lock.lock().unwrap(), Duration::from_millis(300), |done| !*done);
            pty.exited.store(true, Ordering::Relaxed);
            pty.flow.wake.notify_all();
            pty.send_event(PtyEvent::Exit { code });
            pty.send_event(PtyEvent::Close);
        }).map_err(|e| e.to_string())?;
    }
    Ok(Spawned { id, pid })
}

/// Hands a running PTY to a (reloaded) page: output goes to the new channels from now on.
#[tauri::command]
pub fn pty_attach(
    ptys: State<'_, Ptys>,
    id: String,
    data: Channel<InvokeResponseBody>,
    events: Channel<PtyEvent>,
) -> Result<Option<u32>, String> {
    let Ok(pty) = ptys.get(&id) else { return Ok(None) };
    if pty.exited.load(Ordering::Relaxed) {
        return Ok(None);
    }
    *pty.output.lock().unwrap() = Output { data: Some(data), events: Some(events) };
    *pty.flow.unacked.lock().unwrap() = 0;
    pty.flow.wake.notify_all();
    Ok(Some(pty.pid))
}

/// Keystrokes and pastes: the raw request body is the bytes, the `pty-id` header says which terminal.
#[tauri::command]
pub fn pty_write(ptys: State<'_, Ptys>, request: Request<'_>) -> Result<(), String> {
    let id = request.headers().get("pty-id").and_then(|v| v.to_str().ok()).ok_or("no pty-id")?;
    let pty = ptys.get(id)?;
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw bytes".into());
    };
    let mut writer = pty.writer.lock().unwrap();
    writer.write_all(bytes).and_then(|_| writer.flush()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn pty_ack(ptys: State<'_, Ptys>, id: String, length: usize) {
    if let Ok(pty) = ptys.get(&id) {
        let mut unacked = pty.flow.unacked.lock().unwrap();
        *unacked = unacked.saturating_sub(length);
        pty.flow.wake.notify_all();
    }
}

#[tauri::command]
pub fn pty_resize(ptys: State<'_, Ptys>, id: String, cols: u16, rows: u16) -> Result<(), String> {
    let pty = ptys.get(&id)?;
    if pty.exited.load(Ordering::Relaxed) {
        return Ok(());
    }
    let master = pty.master.lock().unwrap();
    master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).map_err(|e| e.to_string())
}

pub fn signal_number(name: Option<&str>) -> i32 {
    match name.unwrap_or("SIGHUP").trim_start_matches("SIG") {
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        other => other.parse().unwrap_or(libc::SIGHUP),
    }
}

#[tauri::command]
pub fn pty_kill(ptys: State<'_, Ptys>, id: String, signal: Option<String>) -> Result<(), String> {
    let pty = ptys.get(&id)?;
    if pty.exited.load(Ordering::Relaxed) {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if pty.pid > 0 {
            unsafe { libc::kill(pty.pid as i32, signal_number(signal.as_deref())) };
            return Ok(());
        }
    }
    let _ = signal;
    let result = pty.killer.lock().unwrap().kill().map_err(|e| e.to_string());
    result
}

/// Forgets a PTY whose page is done with it (after its close event).
#[tauri::command]
pub fn pty_release(ptys: State<'_, Ptys>, id: String) {
    let removed = ptys.0.lock().unwrap().remove(&id);
    if let Some(pty) = removed {
        *pty.output.lock().unwrap() = Output { data: None, events: None };
    }
}

#[derive(Serialize)]
pub struct ChildProcess {
    pid: u32,
    ppid: u32,
    command: String,
}

#[tauri::command]
pub fn process_children(pid: u32) -> Vec<ChildProcess> {
    #[cfg(target_os = "macos")]
    {
        use libproc::processes::{pids_by_type, ProcFilter};
        return pids_by_type(ProcFilter::ByParentProcess { ppid: pid })
            .unwrap_or_default()
            .into_iter()
            .filter(|p| *p != 0)
            .map(|p| ChildProcess {
                pid: p,
                ppid: pid,
                command: libproc::proc_pid::name(p as i32).unwrap_or_default(),
            })
            .collect();
    }
    #[allow(unreachable_code)]
    {
        let _ = pid;
        Vec::new()
    }
}

#[tauri::command]
pub fn process_cwd(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        return libproc::proc_pid::pidcwd(pid as i32).ok().map(|p| p.to_string_lossy().into_owned());
    }
    #[allow(unreachable_code)]
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|p| p.to_string_lossy().into_owned())
}

/// The user's login shell (what Terminal.app would start).
#[tauri::command]
pub fn default_shell() -> String {
    #[cfg(unix)]
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            if let Ok(s) = std::ffi::CStr::from_ptr((*pw).pw_shell).to_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into())
}
