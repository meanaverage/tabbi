//! Window appearance and behavior Tabby's settings control: vibrancy (the blurred desktop behind the window), opacity,
//! following the system's dark mode, and docking to a screen edge. The Electron build does these in its main process.
//! Also the page's frame rate.
use serde::Serialize;
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

/// WebKit holds page rendering near 60 fps even on 120 Hz screens (its PreferPageRenderingUpdatesNear60FPSEnabled
/// feature), so a terminal would scroll and echo at half the rate the screen can show; Electron's Chromium uses the
/// screen's rate. This turns that off, through private WebKit API (nothing happens where it's missing).
/// TABBI_WEBKIT_60FPS=1 keeps WebKit's default, for comparing.
pub fn full_frame_rate(window: &WebviewWindow) {
    if std::env::var_os("TABBI_WEBKIT_60FPS").is_some() {
        return;
    }
    #[cfg(target_os = "macos")]
    let _ = window.with_webview(|webview| unsafe {
        if let Some((preferences, feature)) = webkit::near_60_fps(webview.inner()) {
            let _: () = objc2::msg_send![preferences, _setEnabled: false, forFeature: feature];
        }
    });
    #[cfg(not(target_os = "macos"))]
    let _ = window;
}

#[derive(Serialize)]
pub struct FrameRate {
    /// The most frames per second the window's screen shows.
    screen: isize,
    /// Whether WebKit still holds the page near 60 fps (none where that can't be told).
    limited: Option<bool>,
}

/// For tests: how fast the window's page may draw.
#[tauri::command]
pub async fn window_frame_rate(window: WebviewWindow) -> Result<FrameRate, String> {
    #[cfg(target_os = "macos")]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        window
            .with_webview(move |webview| unsafe {
                use objc2::msg_send;
                use objc2::runtime::AnyObject;
                let view = webview.inner() as *mut AnyObject;
                let ns_window: *mut AnyObject = msg_send![view, window];
                let screen: *mut AnyObject = if ns_window.is_null() { std::ptr::null_mut() } else { msg_send![ns_window, screen] };
                let screen = if screen.is_null() { 0 } else { msg_send![screen, maximumFramesPerSecond] };
                let limited = webkit::near_60_fps(webview.inner()).and_then(|(preferences, feature)| {
                    let readable: bool = msg_send![preferences, respondsToSelector: objc2::sel!(_isEnabledForFeature:)];
                    readable.then(|| msg_send![preferences, _isEnabledForFeature: feature])
                });
                let _ = tx.send(FrameRate { screen, limited });
            })
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        Ok(FrameRate { screen: 0, limited: None })
    }
}

/// Asks WebKit to collect garbage now. It otherwise waits for allocation pressure, so a closed tab's terminal (its
/// scrollback, its WebGL textures) can stay in memory for a long while. Private WebKit API (nothing happens where it's
/// missing).
#[tauri::command]
pub fn webview_collect_garbage(window: WebviewWindow) {
    #[cfg(target_os = "macos")]
    let _ = window.with_webview(|webview| unsafe {
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        let view = webview.inner() as *mut AnyObject;
        let config: *mut AnyObject = msg_send![view, configuration];
        let pool: *mut AnyObject = msg_send![config, processPool];
        let gc = objc2::sel!(_garbageCollectJavaScriptObjectsForTesting);
        let available: bool = msg_send![pool, respondsToSelector: gc];
        if available {
            let _: () = msg_send![pool, _garbageCollectJavaScriptObjectsForTesting];
        }
    });
    #[cfg(not(target_os = "macos"))]
    let _ = window;
}

/// For measurements: the process WebKit renders this window's page in (0 where that can't be told).
#[tauri::command]
pub async fn webview_process_id(window: WebviewWindow) -> Result<i32, String> {
    #[cfg(target_os = "macos")]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        window
            .with_webview(move |webview| unsafe {
                use objc2::msg_send;
                use objc2::runtime::AnyObject;
                let view = webview.inner() as *mut AnyObject;
                let available: bool = msg_send![view, respondsToSelector: objc2::sel!(_webProcessIdentifier)];
                let _ = tx.send(if available { msg_send![view, _webProcessIdentifier] } else { 0 });
            })
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(std::time::Duration::from_secs(5)).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        Ok(0)
    }
}

#[cfg(target_os = "macos")]
mod webkit {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send, sel};

    /// The WKWebView's preferences and WebKit's PreferPageRenderingUpdatesNear60FPSEnabled feature, where this WebKit
    /// has both (private API: `+[WKPreferences _features]`, `-[WKPreferences _setEnabled:forFeature:]`).
    pub unsafe fn near_60_fps(view: *mut std::ffi::c_void) -> Option<(*mut AnyObject, *mut AnyObject)> {
        let view = view as *mut AnyObject;
        let config: *mut AnyObject = msg_send![view, configuration];
        let preferences: *mut AnyObject = msg_send![config, preferences];
        let listed: bool = msg_send![class!(WKPreferences), respondsToSelector: sel!(_features)];
        let settable: bool = msg_send![preferences, respondsToSelector: sel!(_setEnabled:forFeature:)];
        if !listed || !settable {
            return None;
        }
        let features: *mut AnyObject = msg_send![class!(WKPreferences), _features];
        let count: usize = msg_send![features, count];
        for i in 0..count {
            let feature: *mut AnyObject = msg_send![features, objectAtIndex: i];
            let key: *mut AnyObject = msg_send![feature, key];
            let name: *const std::ffi::c_char = msg_send![key, UTF8String];
            if !name.is_null() && std::ffi::CStr::from_ptr(name).to_bytes() == b"PreferPageRenderingUpdatesNear60FPSEnabled" {
                return Some((preferences, feature));
            }
        }
        None
    }
}
