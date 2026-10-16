//! Child processes for the page's `child_process` module (tauri-host/src/node/child_process.ts): spawned with pipes,
//! their stdout and stderr streamed to the page as raw bytes, stdin written from it. Everything comes on one channel, so
//! it arrives in order (two channels don't keep order between them: output would come after the exit): the first byte
//! says what it is, 1 stdout, 2 stderr, 3 the exit (then JSON {code, signal}), 4 the close (both streams done).
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::ipc::{Channel, InvokeBody, InvokeResponseBody, Request};
use tauri::State;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};

const STDOUT: u8 = 1;
const STDERR: u8 = 2;
const EXIT: u8 = 3;
const CLOSE: u8 = 4;

struct Proc {
    pid: u32,
    owner: String,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
}

#[derive(Default)]
pub struct Processes {
    next: AtomicU32,
    running: Mutex<HashMap<u32, Arc<Proc>>>,
}

impl Drop for Proc {
    fn drop(&mut self) { for task in self.tasks.get_mut().unwrap().drain(..) { task.abort(); } }
}
impl Processes {
    pub fn reset(&self, owner: &str) {
        self.running.lock().unwrap().retain(|_, p| {
            if p.owner != owner { return true }
            for task in p.tasks.lock().unwrap().drain(..) { task.abort(); }
            false
        });
    }
    fn get(&self, id: u32) -> Result<Arc<Proc>, String> {
        self.running.lock().unwrap().get(&id).cloned().ok_or_else(|| format!("no process {id}"))
    }
}

fn signal_name(status: &std::process::ExitStatus) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        return status.signal().map(|s| match s {
            libc::SIGTERM => "SIGTERM".to_string(),
            libc::SIGKILL => "SIGKILL".to_string(),
            libc::SIGINT => "SIGINT".to_string(),
            libc::SIGHUP => "SIGHUP".to_string(),
            libc::SIGPIPE => "SIGPIPE".to_string(),
            other => format!("SIG{other}"),
        });
    }
    #[allow(unreachable_code)]
    None
}

#[derive(Serialize)]
pub struct Spawned {
    id: u32,
    pid: u32,
}

/// `env`: the whole environment (the page passes process.env merged with the caller's). `stdin`: false for 'ignore'.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn proc_spawn(
    processes: State<'_, Processes>,
    webview: tauri::Webview,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
    stdin: bool,
    output: Channel<InvokeResponseBody>,
) -> Result<Spawned, String> {
    let mut running = processes.running.lock().unwrap();
    let mut command = Command::new(&program);
    command
        .args(&args)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &cwd {
        command.current_dir(cwd);
    }
    if let Some(env) = &env {
        command.env_clear().envs(env);
    }
    let mut child = command.spawn().map_err(|e| {
        let code = match e.kind() {
            std::io::ErrorKind::NotFound => "ENOENT",
            std::io::ErrorKind::PermissionDenied => "EACCES",
            _ => "EIO",
        };
        format!("{code}: spawn {program}: {e}")
    })?;
    let pid = child.id().unwrap_or(0);
    let id = processes.next.fetch_add(1, Ordering::Relaxed) + 1;
    let proc = Arc::new(Proc { pid, owner: webview.label().into(), tasks: Mutex::new(Vec::new()), stdin: tokio::sync::Mutex::new(child.stdin.take()) });


    let pump = |mut stream: Box<dyn tokio::io::AsyncRead + Unpin + Send>, tag: u8, output: Channel<InvokeResponseBody>| {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stream.read(&mut buf[1..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf[0] = tag;
                        if output.send(InvokeResponseBody::Raw(buf[..=n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        })
    };
    let out = pump(Box::new(child.stdout.take().unwrap()), STDOUT, output.clone());
    let err = pump(Box::new(child.stderr.take().unwrap()), STDERR, output.clone());
    proc.tasks.lock().unwrap().extend([out.abort_handle(), err.abort_handle()]);
    let waiter = tokio::spawn(async move {
        let status = child.wait().await;
        let _ = out.await;
        let _ = err.await;
        let exit = match status {
            Ok(status) => serde_json::json!({ "code": status.code(), "signal": signal_name(&status) }),
            Err(_) => serde_json::json!({ "code": -1, "signal": null }),
        };
        let mut message = vec![EXIT];
        message.extend(exit.to_string().into_bytes());
        let _ = output.send(InvokeResponseBody::Raw(message));
        let _ = output.send(InvokeResponseBody::Raw(vec![CLOSE]));
    });
    proc.tasks.lock().unwrap().push(waiter.abort_handle());
    running.insert(id, proc);
    Ok(Spawned { id, pid })
}

/// The body is the bytes; header `process`.
#[tauri::command]
pub async fn proc_write(processes: State<'_, Processes>, request: Request<'_>) -> Result<(), String> {
    let id: u32 = request.headers().get("process").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).ok_or("no process")?;
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw bytes".into());
    };
    let proc = processes.get(id)?;
    let mut stdin = proc.stdin.lock().await;
    match stdin.as_mut() {
        Some(pipe) => pipe.write_all(bytes).await.map_err(|e| format!("EPIPE: {e}")),
        None => Err("EPIPE: stdin is closed".into()),
    }
}

#[tauri::command]
pub async fn proc_end_stdin(processes: State<'_, Processes>, id: u32) -> Result<(), String> {
    let proc = processes.get(id)?;
    let pipe = proc.stdin.lock().await.take();
    if let Some(mut pipe) = pipe {
        let _ = pipe.shutdown().await;
    }
    Ok(())
}

#[tauri::command]
pub fn proc_kill(processes: State<'_, Processes>, id: u32, signal: Option<String>) -> Result<(), String> {
    let proc = processes.get(id)?;
    #[cfg(unix)]
    {
        let sig = match signal.as_deref().unwrap_or("SIGTERM").trim_start_matches("SIG") {
            "KILL" => libc::SIGKILL,
            "INT" => libc::SIGINT,
            "HUP" => libc::SIGHUP,
            "QUIT" => libc::SIGQUIT,
            _ => libc::SIGTERM,
        };
        if proc.pid > 0 {
            unsafe { libc::kill(proc.pid as i32, sig) };
        }
    }
    #[cfg(not(unix))]
    let _ = (proc, signal);
    Ok(())
}

/// Forgets a finished process (after its close event).
#[tauri::command]
pub fn proc_release(processes: State<'_, Processes>, id: u32) {
    processes.running.lock().unwrap().remove(&id);
}
