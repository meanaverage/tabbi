//! More windows (Tabby's "New window", ⌘N). Each is a window like the first, with its own tabs; only the first one
//! ("main") reopens the tabs saved when the app last quit, as in Tabby. Settings are shared: a window that saves them
//! tells the others to load them again (config.rs).
use std::sync::atomic::{AtomicU32, Ordering};

use tauri::{AppHandle, Manager, WebviewWindowBuilder};

static NEXT: AtomicU32 = AtomicU32::new(2);

/// Makes a window the way `lib.rs` makes the first one; `label` is "main" for that one.
pub fn build(app: &AppHandle, label: &str) -> Result<tauri::WebviewWindow, String> {
    let background = crate::debug::background();
    let mut config = app.config().app.windows.first().cloned().ok_or("no window config")?;
    config.label = label.into();
    let builder = WebviewWindowBuilder::from_config(app, &config)
        .map_err(|e| e.to_string())?
        .focused(!background)
        .visible(!background);
    #[cfg(target_os = "macos")]
    let builder = match crate::debug::web_store() {
        Some(id) => builder.data_store_identifier(id),
        None => builder,
    };
    let window = builder.build().map_err(|e| e.to_string())?;
    if background {
        crate::debug::keep_behind(&window);
    }
    crate::traffic_lights::redraw(&window.as_ref().window());
    Ok(window)
}

/// Tabby's HostAppService.newWindow. The new window's label is returned.
#[tauri::command]
pub fn window_new(app: AppHandle) -> Result<String, String> {
    let label = loop {
        let label = format!("main-{}", NEXT.fetch_add(1, Ordering::Relaxed));
        if app.get_webview_window(&label).is_none() {
            break label;
        }
    };
    build(&app, &label)?;
    Ok(label)
}
