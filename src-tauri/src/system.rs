//! The installed fonts (Settings → Appearance lists them), and quitting.
use tauri::AppHandle;

/// Font family names, sorted, without the system's hidden ones (names starting with a dot).
#[tauri::command]
pub fn list_fonts() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        let names = core_text::font_collection::get_family_names();
        let mut fonts: Vec<String> = names.iter().map(|n| n.to_string()).filter(|n| !n.starts_with('.')).collect();
        fonts.sort();
        fonts.dedup();
        return fonts;
    }
    #[allow(unreachable_code)]
    Vec::new()
}

/// Tabby's PlatformService.quit: as the Quit menu item, through Tabby's close flow.
#[tauri::command]
pub fn app_quit(app: AppHandle) {
    crate::menu::quit(&app);
}
