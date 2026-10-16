#[cfg(not(target_os = "linux"))]
use std::sync::Mutex;

use tauri::State;

#[derive(Default)]
pub struct Clipboard(#[cfg(not(target_os = "linux"))] Mutex<Option<arboard::Clipboard>>);

#[cfg(not(target_os = "linux"))]
fn with<T>(state: &Clipboard, f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>) -> Result<T, String> {
    let mut guard = state.0.lock().unwrap();
    if guard.is_none() {
        *guard = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
    }
    f(guard.as_mut().unwrap()).map_err(|e| e.to_string())
}

/// Test instances on macOS (see debug::background) copy and paste on a clipboard of their own, never the user's:
/// the user works on the same Mac while tests run. `TABBZ_REAL_CLIPBOARD=1` opts a test back into the real one.
mod private {
    use std::sync::Mutex;

    pub struct Board { pub text: Option<String>, pub html: Option<String>, pub png: Vec<u8>, pub change: u64 }
    pub static BOARD: Mutex<Board> = Mutex::new(Board { text: None, html: None, png: Vec::new(), change: 0 });

    pub fn active() -> bool {
        cfg!(target_os = "macos") && crate::debug::background()
    }

    pub fn read() -> super::Contents {
        let board = BOARD.lock().unwrap();
        super::Contents { text: board.text.clone(), html: board.html.clone(), change: board.change.to_string(), png: board.png.clone() }
    }

    pub fn write(contents: super::Contents) {
        let mut board = BOARD.lock().unwrap();
        board.text = contents.text;
        board.html = contents.html;
        board.png = contents.png;
        board.change += 1;
    }
}

#[cfg_attr(target_os = "linux", tauri::command(async))]
#[cfg_attr(not(target_os = "linux"), tauri::command)]
pub fn clipboard_read(app: tauri::AppHandle, state: State<'_, Clipboard>) -> String {
    if private::active() { return private::read().text.unwrap_or_default(); }
    #[cfg(target_os = "linux")]
    { let _ = state; linux::run(&app, |c| Ok(c.wait_for_text().map(|s| s.to_string()).unwrap_or_default())).unwrap_or_default() }
    #[cfg(not(target_os = "linux"))]
    { let _ = app; with(&state, |c| c.get_text()).unwrap_or_default() }
}

#[cfg_attr(target_os = "linux", tauri::command(async))]
#[cfg_attr(not(target_os = "linux"), tauri::command)]
pub fn clipboard_write(app: tauri::AppHandle, state: State<'_, Clipboard>, text: String, html: Option<String>) -> Result<(), String> {
    if private::active() {
        private::write(Contents { text: Some(text), html, ..Default::default() });
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    { let _ = state; linux::write(&app, Contents { text: Some(text), html, ..Default::default() }) }
    #[cfg(not(target_os = "linux"))]
    { let _ = app; match html {
        Some(html) => with(&state, |c| c.set_html(html, Some(text))),
        None => with(&state, |c| c.set_text(text)),
    } }
}

/// Files copied in Finder (Explorer, Files): their paths. Empty when the clipboard holds none.
#[cfg_attr(target_os = "linux", tauri::command(async))]
#[cfg_attr(not(target_os = "linux"), tauri::command)]
pub fn clipboard_files(app: tauri::AppHandle, state: State<'_, Clipboard>) -> Vec<String> {
    if private::active() { return Vec::new(); }
    #[cfg(target_os = "linux")]
    { let _ = state; linux::run(&app, |c| {
        use gtk::prelude::FileExt;
        Ok(c.wait_for_uris().iter().filter_map(|uri| gtk::gio::File::for_uri(uri).path())
            .map(|p| p.to_string_lossy().into_owned()).collect())
    }).unwrap_or_default() }
    #[cfg(not(target_os = "linux"))]
    { let _ = app; with(&state, |c| c.get().file_list()).unwrap_or_default().into_iter().map(|p| p.to_string_lossy().into_owned()).collect() }
}

/// A picture onto the clipboard. The body is its RGBA pixels; headers `width` and `height`.
#[cfg_attr(target_os = "linux", tauri::command(async))]
#[cfg_attr(not(target_os = "linux"), tauri::command)]
pub fn clipboard_write_image(app: tauri::AppHandle, state: State<'_, Clipboard>, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let header = |k: &str| request.headers().get(k).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok());
    let (Some(width), Some(height)) = (header("width"), header("height")) else {
        return Err("no width or height".into());
    };
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw bytes".into());
    };
    if width == 0 || height == 0 || width.checked_mul(height).and_then(|n| n.checked_mul(4)) != Some(bytes.len()) {
        return Err(format!("{} bytes is not {width}×{height} RGBA", bytes.len()));
    }
    if private::active() {
        use image::ImageEncoder;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png).write_image(bytes, width.try_into().map_err(|_| "invalid width")?, height.try_into().map_err(|_| "invalid height")?, image::ExtendedColorType::Rgba8).map_err(|e| e.to_string())?;
        private::write(Contents { png, ..Default::default() });
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        use image::ImageEncoder;
        let _ = state;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png).write_image(bytes, width.try_into().map_err(|_| "invalid width")?, height.try_into().map_err(|_| "invalid height")?, image::ExtendedColorType::Rgba8).map_err(|e| e.to_string())?;
        linux::write(&app, Contents { png, ..Default::default() })
    }
    #[cfg(not(target_os = "linux"))]
    { let _ = app; with(&state, |c| c.set_image(arboard::ImageData { width, height, bytes: std::borrow::Cow::Borrowed(bytes) })) }
}

#[cfg(not(target_os = "linux"))]
fn optional<T>(result: Result<T, arboard::Error>) -> Result<Option<T>, arboard::Error> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Contents {
    text: Option<String>,
    html: Option<String>,
    #[serde(default)]
    change: String,
    #[serde(skip)]
    png: Vec<u8>,
}

#[cfg(not(target_os = "linux"))]
fn read_contents(state: &Clipboard) -> Result<Contents, String> {
    let mut contents = with(state, |c| Ok(Contents {
        text: optional(c.get_text())?,
        html: optional(c.get().html())?,
        ..Default::default()
    }))?;
    #[cfg(target_os = "macos")]
    if let Some(png) = macos::read_png() {
        contents.png = png;
        return Ok(contents);
    }
    if let Some(image) = with(state, |c| optional(c.get_image()))? {
        use image::ImageEncoder;
        let width = u32::try_from(image.width).map_err(|e| e.to_string())?;
        let height = u32::try_from(image.height).map_err(|e| e.to_string())?;
        image::codecs::png::PngEncoder::new(&mut contents.png)
            .write_image(&image.bytes, width, height, image::ExtendedColorType::Rgba8)
            .map_err(|e| e.to_string())?;
    }
    Ok(contents)
}

// Strings preserve the full native counter/hash without JavaScript's 53-bit integer limit.
fn change_count(app: &tauri::AppHandle, state: &Clipboard) -> Result<String, String> {
    let _ = app;
    if private::active() { return Ok(private::read().change); }
    #[cfg(target_os = "macos")]
    {
        let _ = state;
        Ok(macos::change_count())
    }
    #[cfg(windows)]
    {
        let _ = state;
        #[link(name = "user32")]
        extern "system" { fn GetClipboardSequenceNumber() -> u32; }
        Ok(unsafe { GetClipboardSequenceNumber() }.to_string())
    }
    #[cfg(target_os = "linux")]
    { let _ = state; linux::run(app, |_| Ok(linux::revision().to_string())) }
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    {
        use std::hash::{Hash, Hasher};
        with(state, |c| {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            optional(c.get_text())?.hash(&mut hash);
            optional(c.get().html())?.hash(&mut hash);
            if let Some(image) = optional(c.get_image())? {
                image.width.hash(&mut hash);
                image.height.hash(&mut hash);
                image.bytes.hash(&mut hash);
            }
            Ok(hash.finish().to_string())
        })
    }
}

#[cfg_attr(target_os = "linux", tauri::command(async))]
#[cfg_attr(not(target_os = "linux"), tauri::command)]
pub fn clipboard_change_count(app: tauri::AppHandle, state: State<'_, Clipboard>) -> Result<String, String> {
    change_count(&app, &state)
}

/// Raw packet: u32 LE JSON length, UTF-8 {text, html, change}, then optional PNG bytes.
#[cfg_attr(target_os = "linux", tauri::command(async))]
#[cfg_attr(not(target_os = "linux"), tauri::command)]
pub fn clipboard_read_items(app: tauri::AppHandle, state: State<'_, Clipboard>) -> Result<tauri::ipc::Response, String> {
    for _ in 0..3 {
        let before = change_count(&app, &state)?;
        #[cfg(not(target_os = "linux"))]
        let mut contents = if private::active() { private::read() } else { read_contents(&state)? };
        #[cfg(target_os = "linux")]
        let mut contents = linux::run(&app, linux::read)?;
        if before != change_count(&app, &state)? {
            continue;
        }
        contents.change = before;
        let json = serde_json::to_vec(&contents).map_err(|e| e.to_string())?;
        let length = u32::try_from(json.len()).map_err(|e| e.to_string())?;
        let mut bytes = Vec::with_capacity(4 + json.len() + contents.png.len());
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&json);
        bytes.extend_from_slice(&contents.png);
        return Ok(tauri::ipc::Response::new(bytes));
    }
    Err("clipboard changed while reading".into())
}

fn unpack(bytes: &[u8]) -> Result<Contents, String> {
    let prefix = bytes.get(..4).ok_or("missing clipboard header")?;
    let length = u32::from_le_bytes(prefix.try_into().unwrap()) as usize;
    let end = 4_usize.checked_add(length).ok_or("invalid clipboard header length")?;
    let mut contents: Contents = serde_json::from_slice(bytes.get(4..end).ok_or("invalid clipboard header length")?)
        .map_err(|e| e.to_string())?;
    contents.png = bytes[end..].to_vec();
    if !contents.png.is_empty() && !contents.png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("expected PNG image".into());
    }
    Ok(contents)
}

#[cfg_attr(target_os = "linux", tauri::command(async))]
#[cfg_attr(not(target_os = "linux"), tauri::command)]
pub fn clipboard_write_items(app: tauri::AppHandle, state: State<'_, Clipboard>, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw bytes".into());
    };
    let contents = unpack(bytes)?;
    if private::active() {
        private::write(contents);
        return Ok(());
    }
    #[cfg(not(target_os = "linux"))]
    let _ = app;
    #[cfg(target_os = "macos")]
    {
        let _guard = state.0.lock().unwrap();
        macos::write(&contents)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = state;
        linux::write(&app, contents)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        if !contents.png.is_empty() {
            if contents.text.is_some() || contents.html.is_some() {
                return Err("combined image and text clipboard items are not supported on this platform".into());
            }
            let image = image::load_from_memory_with_format(&contents.png, image::ImageFormat::Png)
                .map_err(|e| e.to_string())?.into_rgba8();
            return with(&state, |c| c.set_image(arboard::ImageData {
                width: image.width() as usize,
                height: image.height() as usize,
                bytes: std::borrow::Cow::Owned(image.into_raw()),
            }));
        }
        with(&state, |c| match contents.html {
            Some(html) => c.set_html(html, contents.text),
            None => match contents.text {
                Some(text) => c.set_text(text),
                None => c.clear(),
            },
        })
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use gtk::{gdk, prelude::*, Clipboard, TargetEntry, TargetFlags};
    use std::sync::atomic::{AtomicU64, Ordering};

    static REVISION: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static CLIPBOARD: Clipboard = {
            let clipboard = Clipboard::get(&gdk::SELECTION_CLIPBOARD);
            clipboard.connect_local("owner-change", false, |_| {
                REVISION.fetch_add(1, Ordering::Relaxed);
                None
            });
            clipboard
        };
    }

    pub fn revision() -> u64 { REVISION.load(Ordering::Relaxed) }

    // Commands run off the UI thread; GTK owns the selection and serves requests on its main loop.
    pub fn run<T: Send + 'static>(app: &tauri::AppHandle, f: impl FnOnce(&Clipboard) -> Result<T, String> + Send + 'static) -> Result<T, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        app.run_on_main_thread(move || CLIPBOARD.with(|c| { let _ = tx.send(f(c)); })).map_err(|e| e.to_string())?;
        rx.recv().map_err(|e| e.to_string())?
    }

    pub fn read(clipboard: &Clipboard) -> Result<super::Contents, String> {
        let targets = clipboard.wait_for_targets().unwrap_or_default();
        let data = |mime: &str| {
            let atom = gdk::Atom::intern(mime);
            targets.contains(&atom).then(|| clipboard.wait_for_contents(&atom)).flatten()
                .filter(|s| s.length() >= 0).map(|s| s.data())
        };
        let html = data("text/html").map(String::from_utf8).transpose().map_err(|e| e.to_string())?;
        let png = match data("image/png") {
            Some(png) => png,
            None => clipboard.wait_for_image().map(|p| p.save_to_bufferv("png", &[])).transpose().map_err(|e| e.to_string())?.unwrap_or_default(),
        };
        Ok(super::Contents { text: clipboard.wait_for_text().map(|s| s.to_string()), html, png, ..Default::default() })
    }

    pub fn write(app: &tauri::AppHandle, contents: super::Contents) -> Result<(), String> {
        if !contents.png.is_empty() {
            image::load_from_memory_with_format(&contents.png, image::ImageFormat::Png).map_err(|e| e.to_string())?;
        }
        run(app, move |clipboard| {
            let mut formats = Vec::new();
            if let Some(text) = contents.text {
                for target in ["UTF8_STRING", "text/plain;charset=utf-8", "text/plain"] {
                    formats.push((target, text.as_bytes().to_vec()));
                }
            }
            if let Some(html) = contents.html { formats.push(("text/html", html.into_bytes())); }
            if !contents.png.is_empty() { formats.push(("image/png", contents.png)); }
            REVISION.fetch_add(1, Ordering::Relaxed);
            if formats.is_empty() { clipboard.clear(); return Ok(()); }
            let targets: Vec<_> = formats.iter().enumerate()
                .map(|(index, (mime, _))| TargetEntry::new(mime, TargetFlags::empty(), index as u32)).collect();
            if !clipboard.set_with_data(&targets, move |_, selection, index| {
                if let Some((mime, bytes)) = formats.get(index as usize) {
                    selection.set(&gdk::Atom::intern(mime), 8, bytes);
                }
            }) { return Err("could not own the clipboard selection".into()); }
            Ok(())
        })
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::{rc::autoreleasepool, runtime::ProtocolObject};
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeHTML, NSPasteboardTypePNG, NSPasteboardTypeString};
    use objc2_foundation::{NSArray, NSData, NSString};

    pub fn change_count() -> String {
        autoreleasepool(|_| NSPasteboard::generalPasteboard().changeCount().to_string())
    }

    pub fn read_png() -> Option<Vec<u8>> {
        autoreleasepool(|_| unsafe {
            NSPasteboard::generalPasteboard().dataForType(NSPasteboardTypePNG).map(|data| data.to_vec())
        })
    }

    pub fn write(contents: &super::Contents) -> Result<(), String> {
        autoreleasepool(|_| unsafe {
            let pasteboard = NSPasteboard::generalPasteboard();
            let item = NSPasteboardItem::new();
            for (text, kind) in [(&contents.text, NSPasteboardTypeString), (&contents.html, NSPasteboardTypeHTML)] {
                if let Some(text) = text {
                    if !item.setString_forType(&NSString::from_str(text), kind) {
                        return Err("could not set clipboard text".into());
                    }
                }
            }
            if !contents.png.is_empty() && !item.setData_forType(&NSData::with_bytes(&contents.png), NSPasteboardTypePNG) {
                return Err("could not set clipboard PNG".into());
            }
            pasteboard.clearContents();
            if contents.text.is_none() && contents.html.is_none() && contents.png.is_empty() {
                return Ok(());
            }
            if !pasteboard.writeObjects(&NSArray::from_retained_slice(&[ProtocolObject::from_retained(item)])) {
                return Err("could not write clipboard item".into());
            }
            Ok(())
        })
    }
}

impl Clipboard {
    pub fn read_text(&self) -> String {
        if private::active() { return private::read().text.unwrap_or_default(); }
        with(self, |c| c.get_text()).unwrap_or_default()
    }
    pub fn file_paths(&self) -> Vec<String> {
        if private::active() { return Vec::new(); }
        with(self, |c| c.get().file_list()).unwrap_or_default().into_iter().map(|p| p.to_string_lossy().into_owned()).collect()
    }
}
