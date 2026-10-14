//! Tabby's config.yaml. The app keeps its own (`~/Library/Application Support/tabbi/config.yaml`), so it can run
//! next to Tabby; the first time, it starts from a copy of Tabby's. `TABBI_DATA_DIR` gives a test instance a
//! folder of its own, and then nothing of Tabby's is copied.
use std::path::PathBuf;

use tauri::{AppHandle, Manager};

pub fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = match std::env::var_os("TABBI_DATA_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => app.path().data_dir().map_err(|e| e.to_string())?.join("tabbi"),
    };
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn config_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(data_dir(app)?.join("config.yaml"))
}

#[tauri::command]
pub async fn config_read(app: AppHandle) -> Result<String, String> {
    let path = config_path(&app)?;
    if !path.exists() {
        let tabby = app.path().data_dir().map_err(|e| e.to_string())?.join("tabby").join("config.yaml");
        if std::env::var_os("TABBI_DATA_DIR").is_none() && tabby.exists() {
            std::fs::copy(&tabby, &path).map_err(|e| e.to_string())?;
        } else {
            return Ok(String::new());
        }
    }
    tokio::fs::read_to_string(&path).await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn config_write(app: AppHandle, content: String) -> Result<(), String> {
    let path = config_path(&app)?;
    // Written next to it and renamed over it, so a crash never leaves half a config.
    let tmp = path.with_extension("yaml.new");
    tokio::fs::write(&tmp, content).await.map_err(|e| e.to_string())?;
    tokio::fs::rename(&tmp, &path).await.map_err(|e| e.to_string())
}
