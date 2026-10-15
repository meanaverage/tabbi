//! Local terminals: a PTY per shell (portable-pty), its output streamed to the page as raw bytes over a Tauri channel.
//!
//! Like the Electron build's main process: output is sent in whole UTF-8 characters, and at most MAX_UNACKED bytes are
//! in flight until the page acknowledges them (then the reader stops reading, which blocks the program writing to the
//! terminal instead of piling its output up in memory). PTYs outlive a page reload; `pty_attach` gives one to the new
//! page.
//!
//! macOS hands PTY output over about 1 KB at a time, and each message to the page costs a round trip through the
//! webview, so a reader thread gathers output and a sender thread passes it on in batches: at once after a quiet
//! moment (a keystroke's echo), and while output keeps coming, at most every BATCH_INTERVAL.
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody, Request};
use tauri::State;

const READ_CHUNK: usize = 64 * 1024;
const MAX_UNACKED: usize = 512 * 1024;
const BATCH_INTERVAL: Duration = Duration::from_millis(4);
/// A short reply (an echo, a prompt) waits less when it comes right after another one.
const SMALL_BATCH_INTERVAL: Duration = Duration::from_millis(1);
const SMALL_BATCH: usize = 256;
const MAX_BATCH: usize = 256 * 1024;
/// After the shell exits, how long nothing more has to come before the terminal counts as closed although a program
/// the shell left running still holds it open (node-pty waits 200 ms).
const CLOSE_AFTER_QUIET: Duration = Duration::from_millis(300);

// One channel carries everything, so the exit can't overtake the output before it: each message starts with a tag,
// then the bytes (DATA), a little-endian u32 exit code (EXIT), or nothing (CLOSE). plugin/src/pty.ts reads them.
const DATA: u8 = 1;
const EXIT: u8 = 2;
const CLOSE: u8 = 3;

fn message(tag: u8, bytes: &[u8]) -> InvokeResponseBody {
    let mut message = Vec::with_capacity(bytes.len() + 1);
    message.push(tag);
    message.extend_from_slice(bytes);
    InvokeResponseBody::Raw(message)
}

/// Output read from the PTY and not sent yet.
struct Pending {
    state: Mutex<Gathered>,
    ready: Condvar,
}

#[derive(Default)]
struct Gathered {
    bytes: Vec<u8>,
    /// The PTY has nothing more to read.
    finished: bool,
    /// Since when the reader has been waiting for the PTY to give it something.
    reading_since: Option<Instant>,
    /// The sender has taken a batch and not sent it yet.
    sending: bool,
}

impl Pending {
    fn reading(&self) {
        self.state.lock().unwrap().reading_since = Some(Instant::now());
    }

    fn push(&self, data: &[u8]) {
        let mut state = self.state.lock().unwrap();
        state.bytes.extend_from_slice(data);
        state.reading_since = None;
        self.ready.notify_all();
    }

    /// Everything the PTY gave has been sent, and it has given nothing for `quiet`.
    fn quiet_for(&self, quiet: Duration) -> bool {
        let state = self.state.lock().unwrap();
        state.bytes.is_empty() && !state.sending && state.reading_since.is_some_and(|t| t.elapsed() >= quiet)
    }

    fn finish(&self) {
        self.state.lock().unwrap().finished = true;
        self.ready.notify_all();
    }

    /// Waits for output, then for the batch to be due, and takes it; also says whether the PTY has finished.
    fn take(&self, last_send: Instant) -> (Vec<u8>, bool) {
        let mut state = self.state.lock().unwrap();
        state.sending = false;
        while state.bytes.is_empty() && !state.finished {
            state = self.ready.wait(state).unwrap();
        }
        loop {
            let interval = if state.bytes.len() <= SMALL_BATCH { SMALL_BATCH_INTERVAL } else { BATCH_INTERVAL };
            let due = last_send + interval;
            let now = Instant::now();
            if state.finished || now >= due || state.bytes.len() >= MAX_BATCH {
                break;
            }
            state = self.ready.wait_timeout(state, due - now).unwrap().0;
        }
        state.sending = !state.bytes.is_empty();
        (std::mem::take(&mut state.bytes), state.finished)
    }
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
    output: Mutex<Option<Channel<InvokeResponseBody>>>,
    flow: Flow,
    /// The shell has exited (its process is gone: no more signals or resizes).
    exited: AtomicBool,
    /// The exit has been sent: the page has everything.
    closed: AtomicBool,
    /// The page is done with it.
    released: AtomicBool,
}

#[derive(Default)]
pub struct Ptys(Mutex<HashMap<String, Arc<Pty>>>);

impl Ptys {
    fn get(&self, id: &str) -> Result<Arc<Pty>, String> {
        self.0.lock().unwrap().get(id).cloned().ok_or_else(|| format!("no terminal {id}"))
    }
}

impl Pty {
    fn send_data(&self, bytes: &[u8]) {
        // (sent while holding the channel, so nothing goes out after the exit: see send_exit)
        let output = self.output.lock().unwrap();
        if let Some(channel) = &*output {
            // Counted before it's sent: the acknowledgement can come back before send() returns.
            *self.flow.unacked.lock().unwrap() += bytes.len();
            if channel.send(message(DATA, bytes)).is_err() {
                self.ack(bytes.len());
            }
        }
    }

    fn send_exit(&self, code: u32) {
        let mut output = self.output.lock().unwrap();
        if let Some(channel) = output.take() {
            let _ = channel.send(message(EXIT, &code.to_le_bytes()));
            let _ = channel.send(message(CLOSE, &[]));
        }
        self.closed.store(true, Ordering::Relaxed);
        self.flow.wake.notify_all();
    }

    fn ack(&self, length: usize) {
        let mut unacked = self.flow.unacked.lock().unwrap();
        *unacked = unacked.saturating_sub(length);
        self.flow.wake.notify_all();
    }

    /// Blocks the reader while too much output is unacknowledged (or nobody is attached to take it).
    fn wait_for_room(&self) {
        let mut unacked = self.flow.unacked.lock().unwrap();
        while *unacked > MAX_UNACKED && !self.closed.load(Ordering::Relaxed) && !self.released.load(Ordering::Relaxed) {
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
    output: Channel<InvokeResponseBody>,
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
        output: Mutex::new(Some(output)),
        flow: Flow { unacked: Mutex::new(0), wake: Condvar::new() },
        exited: AtomicBool::new(false),
        closed: AtomicBool::new(false),
        released: AtomicBool::new(false),
    });
    ptys.0.lock().unwrap().insert(id.clone(), pty.clone());

    let pending = Arc::new(Pending { state: Mutex::new(Gathered::default()), ready: Condvar::new() });
    let drained = Arc::new((Mutex::new(false), Condvar::new()));
    {
        let pty = pty.clone();
        let pending = pending.clone();
        std::thread::Builder::new().name(format!("pty-read-{pid}")).spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                pty.wait_for_room();
                pending.reading();
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => pending.push(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            pending.finish();
        }).map_err(|e| e.to_string())?;
    }
    {
        let pty = pty.clone();
        let pending = pending.clone();
        let drained = drained.clone();
        std::thread::Builder::new().name(format!("pty-send-{pid}")).spawn(move || {
            // A character cut in two at the end of a batch waits for the rest of it.
            let mut carry: Vec<u8> = Vec::new();
            let mut last_send = Instant::now() - BATCH_INTERVAL;
            loop {
                let (mut batch, finished) = pending.take(last_send);
                if !carry.is_empty() {
                    carry.append(&mut batch);
                    batch = std::mem::take(&mut carry);
                }
                let cut = if finished { batch.len() } else { utf8_boundary(&batch) };
                carry = batch.split_off(cut);
                if !batch.is_empty() {
                    pty.send_data(&batch);
                    last_send = Instant::now();
                }
                if finished {
                    break;
                }
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
            pty.exited.store(true, Ordering::Relaxed);
            // What the shell wrote just before exiting is still on its way: the exit goes after all of it, once the
            // PTY has nothing more to read. A program the shell left running can keep the PTY open, though: then the
            // exit goes once the PTY has given nothing for a moment (everything before it has been sent by then).
            let (lock, cvar) = &*drained;
            let mut done = lock.lock().unwrap();
            while !*done && !pty.released.load(Ordering::Relaxed) && !pending.quiet_for(CLOSE_AFTER_QUIET) {
                done = cvar.wait_timeout(done, Duration::from_millis(100)).unwrap().0;
            }
            drop(done);
            pty.send_exit(code);
        }).map_err(|e| e.to_string())?;
    }
    Ok(Spawned { id, pid })
}

/// Hands a running PTY to a (reloaded) page: output goes to the new channel from now on.
#[tauri::command]
pub fn pty_attach(
    ptys: State<'_, Ptys>,
    id: String,
    output: Channel<InvokeResponseBody>,
) -> Result<Option<u32>, String> {
    let Ok(pty) = ptys.get(&id) else { return Ok(None) };
    let mut current = pty.output.lock().unwrap();
    if pty.closed.load(Ordering::Relaxed) {
        return Ok(None);
    }
    *current = Some(output);
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
        pty.ack(length);
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
        pty.released.store(true, Ordering::Relaxed);
        *pty.output.lock().unwrap() = None;
        pty.flow.wake.notify_all();
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
