//! Window appearance and behavior Tabby's settings control: vibrancy (the blurred desktop behind the window), opacity,
//! following the system's dark mode, and docking to a screen edge. The Electron build does these in its main process.
use tauri::{utils::config::WindowEffectsConfig, window::Effect, Theme, WebviewWindow};

/// `kind`: Tabby's appearance.vibrancyType ('blur' or 'fluent', named after Windows' effects); on macOS the closest
/// window materials.
#[tauri::command]
pub fn window_set_vibrancy(window: WebviewWindow, enabled: bool, kind: Option<String>) -> Result<(), String> {
    if !enabled {
        return window.set_effects(None::<WindowEffectsConfig>).map_err(|e| e.to_string());
    }
    let effect = match kind.as_deref() {
        Some("fluent") => Effect::HudWindow,
        _ => Effect::UnderWindowBackground,
    };
    window
        .set_effects(WindowEffectsConfig { effects: vec![effect], ..Default::default() })
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn window_set_opacity(window: WebviewWindow, opacity: f64) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let value = opacity.clamp(0.05, 1.0);
        let ns_window = window.ns_window().map_err(|e| e.to_string())? as usize;
        window
            .run_on_main_thread(move || unsafe {
                if let Some(ns_window) = (ns_window as *const objc2_app_kit::NSWindow).as_ref() {
                    ns_window.setAlphaValue(value);
                }
            })
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (window, opacity);
    Ok(())
}

/// `mode`: Tabby's appearance.colorSchemeMode, 'dark', 'light' or 'auto' (follow the system).
#[tauri::command]
pub fn window_set_dark_mode(window: WebviewWindow, mode: String) -> Result<(), String> {
    let theme = match mode.as_str() {
        "dark" => Some(Theme::Dark),
        "light" => Some(Theme::Light),
        _ => None,
    };
    window.set_theme(theme).map_err(|e| e.to_string())
}

/// What the Electron build does for a window docked to a screen edge and kept on top: above other windows (and full
/// screen apps), on every Space, and no Dock icon of its own while it acts as the always-there terminal.
#[tauri::command]
pub fn window_set_docked(window: WebviewWindow, enabled: bool) -> Result<(), String> {
    if window.label() != "main" {
        return Ok(());
    }
    // A test instance never floats above the windows of the person using the Mac.
    let enabled = enabled && !crate::debug::background();
    window.set_always_on_top(enabled).map_err(|e| e.to_string())?;
    window.set_visible_on_all_workspaces(enabled).map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    {
        let ns_window = window.ns_window().map_err(|e| e.to_string())? as usize;
        window
            .run_on_main_thread(move || unsafe {
                use objc2::MainThreadMarker;
                use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSWindow, NSWindowCollectionBehavior as Behavior};
                if let Some(ns_window) = (ns_window as *const NSWindow).as_ref() {
                    // NSScreenSaverWindowLevel: above full screen apps too.
                    ns_window.setLevel(if enabled { 1000 } else { 0 });
                    let mut behavior = ns_window.collectionBehavior();
                    behavior.set(Behavior::FullScreenPrimary, !enabled);
                    behavior.set(Behavior::FullScreenAuxiliary, enabled);
                    ns_window.setCollectionBehavior(behavior);
                }
                let app = NSApplication::sharedApplication(MainThreadMarker::new_unchecked());
                // A test instance stays an accessory app (no Dock icon, never activated): see debug::background.
                app.setActivationPolicy(if enabled || crate::debug::background() {
                    NSApplicationActivationPolicy::Accessory
                } else {
                    NSApplicationActivationPolicy::Regular
                });
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
