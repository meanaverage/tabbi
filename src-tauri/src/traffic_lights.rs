//! The macOS window buttons (close, minimize, zoom) where Tabby wants them: the close button's left edge `x` points
//! from the window's left side and the buttons' top `y` points below the window's top, as Electron's
//! `setWindowButtonPosition` places them (Tabby passes its theme's macOSWindowButtonsInsetX/Y, default 14 and 11).
//!
//! This is Electron's own geometry (shell/browser/ui/cocoa/window_buttons_proxy.mm, `redraw`): the buttons' container
//! is made `button height + 2y` tall at the top of the window, the buttons centered in it, and laid out from `x` with
//! their usual spacing. macOS puts the buttons back where it likes them on several occasions, so this runs again on
//! the same ones Electron uses: focus changes, resizes (leaving full screen included), title and theme changes.
use std::sync::Mutex;

use tauri::{Manager, Runtime, State};

pub struct TrafficLights(Mutex<(f64, f64)>);

impl Default for TrafficLights {
    fn default() -> Self {
        Self(Mutex::new((14.0, 11.0)))
    }
}

pub fn redraw<R: Runtime>(window: &tauri::Window<R>) {
    #[cfg(target_os = "macos")]
    {
        let (x, y) = *window.state::<TrafficLights>().0.lock().unwrap();
        let Ok(ns_window) = window.ns_window() else { return };
        let ns_window = ns_window as usize;
        let _ = window.run_on_main_thread(move || unsafe { place(ns_window as *const objc2_app_kit::NSWindow, x, y) });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = window;
}

#[cfg(target_os = "macos")]
unsafe fn place(ns_window: *const objc2_app_kit::NSWindow, x: f64, y: f64) {
    use objc2_app_kit::{NSWindowButton, NSWindowStyleMask};
    use objc2_foundation::NSPoint;

    let Some(window) = ns_window.as_ref() else { return };
    // In full screen macOS shows the buttons in its own title bar (as Electron, leave them alone).
    if window.styleMask().contains(NSWindowStyleMask::FullScreen) {
        return;
    }
    let (Some(close), Some(minimize), Some(zoom)) = (
        window.standardWindowButton(NSWindowButton::CloseButton),
        window.standardWindowButton(NSWindowButton::MiniaturizeButton),
        window.standardWindowButton(NSWindowButton::ZoomButton),
    ) else {
        return;
    };
    let Some(container) = close.superview().and_then(|v| v.superview()) else { return };
    let (close_frame, minimize_frame) = (close.frame(), minimize.frame());
    let (width, height) = (close_frame.size.width, close_frame.size.height);
    let spacing = minimize_frame.origin.x - (close_frame.origin.x + width);

    let mut bounds = container.frame();
    bounds.size.height = height + 2.0 * y;
    bounds.origin.y = window.frame().size.height - bounds.size.height;
    container.setFrame(bounds);

    let top = (bounds.size.height - height) / 2.0;
    let mut left = x;
    for button in [&close, &minimize, &zoom] {
        button.setFrameOrigin(NSPoint::new(left, top));
        left += width + spacing;
    }
}

pub fn on_window_event<R: Runtime>(window: &tauri::Window<R>, event: &tauri::WindowEvent) {
    if matches!(
        event,
        tauri::WindowEvent::Focused(_) | tauri::WindowEvent::Resized(_) | tauri::WindowEvent::ThemeChanged(_) | tauri::WindowEvent::ScaleFactorChanged { .. }
    ) {
        redraw(window);
    }
}

/// Tabby's theme says where the buttons go (tabby-electron does the same with Electron's API).
#[tauri::command]
pub fn window_set_traffic_light_position(window: tauri::Window, state: State<'_, TrafficLights>, x: f64, y: f64) {
    *state.0.lock().unwrap() = (x, y);
    redraw(&window);
}

/// Setting the title makes macOS lay its title bar out again, buttons included. Tauri sets titles later, in a step of
/// its own, which would undo a placement made now; so (as Electron) the title is set and the buttons placed in one go.
#[tauri::command]
pub fn window_set_title(window: tauri::Window, state: State<'_, TrafficLights>, title: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let (x, y) = *state.0.lock().unwrap();
        let ns_window = window.ns_window().map_err(|e| e.to_string())? as usize;
        return window
            .run_on_main_thread(move || unsafe {
                let ns_window = ns_window as *const objc2_app_kit::NSWindow;
                if let Some(w) = ns_window.as_ref() {
                    w.setTitle(&objc2_foundation::NSString::from_str(&title));
                }
                place(ns_window, x, y);
            })
            .map_err(|e| e.to_string());
    }
    #[allow(unreachable_code)]
    {
        let _ = state;
        window.set_title(&title).map_err(|e| e.to_string())
    }
}

/// Where the close button is, from the window's top-left corner in points (for tests).
#[tauri::command]
pub async fn window_traffic_light_frame(window: tauri::Window) -> Result<(f64, f64, f64, f64), String> {
    #[cfg(target_os = "macos")]
    {
        let ns_window = window.ns_window().map_err(|e| e.to_string())? as usize;
        let (tx, rx) = tokio::sync::oneshot::channel();
        window
            .run_on_main_thread(move || unsafe {
                use objc2_app_kit::NSWindowButton;
                let window = &*(ns_window as *const objc2_app_kit::NSWindow);
                let frame = window.standardWindowButton(NSWindowButton::CloseButton).map(|close| {
                    let in_window = close.convertRect_toView(close.bounds(), None);
                    let height = window.contentView().map(|v| v.frame().size.height).unwrap_or_default();
                    (in_window.origin.x, height - in_window.origin.y - in_window.size.height, in_window.size.width, in_window.size.height)
                });
                let _ = tx.send(frame);
            })
            .map_err(|e| e.to_string())?;
        return rx.await.map_err(|e| e.to_string())?.ok_or_else(|| "no close button".to_string());
    }
    #[allow(unreachable_code)]
    {
        let _ = window;
        Err("macOS only".into())
    }
}
