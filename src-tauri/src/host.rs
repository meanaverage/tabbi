//! What the page needs to stand in for Node's `process` and `os`: environment, platform, versions.
use std::collections::HashMap;

use serde::Serialize;

#[derive(Serialize)]
pub struct HostInfo {
    env: HashMap<String, String>,
    platform: &'static str,
    arch: &'static str,
    release: String,
    hostname: String,
    version: String,
    executable: String,
    debug: bool,
}

fn node_platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    }
}

/// `uname`'s release and node name, as Node's os.release() and os.hostname() report them.
fn uname() -> (String, String) {
    #[cfg(unix)]
    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) == 0 {
            let s = |p: &[libc::c_char]| std::ffi::CStr::from_ptr(p.as_ptr()).to_string_lossy().into_owned();
            return (s(&u.release), s(&u.nodename));
        }
    }
    #[allow(unreachable_code)]
    (String::new(), String::from("localhost"))
}

#[tauri::command]
pub fn host_info(app: tauri::AppHandle) -> HostInfo {
    let (release, hostname) = uname();
    HostInfo {
        env: std::env::vars().collect(),
        platform: node_platform(),
        arch: node_arch(),
        release,
        hostname,
        version: app.package_info().version.to_string(),
        executable: std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        debug: cfg!(debug_assertions) || std::env::var_os("TABBY_DEV").is_some(),
    }
}

/// Opens a link in the default browser (or mail app): links clicked in the terminal and in Tabby's settings.
#[tauri::command]
pub fn open_external(url: String) -> Result<(), String> {
    let scheme = url.split_once(':').map(|(s, _)| s.to_ascii_lowercase()).unwrap_or_default();
    if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
        return Err(format!("not opening {scheme}: links"));
    }
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    std::process::Command::new(opener).arg(&url).spawn().map(|_| ()).map_err(|e| e.to_string())
}
