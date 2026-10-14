//! The system clipboard. A test instance gets a private one, so tests never replace what the user copied.
use std::sync::Mutex;

use tauri::State;

#[derive(Default)]
pub struct Clipboard {
    system: Mutex<Option<arboard::Clipboard>>,
    private: Mutex<String>,
}

impl Clipboard {
    fn with<T>(&self, f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>) -> Result<T, String> {
        let mut guard = self.system.lock().unwrap();
        if guard.is_none() {
            *guard = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
        }
        f(guard.as_mut().unwrap()).map_err(|e| e.to_string())
    }

    pub fn read_text(&self) -> String {
        if crate::debug::background() {
            return self.private.lock().unwrap().clone();
        }
        self.with(|c| c.get_text()).unwrap_or_default()
    }

    fn write(&self, text: String, html: Option<String>) -> Result<(), String> {
        if crate::debug::background() {
            *self.private.lock().unwrap() = text;
            return Ok(());
        }
        match html {
            Some(html) => self.with(|c| c.set_html(html, Some(text))),
            None => self.with(|c| c.set_text(text)),
        }
    }
}

#[tauri::command]
pub fn clipboard_read(state: State<'_, Clipboard>) -> String {
    state.read_text()
}

#[tauri::command]
pub fn clipboard_write(state: State<'_, Clipboard>, text: String, html: Option<String>) -> Result<(), String> {
    state.write(text, html)
}
