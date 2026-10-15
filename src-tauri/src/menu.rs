//! The macOS menu bar, laid out as Electron Tabby's (app/lib/app.ts `setupMenu`). Quit asks the windows to close, so
//! Tabby can confirm closing running tabs, instead of exiting at once; Preferences opens Settings in the focused
//! window. Only debug builds offer the developer tools.
use tauri::menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Emitter, Manager, Runtime};

const WEBSITE: &str = "https://github.com/meanaverage/tabbi";

pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let about = AboutMetadata { name: Some("tabbi".into()), website: Some(WEBSITE.into()), ..Default::default() };
    let application = Submenu::with_items(app, "tabbi", true, &[
        &PredefinedMenuItem::about(app, Some("About tabbi"), Some(about))?,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "preferences", "Preferences…", true, Some("Cmd+,"))?,
        &PredefinedMenuItem::separator(app)?,
        &PredefinedMenuItem::services(app, None)?,
        &PredefinedMenuItem::separator(app)?,
        &PredefinedMenuItem::hide(app, None)?,
        &PredefinedMenuItem::hide_others(app, None)?,
        &PredefinedMenuItem::show_all(app, None)?,
        &PredefinedMenuItem::separator(app)?,
        &MenuItem::with_id(app, "quit", "Quit tabbi", true, Some("Cmd+Q"))?,
    ])?;
    let edit = Submenu::with_items(app, "Edit", true, &[
        &PredefinedMenuItem::undo(app, None)?,
        &PredefinedMenuItem::redo(app, None)?,
        &PredefinedMenuItem::separator(app)?,
        &PredefinedMenuItem::cut(app, None)?,
        &PredefinedMenuItem::copy(app, None)?,
        &PredefinedMenuItem::paste(app, None)?,
        &PredefinedMenuItem::select_all(app, None)?,
    ])?;
    let view = Submenu::with_items(app, "View", true, &[
        #[cfg(debug_assertions)]
        &MenuItem::with_id(app, "devtools", "Toggle Developer Tools", true, Some("Cmd+Alt+I"))?,
        #[cfg(debug_assertions)]
        &PredefinedMenuItem::separator(app)?,
        &PredefinedMenuItem::fullscreen(app, None)?,
    ])?;
    let window = Submenu::with_items(app, "Window", true, &[
        &PredefinedMenuItem::minimize(app, None)?,
        &PredefinedMenuItem::maximize(app, Some("Zoom"))?,
        &PredefinedMenuItem::separator(app)?,
        &PredefinedMenuItem::bring_all_to_front(app, None)?,
    ])?;
    let help = Submenu::with_items(app, "Help", true, &[
        &MenuItem::with_id(app, "website", "tabbi on GitHub", true, None::<&str>)?,
    ])?;
    Menu::with_items(app, &[&application, &edit, &view, &window, &help])
}

/// The window menu commands go to: the focused one, else the first.
fn target<R: Runtime>(app: &AppHandle<R>) -> Option<tauri::WebviewWindow<R>> {
    let windows = app.webview_windows();
    windows.values().find(|w| w.is_focused().unwrap_or(false)).cloned().or_else(|| windows.get("main").cloned())
}

pub fn on_event<R: Runtime>(app: &AppHandle<R>, id: &str) {
    match id {
        "preferences" => {
            if let Some(window) = target(app) {
                let _ = window.show();
                if !crate::debug::background() {
                    let _ = window.set_focus();
                }
                let _ = app.emit_to(tauri::EventTarget::labeled(window.label()), "host:preferences-menu", ());
            }
        }
        "quit" => quit(app),
        "website" => {
            let _ = crate::host::open_external(WEBSITE.into());
        }
        #[cfg(debug_assertions)]
        "devtools" if !crate::debug::background() => {
            if let Some(window) = target(app) {
                if window.is_devtools_open() {
                    window.close_devtools();
                } else {
                    window.open_devtools();
                }
            }
        }
        _ => {}
    }
}

/// Asks every window to close, as closing it would: Tabby confirms closing running tabs, and the app exits once the
/// last window is gone.
pub fn quit<R: Runtime>(app: &AppHandle<R>) {
    let windows = app.webview_windows();
    if windows.is_empty() {
        app.exit(0);
    }
    for window in windows.values() {
        let _ = window.close();
    }
}
