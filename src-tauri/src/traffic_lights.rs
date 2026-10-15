//! The macOS window buttons (close, minimize, zoom) where Tabby wants them: the close button's left edge `x` points
//! from the window's left side and the buttons' top `y` points below the window's top, as Electron's
//! `setWindowButtonPosition` places them (Tabby passes its theme's macOSWindowButtonsInsetX/Y, default 14 and 11).
//!
//! This is Electron's own geometry (shell/browser/ui/cocoa/window_buttons_proxy.mm, `redraw`): the buttons' container
//! is made `button height + 2y` tall at the top of the window, the buttons centered in it, and laid out from `x` with
//! their usual spacing. macOS puts the buttons back where it likes them on several occasions, so this runs again on
//! the same ones Electron uses: focus changes, resizes (leaving full screen included), title and theme changes.
//!
//! Those events come after macOS has drawn the window, though: while the window is being resized, every new size was
//! drawn with the buttons in macOS's place first and in ours a moment later, so they jittered between the two. So the
//! buttons are also moved back the moment macOS moves them (their views' frame-change notifications), before the
//! window is drawn.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tauri::{Manager, Runtime, State};

pub struct TrafficLights(Mutex<(f64, f64)>);

impl Default for TrafficLights {
    fn default() -> Self {
        Self(Mutex::new((14.0, 11.0)))
    }
}

/// How many times the buttons were moved back (for tests: none while nothing happens).
static MOVES: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "macos")]
thread_local! {
    /// (placing them moves them, which tells us again)
    static PLACING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// the close buttons whose moves are followed
    static FOLLOWED: std::cell::RefCell<std::collections::HashSet<usize>> = Default::default();
}

pub fn redraw<R: Runtime>(window: &tauri::Window<R>) {
    #[cfg(target_os = "macos")]
    {
        let (x, y) = *window.state::<TrafficLights>().0.lock().unwrap();
        let Ok(ns_window) = window.ns_window() else { return };
        let ns_window = ns_window as usize;
        let handle = window.clone();
        let _ = window.run_on_main_thread(move || unsafe {
            place(ns_window as *const objc2_app_kit::NSWindow, x, y);
            follow(handle, ns_window);
        });
    }
    #[cfg(not(target_os = "macos"))]
    let _ = window;
}

/// Places the buttons again whenever macOS moves one of them or resizes their container.
#[cfg(target_os = "macos")]
unsafe fn follow<R: Runtime>(window: tauri::Window<R>, ns_window: usize) {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_app_kit::NSWindowButton;
    use objc2_foundation::NSString;

    let Some(w) = (ns_window as *const objc2_app_kit::NSWindow).as_ref() else { return };
    let (Some(close), Some(minimize), Some(zoom)) = (
        w.standardWindowButton(NSWindowButton::CloseButton),
        w.standardWindowButton(NSWindowButton::MiniaturizeButton),
        w.standardWindowButton(NSWindowButton::ZoomButton),
    ) else {
        return;
    };
    let Some(container) = close.superview().and_then(|v| v.superview()) else { return };
    // (macOS makes new buttons for some changes, full screen for one; those are followed anew)
    if !FOLLOWED.with(|followed| followed.borrow_mut().insert(Retained::as_ptr(&close) as usize)) {
        return;
    }
    let views: [*const AnyObject; 4] = [
        Retained::as_ptr(&close).cast(),
        Retained::as_ptr(&minimize).cast(),
        Retained::as_ptr(&zoom).cast(),
        Retained::as_ptr(&container).cast(),
    ];
    let block = block2::RcBlock::new(move |_notification: *mut AnyObject| {
        if PLACING.with(|placing| placing.get()) {
            return;
        }
        let Ok(position) = window.state::<TrafficLights>().0.try_lock().map(|p| *p) else { return };
        place(ns_window as *const objc2_app_kit::NSWindow, position.0, position.1);
    });
    let center: *mut AnyObject = msg_send![class!(NSNotificationCenter), defaultCenter];
    let name = NSString::from_str("NSViewFrameDidChangeNotification");
    for view in views {
        // (the observer stays for as long as the app runs; it only hears from this view)
        let _: *mut AnyObject = msg_send![center, addObserverForName: &*name, object: view, queue: std::ptr::null_mut::<AnyObject>(), usingBlock: &*block];
    }
}

#[cfg(target_os = "macos")]
unsafe fn place(ns_window: *const objc2_app_kit::NSWindow, x: f64, y: f64) {
    PLACING.with(|placing| placing.set(true));
    place_now(ns_window, x, y);
    PLACING.with(|placing| placing.set(false));
}

#[cfg(target_os = "macos")]
unsafe fn place_now(ns_window: *const objc2_app_kit::NSWindow, x: f64, y: f64) {
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

    // (only what's out of place, so that placing them doesn't make macOS lay the title bar out again for nothing)
    let mut moved = false;
    let mut bounds = container.frame();
    bounds.size.height = height + 2.0 * y;
    bounds.origin.y = window.frame().size.height - bounds.size.height;
    if container.frame() != bounds {
        container.setFrame(bounds);
        moved = true;
    }

    let top = (bounds.size.height - height) / 2.0;
    let mut left = x;
    for button in [&close, &minimize, &zoom] {
        let origin = NSPoint::new(left, top);
        if button.frame().origin != origin {
            button.setFrameOrigin(origin);
            moved = true;
        }
        left += width + spacing;
    }
    if moved {
        MOVES.fetch_add(1, Ordering::Relaxed);
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
                let _ = tx.send(close_button_frame(&*(ns_window as *const objc2_app_kit::NSWindow)));
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

/// For test instances only: resizes the window `steps` times as a drag of its edge does, each size drawn at once, and
/// says where the close button was in each drawing (x, y as window_traffic_light_frame).
#[tauri::command]
pub async fn window_traffic_light_resize_steps(window: tauri::Window, steps: u32) -> Result<Vec<(f64, f64)>, String> {
    if std::env::var_os("TABBI_DEBUG_PORT").is_none() {
        return Err("only for test instances".into());
    }
    #[cfg(target_os = "macos")]
    {
        let ns_window = window.ns_window().map_err(|e| e.to_string())? as usize;
        let (tx, rx) = tokio::sync::oneshot::channel();
        window
            .run_on_main_thread(move || unsafe {
                let window = &*(ns_window as *const objc2_app_kit::NSWindow);
                let mut seen = Vec::new();
                for step in 0..steps {
                    let mut frame = window.frame();
                    frame.size.width += if step % 2 == 0 { 9.0 } else { -9.0 };
                    frame.size.height += if step % 2 == 0 { 5.0 } else { -5.0 };
                    window.setFrame_display(frame, true);
                    if let Some((x, y, _, _)) = close_button_frame(window) {
                        seen.push((x, y));
                    }
                }
                let _ = tx.send(seen);
            })
            .map_err(|e| e.to_string())?;
        return rx.await.map_err(|e| e.to_string());
    }
    #[allow(unreachable_code)]
    {
        let _ = (window, steps);
        Err("macOS only".into())
    }
}

/// For tests: how many times the buttons were moved back so far.
#[tauri::command]
pub fn window_traffic_light_moves() -> u64 {
    MOVES.load(Ordering::Relaxed)
}

#[cfg(target_os = "macos")]
unsafe fn close_button_frame(window: &objc2_app_kit::NSWindow) -> Option<(f64, f64, f64, f64)> {
    use objc2_app_kit::NSWindowButton;
    let close = window.standardWindowButton(NSWindowButton::CloseButton)?;
    let in_window = close.convertRect_toView(close.bounds(), None);
    let height = window.contentView().map(|v| v.frame().size.height).unwrap_or_default();
    Some((in_window.origin.x, height - in_window.origin.y - in_window.size.height, in_window.size.width, in_window.size.height))
}
