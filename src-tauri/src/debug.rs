//! For testing. With `TABBI_DEBUG_PORT` set, the app runs JavaScript sent to 127.0.0.1:<port> in its page and
//! sends back the result: first line the token from `<data dir>/debug-token`, then one JSON string of code per line.
//!
//! Such a test instance stays in the background: no Dock icon, its window behind the others, and it never takes the
//! keyboard (the person using the Mac keeps typing elsewhere). `TABBI_FOREGROUND=1` brings it forward.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// A test instance's own web storage (Tabby keeps its open tabs there), so test runs don't reopen each other's tabs
/// or the user's. One per TABBI_DATA_DIR, stable across builds: FNV-1a over the path, from two starting points.
pub fn web_store() -> Option<[u8; 16]> {
    let dir = std::path::PathBuf::from(std::env::var_os("TABBI_DATA_DIR")?);
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    let fnv = |seed: u64| dir.as_os_str().as_encoded_bytes().iter().fold(seed, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3));
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&fnv(0xcbf29ce484222325).to_be_bytes());
    id[8..].copy_from_slice(&fnv(0x84222325cbf29ce4).to_be_bytes());
    Some(id)
}

pub fn background() -> bool {
    std::env::var_os("TABBI_DEBUG_PORT").is_some() && std::env::var_os("TABBI_FOREGROUND").is_none()
}

/// Sends a test window behind every other window without activating the app. WebKit would treat the covered window
/// as hidden and stop rendering it, so that is turned off first.
pub fn keep_behind(window: &tauri::WebviewWindow) {
    #[cfg(target_os = "macos")]
    {
        let _ = window.with_webview(|webview| unsafe {
            let view = webview.inner() as *mut objc2::runtime::AnyObject;
            let _: () = objc2::msg_send![view, _setWindowOcclusionDetectionEnabled: false];
        });
        if let Ok(ns_window) = window.ns_window() {
            let ns_window = ns_window as usize;
            let _ = window.run_on_main_thread(move || unsafe {
                let _: () = objc2::msg_send![ns_window as *mut objc2::runtime::AnyObject, orderBack: std::ptr::null_mut::<objc2::runtime::AnyObject>()];
            });
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = window;
}

pub fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    // Something focused a test instance anyway: hand the keyboard straight back.
    #[cfg(target_os = "macos")]
    if matches!(event, tauri::WindowEvent::Focused(true)) && background() {
        let _ = window.run_on_main_thread(|| unsafe {
            use objc2::MainThreadMarker;
            use objc2_app_kit::NSApplication;
            let app = NSApplication::sharedApplication(MainThreadMarker::new_unchecked());
            app.hide(None);
            app.unhideWithoutActivation();
        });
        if let Some(webview) = window.get_webview_window(window.label()) {
            keep_behind(&webview);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (window, event);
}

#[derive(Default)]
pub struct DebugState {
    next: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>,
}

pub fn start(app: &AppHandle) {
    let Some(port) = std::env::var("TABBI_DEBUG_PORT").ok().and_then(|p| p.parse::<u16>().ok()) else {
        return;
    };
    app.manage(DebugState::default());
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let token_path = match crate::config::data_dir(app) {
        Ok(dir) => dir.join("debug-token"),
        Err(e) => {
            eprintln!("debug: no data folder, not listening: {e}");
            return;
        }
    };
    let written = {
        use std::io::Write;
        let _ = std::fs::remove_file(&token_path);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&token_path).and_then(|mut f| f.write_all(format!("{token}\n").as_bytes()))
    };
    if let Err(e) = written {
        eprintln!("debug: could not write {}, not listening: {e}", token_path.display());
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("debug: could not listen on {port}: {e}");
                return;
            }
        };
        eprintln!("debug: listening on 127.0.0.1:{port}");
        while let Ok((stream, _)) = listener.accept().await {
            let app = app.clone();
            let token = token.clone();
            tauri::async_runtime::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                match lines.next_line().await {
                    Ok(Some(line)) if line == token => {}
                    _ => return,
                }
                while let Ok(Some(line)) = lines.next_line().await {
                    let reply = match serde_json::from_str::<String>(&line) {
                        Ok(code) => run(&app, code).await,
                        Err(e) => serde_json::json!({ "error": format!("bad request: {e}") }),
                    };
                    let mut out = reply.to_string();
                    out.push('\n');
                    if write.write_all(out.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
}

async fn run(app: &AppHandle, code: String) -> serde_json::Value {
    let Some(window) = app.get_webview_window("main") else {
        return serde_json::json!({ "error": "no window" });
    };
    let state = app.state::<DebugState>();
    let id = state.next.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    state.pending.lock().unwrap().insert(id, tx);
    let script = format!(
        r#"(async () => {{
            const __reply = r => window.__TAURI_INTERNALS__.invoke('debug_reply', {{ id: {id}, reply: r }});
            try {{
                const __body = async () => {{ {code} }};
                const __zone = window.tabby && window.require('@angular/core').NgZone;
                const value = await (__zone ? window.tabby.get(__zone).run(__body) : __body());
                let json; try {{ json = JSON.parse(JSON.stringify(value ?? null)) }} catch (e) {{ json = String(value) }}
                __reply({{ ok: json }});
            }} catch (e) {{ __reply({{ error: String(e && ((e.name || "Error") + ": " + e.message + "\n" + (e.stack || "")) || e) }}); }}
        }})()"#
    );
    if let Err(e) = window.eval(&script) {
        state.pending.lock().unwrap().remove(&id);
        return serde_json::json!({ "error": e.to_string() });
    }
    match tokio::time::timeout(std::time::Duration::from_secs(60), rx).await {
        Ok(Ok(v)) => v,
        _ => {
            state.pending.lock().unwrap().remove(&id);
            serde_json::json!({ "error": "timed out" })
        }
    }
}

#[tauri::command]
pub fn debug_reply(app: AppHandle, id: u64, reply: serde_json::Value) {
    if let Some(state) = app.try_state::<DebugState>() {
        if let Some(tx) = state.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(reply);
        }
    }
}

/// The page's console.warn/error and uncaught errors, for the terminal that started the app.
#[tauri::command]
pub fn console_log(level: String, message: String) {
    eprintln!("page {level}: {message}");
}
