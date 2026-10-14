//! Answers for the page's synchronous calls. Tabby's code expects some answers immediately (`fs.statSync`, the
//! clipboard for a paste, `process.kill(pid, 0)`), and a Tauri page can only ask Rust asynchronously. A synchronous
//! XMLHttpRequest to this `tabbysync:` scheme gets the answer while the page waits, so Tabby's code needn't change.
//!
//! POST tabbysync://localhost/<call> with JSON arguments; 200 with a JSON result, or 500 with a Node-style error.
use base64::Engine;
use serde_json::{json, Value};
use tauri::http::{Request, Response};
use tauri::{Manager, Runtime, UriSchemeContext};

pub fn handle<R: Runtime>(ctx: UriSchemeContext<'_, R>, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let call = request.uri().path().trim_start_matches('/').to_string();
    let args: Value = serde_json::from_slice(request.body()).unwrap_or(Value::Null);
    let path = args["path"].as_str().unwrap_or_default();
    let result = match call.as_str() {
        "stat" => crate::fs::stat(path, args["lstat"].as_bool().unwrap_or(false))
            .and_then(|s| serde_json::to_value(s).map_err(|e| e.to_string())),
        "read_file" => std::fs::read(path)
            .map(|b| json!(base64::engine::general_purpose::STANDARD.encode(b)))
            .map_err(|e| crate::fs::node_error(e, "open", path)),
        "realpath" => std::fs::canonicalize(path)
            .map(|p| json!(p.to_string_lossy()))
            .map_err(|e| crate::fs::node_error(e, "realpath", path)),
        "readdir" => std::fs::read_dir(path)
            .map(|dir| json!(dir.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>()))
            .map_err(|e| crate::fs::node_error(e, "scandir", path)),
        "clipboard_read" => Ok(json!(ctx.app_handle().state::<crate::clipboard::Clipboard>().read_text())),
        "process_alive" => Ok(json!(process_alive(args["pid"].as_i64().unwrap_or(0)))),
        "process_kill" => Ok(json!(process_kill(args["pid"].as_i64().unwrap_or(0), args["signal"].as_str().unwrap_or("SIGTERM")))),
        _ => Err(format!("unknown call {call}")),
    };
    let (status, body) = match result {
        Ok(value) => (200, value.to_string()),
        Err(error) => (500, error),
    };
    Response::builder()
        .status(status)
        .header("Access-Control-Allow-Origin", "*")
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(body.into_bytes())
        .unwrap()
}

fn process_kill(pid: i64, signal: &str) -> bool {
    #[cfg(unix)]
    {
        if pid <= 0 {
            return false;
        }
        return unsafe { libc::kill(pid as i32, crate::pty::signal_number(Some(signal))) == 0 };
    }
    #[allow(unreachable_code)]
    {
        let _ = (pid, signal);
        false
    }
}

fn process_alive(pid: i64) -> bool {
    #[cfg(unix)]
    {
        if pid <= 0 {
            return false;
        }
        // Signal 0 checks without signalling: EPERM still means the process exists.
        return unsafe { libc::kill(pid as i32, 0) == 0 || *libc::__error() == libc::EPERM };
    }
    #[allow(unreachable_code)]
    {
        let _ = pid;
        false
    }
}
