//! The macOS window buttons (close, minimize, zoom) where Tabby wants them: the close button's left edge `x` points
//! from the window's left side and the buttons' top `y` points below the window's top, as Electron's
//! `setWindowButtonPosition` places them (Tabby passes its theme's macOSWindowButtonsInsetX/Y, default 14 and 11).
//!
//! This is Electron's own geometry (shell/browser/ui/cocoa/window_buttons_proxy.mm, `redraw`): the buttons' container
//! is made `button height + 2y` tall at the top of the window, the buttons centered in it, and laid out from `x` as
//! far apart as macOS lays them out (see native_pitches).
//!
//! macOS lays the buttons out again on several occasions, a resize above all, and draws them where it put them.
//! Moving them back afterwards (on the window's events, as Electron does, or when macOS says a view moved) comes too
//! late or misses some of macOS's moves, and they jitter while the window is resized. So the buttons and their
//! container don't take any other place: each is made an instance of a subclass of its own class (as Key-Value
//! Observing does) whose setFrame:, setFrameOrigin: and setFrameSize: (and macOS's own setFrameOrigin:ignoreRentry: for
//! its buttons) put it where it belongs. In full screen, where macOS shows the
//! buttons in a title bar of its own, they go where macOS says.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use tauri::{Manager, Runtime, State};

pub struct TrafficLights(Mutex<(f64, f64)>);

impl Default for TrafficLights {
    fn default() -> Self {
        Self(Mutex::new((14.0, 11.0)))
    }
}

/// How many times the buttons were moved (for tests: none while nothing happens).
static MOVES: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "macos")]
thread_local! {
    /// where each button goes in its title bar, by the button's address
    static ORIGINS: std::cell::RefCell<std::collections::HashMap<usize, objc2_foundation::NSPoint>> = Default::default();
    /// how tall the buttons' container is, by its address
    static HEIGHTS: std::cell::RefCell<std::collections::HashMap<usize, f64>> = Default::default();
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

/// The distances from the close button's left edge to minimize's, and from minimize's to zoom's, as macOS lays them out:
/// measured once in a window of the same kind made for it and never shown. (Measuring the window's own buttons doesn't
/// work: while macOS lays them out one at a time, some are where macOS puts them and some where we did.)
#[cfg(target_os = "macos")]
unsafe fn native_pitches(style: objc2_app_kit::NSWindowStyleMask) -> (f64, f64) {
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    thread_local! {
        static PITCHES: std::cell::Cell<Option<(f64, f64)>> = const { std::cell::Cell::new(None) };
    }
    if let Some(pitches) = PITCHES.with(|p| p.get()) {
        return pitches;
    }
    let measured = (|| {
        let allocated: Allocated<AnyObject> = msg_send![class!(NSWindow), alloc];
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(480.0, 320.0));
        let probe: Option<Retained<AnyObject>> = msg_send![allocated, initWithContentRect: rect, styleMask: style, backing: 2usize, defer: true];
        let probe = probe?;
        let _: () = msg_send![&*probe, setReleasedWhenClosed: false];
        let _: () = msg_send![&*probe, layoutIfNeeded];
        let mut lefts = [0.0; 3];
        for (i, kind) in [0usize, 1, 2].into_iter().enumerate() {
            let button: Option<Retained<AnyObject>> = msg_send![&*probe, standardWindowButton: kind];
            let frame: NSRect = msg_send![&*button?, frame];
            lefts[i] = frame.origin.x;
        }
        let pitches = (lefts[1] - lefts[0], lefts[2] - lefts[1]);
        (pitches.0 > 0.0 && pitches.1 > 0.0).then_some(pitches)
    })();
    // (if that fails: 14-point buttons 6 points apart, as macOS had them before 26, which has them 9 apart)
    let pitches = measured.unwrap_or((20.0, 20.0));
    PITCHES.with(|p| p.set(Some(pitches)));
    pitches
}

#[cfg(target_os = "macos")]
unsafe fn place(ns_window: *const objc2_app_kit::NSWindow, x: f64, y: f64) {
    use objc2::rc::Retained;
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
    let height = close.frame().size.height;
    let pitches = native_pitches(window.styleMask());
    let top = y;

    HEIGHTS.with(|h| h.borrow_mut().insert(Retained::as_ptr(&container) as usize, height + 2.0 * y));
    ORIGINS.with(|o| {
        let mut origins = o.borrow_mut();
        for (button, left) in [(&close, x), (&minimize, x + pitches.0), (&zoom, x + pitches.0 + pitches.1)] {
            origins.insert(Retained::as_ptr(button) as usize, NSPoint::new(left, top));
        }
    });
    let before = [container.frame(), close.frame(), minimize.frame(), zoom.frame()];
    keep_in_place(&container);
    for button in [&close, &minimize, &zoom] {
        keep_in_place(button);
    }
    // (the views put themselves in place when their frames are set; set them to what they are)
    container.setFrame(container.frame());
    for button in [&close, &minimize, &zoom] {
        button.setFrameOrigin(button.frame().origin);
    }
    if before != [container.frame(), close.frame(), minimize.frame(), zoom.frame()] {
        MOVES.fetch_add(1, Ordering::Relaxed);
    }
}

/// Makes `view` an instance of a subclass of its class that puts it in its place (ORIGINS, HEIGHTS) whenever its frame
/// is set, except in full screen.
#[cfg(target_os = "macos")]
unsafe fn keep_in_place(view: &objc2_app_kit::NSView) {
    use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
    use objc2::{msg_send, sel};
    use objc2_foundation::{NSPoint, NSRect, NSSize};

    const PREFIX: &str = "TabbiKeptInPlace_";
    let class = view.class();
    let name = class.name().to_string_lossy();
    if name.starts_with(PREFIX) {
        return;
    }

    /// The frame the view gets instead of `frame`.
    unsafe fn in_place(this: &AnyObject, mut frame: NSRect) -> NSRect {
        use objc2_app_kit::{NSView, NSWindowStyleMask};
        let view = &*(this as *const AnyObject as *const NSView);
        if view.window().is_some_and(|w| w.styleMask().contains(NSWindowStyleMask::FullScreen)) {
            return frame;
        }
        let key = this as *const AnyObject as usize;
        if let Some(origin) = ORIGINS.with(|o| o.borrow().get(&key).copied()) {
            frame.origin = origin;
        }
        if let Some(height) = HEIGHTS.with(|h| h.borrow().get(&key).copied()) {
            // (at the top of the window)
            let above = view.superview().map(|v| v.frame().size.height).unwrap_or(frame.origin.y + frame.size.height);
            frame.size.height = height;
            frame.origin.y = above - height;
        }
        frame
    }
    unsafe extern "C-unwind" fn set_frame(this: &AnyObject, _cmd: Sel, frame: NSRect) {
        let original = this.class().superclass().unwrap();
        let _: () = msg_send![super(this, original), setFrame: in_place(this, frame)];
    }
    // (each only changes what it's for: NSView's setFrame: calls these two, which mustn't call it back)
    unsafe extern "C-unwind" fn set_frame_origin(this: &AnyObject, _cmd: Sel, origin: NSPoint) {
        let original = this.class().superclass().unwrap();
        let current: NSRect = msg_send![this, frame];
        let _: () = msg_send![super(this, original), setFrameOrigin: in_place(this, NSRect::new(origin, current.size)).origin];
    }
    // (macOS's own way of moving its buttons, which goes past setFrameOrigin:)
    unsafe extern "C-unwind" fn set_frame_origin_ignore_reentry(this: &AnyObject, _cmd: Sel, origin: NSPoint, ignore: Bool) {
        let original = this.class().superclass().unwrap();
        let current: NSRect = msg_send![this, frame];
        let origin = in_place(this, NSRect::new(origin, current.size)).origin;
        let _: () = msg_send![super(this, original), setFrameOrigin: origin, ignoreRentry: ignore];
    }
    unsafe extern "C-unwind" fn set_frame_size(this: &AnyObject, _cmd: Sel, size: NSSize) {
        let original = this.class().superclass().unwrap();
        let current: NSRect = msg_send![this, frame];
        let _: () = msg_send![super(this, original), setFrameSize: in_place(this, NSRect::new(current.origin, size)).size];
    }

    let subclass_name = std::ffi::CString::new(format!("{PREFIX}{name}")).unwrap();
    let subclass: &'static AnyClass = match AnyClass::get(&subclass_name) {
        Some(subclass) => subclass,
        None => {
            let Some(mut builder) = ClassBuilder::new(&subclass_name, class) else { return };
            builder.add_method(sel!(setFrame:), set_frame as unsafe extern "C-unwind" fn(_, _, _));
            builder.add_method(sel!(setFrameOrigin:), set_frame_origin as unsafe extern "C-unwind" fn(_, _, _));
            builder.add_method(sel!(setFrameSize:), set_frame_size as unsafe extern "C-unwind" fn(_, _, _));
            if class.instance_method(sel!(setFrameOrigin:ignoreRentry:)).is_some() {
                builder.add_method(sel!(setFrameOrigin:ignoreRentry:), set_frame_origin_ignore_reentry as unsafe extern "C-unwind" fn(_, _, _, _));
            }
            builder.register()
        }
    };
    objc2::ffi::object_setClass(view as *const _ as *mut AnyObject, subclass);
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

/// For test instances only: where the buttons are now, then while the window is resized `steps` times as a drag of
/// its edge does (each size drawn at once): the left edges of close, minimize and zoom and the close button's top, as
/// window_traffic_light_frame. And macOS's own distances between the buttons' left edges, and their width.
#[tauri::command]
pub async fn window_traffic_light_resize_steps(window: tauri::Window, steps: u32) -> Result<serde_json::Value, String> {
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
                let rest = buttons_at(window);
                let mut seen = Vec::new();
                for step in 0..steps {
                    let mut frame = window.frame();
                    frame.size.width += if step % 2 == 0 { 9.0 } else { -9.0 };
                    frame.size.height += if step % 2 == 0 { 5.0 } else { -5.0 };
                    window.setFrame_display(frame, true);
                    seen.push(buttons_at(window));
                }
                let width = close_button_frame(window).map(|f| f.2);
                let _ = tx.send(serde_json::json!({ "rest": rest, "steps": seen, "native": native_pitches(window.styleMask()), "width": width }));
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

/// (for the test above) close, minimize and zoom's left edges and the close button's top, from the window's top left
#[cfg(target_os = "macos")]
unsafe fn buttons_at(window: &objc2_app_kit::NSWindow) -> Vec<f64> {
    use objc2_app_kit::NSWindowButton;
    let mut out = Vec::new();
    for kind in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
        if let Some(button) = window.standardWindowButton(kind) {
            out.push(button.convertRect_toView(button.bounds(), None).origin.x);
        }
    }
    if let Some((_, top, _, _)) = close_button_frame(window) {
        out.push(top);
    }
    out
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
