//! Tabby's config.yaml. The app keeps its own (`~/Library/Application Support/tabbi/config.yaml`), so it can run
//! next to Tabby; on the first launch it offers to start from a copy of Tabby's. `TABBI_DATA_DIR` gives a test
//! instance a folder of its own, and a test instance never looks at Tabby's folder unless `TABBI_IMPORT_FROM` names
//! one (with `TABBI_IMPORT_ANSWER`, `import` or `fresh`, standing in for the user's choice).
use std::path::PathBuf;

use tauri::{AppHandle, Emitter, Manager};

pub fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = match std::env::var_os("TABBI_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => app.path().data_dir().map_err(|e| e.to_string())?.join("tabbi"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Where the window's size and position are kept, next to the config.
pub fn window_state_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("window-state.json"))
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("config.yaml"))
}

#[tauri::command]
pub async fn config_read(app: AppHandle) -> Result<String, String> {
    match tokio::fs::read_to_string(config_path(&app)?).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.to_string()),
    }
}

/// Tabby's folder: where a first launch can import settings from.
fn tabby_dir(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("TABBI_IMPORT_FROM") {
        return Ok(PathBuf::from(dir));
    }
    Ok(app.path().data_dir().map_err(|e| e.to_string())?.join("tabby"))
}

/// The first launch, before the page starts Tabby: with Tabby installed, a dialog asks whether to start from a copy
/// of its settings. Tabby's file is copied as it is (an encrypted one asks for its passphrase in tabbi, as in Tabby),
/// and never changed. Either answer leaves a config behind, so the question is asked once.
#[tauri::command]
pub async fn config_first_launch(app: AppHandle) -> Result<(), String> {
    let path = config_path(&app)?;
    let background = crate::debug::background();
    if path.exists() || (background && std::env::var_os("TABBI_IMPORT_FROM").is_none()) {
        return Ok(());
    }
    let source = tabby_dir(&app)?.join("config.yaml");
    if !source.exists() {
        return Ok(());
    }
    let import = if background {
        std::env::var("TABBI_IMPORT_ANSWER").as_deref() == Ok("import")
    } else {
        ask_to_import(&app).await
    };
    let content = if import {
        tokio::fs::read_to_string(&source).await.map_err(|e| format!("Could not read Tabby's settings: {e}"))?
    } else {
        String::new()
    };
    // (never over a config that appeared meanwhile)
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => std::io::Write::write_all(&mut file, content.as_bytes()).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

async fn ask_to_import(app: &AppHandle) -> bool {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .message(
            "Bring your settings over from Tabby? Your profiles, hotkeys, appearance and SSH settings come over, \
             and Tabby's own files aren't changed.\n\nSaved passwords stay in the Keychain: macOS may ask whether \
             tabbi can use each one.",
        )
        .title("Welcome to tabbi")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom("Import from Tabby".into(), "Start fresh".into()))
        .show(move |import| {
            let _ = tx.send(import);
        });
    rx.await.unwrap_or(false)
}

#[tauri::command]
pub async fn config_write(app: AppHandle, webview: tauri::Webview, content: String) -> Result<(), String> {
    let path = config_path(&app)?;
    // Written next to it and renamed over it, so a crash never leaves half a config.
    let tmp = path.with_extension("yaml.new");
    tokio::fs::write(&tmp, content).await.map_err(|e| e.to_string())?;
    tokio::fs::rename(&tmp, &path).await.map_err(|e| e.to_string())?;
    // The other windows load it again, as the Electron build's main process tells them to.
    let saved_by = webview.label().to_string();
    let _ = app.emit_filter("host:config-change", (), |target| match target {
        tauri::EventTarget::WebviewWindow { label } | tauri::EventTarget::Webview { label } | tauri::EventTarget::Window { label } => *label != saved_by,
        _ => false,
    });
    Ok(())
}
