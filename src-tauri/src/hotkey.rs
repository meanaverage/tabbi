//! Tabby's "Toggle terminal window" hotkey, system-wide: pressed anywhere, it tells the main window, whose docking
//! service shows and focuses it, or hides it. A test instance never registers one: it would take the key from the
//! person using the Mac.
use std::str::FromStr;

use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

pub fn plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, _shortcut, event| {
            if event.state() == ShortcutState::Pressed {
                let _ = app.emit_to(tauri::EventTarget::labeled("main"), "host:toggle-window", ());
            }
        })
        .build()
}

/// `specs`: Tauri accelerators ("Ctrl+Space", "Command+Shift+T"), already converted from Tabby's notation.
#[tauri::command]
pub fn global_hotkey_set(app: AppHandle, specs: Vec<String>) -> Result<(), String> {
    let shortcuts = app.global_shortcut();
    shortcuts.unregister_all().map_err(|e| e.to_string())?;
    if crate::debug::background() {
        return Ok(());
    }
    let mut errors = Vec::new();
    for spec in specs {
        match Shortcut::from_str(&spec) {
            Ok(shortcut) => {
                if let Err(e) = shortcuts.register(shortcut) {
                    errors.push(format!("{spec}: {e}"));
                }
            }
            Err(e) => errors.push(format!("{spec}: {e}")),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}
