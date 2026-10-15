//! The Dock icon's menu (right click on the icon): the profiles, recent ones first, as Electron Tabby's
//! DockMenuService builds it. Each window sends its items; the menu shows the focused window's, and a click is passed
//! back to that window by item id. Tauri has no Dock menu API, so this adds `applicationDockMenu:` to the
//! application's delegate (extending Tauri's delegate, which keeps owning the app's lifecycle).
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Runtime, WebviewWindow};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DockMenuItem {
    id: String,
    label: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DockMenuState {
    window_label: String,
    items: Vec<DockMenuItem>,
}

#[cfg(target_os = "macos")]
mod native {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Imp, Sel};
    use objc2::{ffi, sel, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
    use objc2_foundation::NSString;
    use tauri::{Emitter, Manager};

    use super::*;

    static APP: OnceLock<AppHandle> = OnceLock::new();
    static MENUS: Mutex<BTreeMap<String, Vec<DockMenuItem>>> = Mutex::new(BTreeMap::new());
    static FOCUSED: Mutex<String> = Mutex::new(String::new());

    pub fn install(app: &AppHandle) -> Result<(), String> {
        APP.set(app.clone()).map_err(|_| "The Dock menu is already installed")?;
        let mtm = MainThreadMarker::new().ok_or("The Dock menu must be installed on the main thread")?;
        let application = NSApplication::sharedApplication(mtm);
        let delegate = application.delegate().ok_or("NSApplication has no delegate")?;
        let delegate: &AnyObject = AsRef::<AnyObject>::as_ref(&*delegate);
        let class = delegate.class() as *const _ as *mut _;
        unsafe {
            let dock_imp: Imp = std::mem::transmute(
                application_dock_menu as unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject) -> *mut NSMenu,
            );
            let click_imp: Imp =
                std::mem::transmute(dock_menu_click as unsafe extern "C-unwind" fn(&AnyObject, Sel, &NSMenuItem));
            if !ffi::class_addMethod(class, sel!(applicationDockMenu:), dock_imp, c"@@:@".as_ptr()).as_bool()
                || !ffi::class_addMethod(class, sel!(tabbiDockMenuClick:), click_imp, c"v@:@".as_ptr()).as_bool()
            {
                return Err("The application delegate already has a Dock menu".into());
            }
        }
        Ok(())
    }

    pub fn window_event(label: &str, event: &tauri::WindowEvent) {
        match event {
            tauri::WindowEvent::Focused(true) => *FOCUSED.lock().unwrap() = label.into(),
            tauri::WindowEvent::Destroyed => {
                MENUS.lock().unwrap().remove(label);
            }
            _ => {}
        }
    }

    pub fn snapshot() -> DockMenuState {
        let menus = MENUS.lock().unwrap();
        let focused = FOCUSED.lock().unwrap();
        match menus.get_key_value(&*focused).or_else(|| menus.iter().next()) {
            Some((label, items)) => DockMenuState { window_label: label.clone(), items: items.clone() },
            None => DockMenuState::default(),
        }
    }

    pub fn set(label: String, items: Vec<DockMenuItem>) {
        MENUS.lock().unwrap().insert(label, items);
    }

    unsafe extern "C-unwind" fn application_dock_menu(delegate: &AnyObject, _cmd: Sel, _sender: &AnyObject) -> *mut NSMenu {
        let snapshot = snapshot();
        let mtm = MainThreadMarker::new_unchecked();
        let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("tabbi"));
        for config in &snapshot.items {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&config.label),
                Some(sel!(tabbiDockMenuClick:)),
                &NSString::from_str(""),
            );
            // The window and item are kept in the native item: items and focus can change while the menu is open.
            let value = serde_json::to_string(&(&snapshot.window_label, &config.id)).unwrap();
            item.setRepresentedObject(Some(&NSString::from_str(&value)));
            item.setTarget(Some(delegate));
            menu.addItem(&item);
        }
        Retained::autorelease_return(menu)
    }

    unsafe extern "C-unwind" fn dock_menu_click(_delegate: &AnyObject, _cmd: Sel, sender: &NSMenuItem) {
        let Some(value) = sender.representedObject() else { return };
        let value: &NSString = &*(Retained::as_ptr(&value) as *const NSString);
        let Ok((label, id)) = serde_json::from_str::<(String, String)>(&value.to_string()) else { return };
        let Some(app) = APP.get() else { return };
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.show();
            let _ = window.unminimize();
            if !crate::debug::background() {
                let _ = window.set_focus();
            }
            let _ = app.emit_to(tauri::EventTarget::labeled(label), "host:dock-menu", id);
        }
    }
}

pub fn install(app: &AppHandle) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return native::install(app);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
        Ok(())
    }
}

pub fn window_event(label: &str, event: &tauri::WindowEvent) {
    #[cfg(target_os = "macos")]
    native::window_event(label, event);
    #[cfg(not(target_os = "macos"))]
    let _ = (label, event);
}

#[tauri::command]
pub fn dock_menu_set<R: Runtime>(window: WebviewWindow<R>, items: Vec<DockMenuItem>) {
    #[cfg(target_os = "macos")]
    native::set(window.label().to_string(), items);
    #[cfg(not(target_os = "macos"))]
    let _ = (window, items);
}

/// What the Dock menu would show now (for tests).
#[tauri::command]
pub fn dock_menu_state() -> DockMenuState {
    #[cfg(target_os = "macos")]
    return native::snapshot();
    #[cfg(not(target_os = "macos"))]
    DockMenuState::default()
}
