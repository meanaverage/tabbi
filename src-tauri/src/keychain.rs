//! Saved passwords use keytar's service/account keys, so Electron Tabby's credentials remain accessible.
//! The macOS calls run off the async runtime.

#[cfg(target_os = "macos")]
mod imp {
    use security_framework::base::Error;
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit};
    use security_framework::passwords;

    // errSecItemNotFound
    const NOT_FOUND: i32 = -25300;

    fn message(e: Error) -> String {
        format!("{e} ({})", e.code())
    }

    pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
        match passwords::get_generic_password(service, account) {
            Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(message(e)),
        }
    }

    pub fn set(service: &str, account: &str, password: &str) -> Result<(), String> {
        passwords::set_generic_password(service, account, password.as_bytes()).map_err(message)
    }

    pub fn delete(service: &str, account: &str) -> Result<bool, String> {
        match passwords::delete_generic_password(service, account) {
            Ok(()) => Ok(true),
            Err(e) if e.code() == NOT_FOUND => Ok(false),
            Err(e) => Err(message(e)),
        }
    }

    /// Accounts first, then each one's password: the keychain won't return data for more than one item at once.
    pub fn find(service: &str) -> Result<Vec<(String, String)>, String> {
        let results = match ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(service)
            .load_attributes(true)
            .limit(Limit::All)
            .search()
        {
            Ok(results) => results,
            Err(e) if e.code() == NOT_FOUND => return Ok(Vec::new()),
            Err(e) => return Err(message(e)),
        };
        let mut found = Vec::new();
        for account in results.iter().filter_map(|r| r.simplify_dict()?.get("acct").cloned()) {
            if let Some(password) = get(service, &account)? {
                found.push((account, password));
            }
        }
        Ok(found)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    const UNSUPPORTED: &str = "Saved passwords are not supported on this platform yet";

    pub fn get(_: &str, _: &str) -> Result<Option<String>, String> {
        Err(UNSUPPORTED.into())
    }
    pub fn set(_: &str, _: &str, _: &str) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    pub fn delete(_: &str, _: &str) -> Result<bool, String> {
        Err(UNSUPPORTED.into())
    }
    pub fn find(_: &str) -> Result<Vec<(String, String)>, String> {
        Err(UNSUPPORTED.into())
    }
}

/// A plain file standing in for the keychain in test instances on macOS. Every build is signed differently, so a
/// build reading an item another build saved makes macOS ask the user for their login password, in the middle of
/// their work. Test instances (see debug::background) use `$TABBI_DATA_DIR/test-keychain.json` instead, or
/// `TABBI_KEYCHAIN_FILE`; `TABBI_REAL_KEYCHAIN=1` opts a test back into the real keychain.
mod file {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    pub fn path() -> Option<PathBuf> {
        if let Some(file) = std::env::var_os("TABBI_KEYCHAIN_FILE") {
            return Some(PathBuf::from(file));
        }
        if !cfg!(target_os = "macos") || !crate::debug::background() || std::env::var_os("TABBI_REAL_KEYCHAIN").is_some() {
            return None;
        }
        let dir = std::env::var_os("TABBI_DATA_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        Some(dir.join("test-keychain.json"))
    }

    fn load(path: &PathBuf) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn save(path: &PathBuf, items: &BTreeMap<String, BTreeMap<String, String>>) -> Result<(), String> {
        use std::io::Write;
        let tmp = path.with_extension("json.new");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut f = options.open(&tmp).map_err(|e| e.to_string())?;
        f.write_all(serde_json::to_string_pretty(items).map_err(|e| e.to_string())?.as_bytes()).map_err(|e| e.to_string())?;
        drop(f);
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }

    pub fn get(path: &PathBuf, service: &str, account: &str) -> Result<Option<String>, String> {
        let _lock = LOCK.lock().unwrap();
        Ok(load(path)?.get(service).and_then(|s| s.get(account)).cloned())
    }

    pub fn set(path: &PathBuf, service: &str, account: &str, password: &str) -> Result<(), String> {
        let _lock = LOCK.lock().unwrap();
        let mut items = load(path)?;
        items.entry(service.into()).or_default().insert(account.into(), password.into());
        save(path, &items)
    }

    pub fn delete(path: &PathBuf, service: &str, account: &str) -> Result<bool, String> {
        let _lock = LOCK.lock().unwrap();
        let mut items = load(path)?;
        let found = items.get_mut(service).and_then(|s| s.remove(account)).is_some();
        if found {
            items.retain(|_, accounts| !accounts.is_empty());
            save(path, &items)?;
        }
        Ok(found)
    }

    pub fn find(path: &PathBuf, service: &str) -> Result<Vec<(String, String)>, String> {
        let _lock = LOCK.lock().unwrap();
        Ok(load(path)?.remove(service).unwrap_or_default().into_iter().collect())
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn round_trip() {
            let path = std::env::temp_dir().join(format!("tabbi-keychain-{}.json", uuid::Uuid::new_v4()));
            assert_eq!(super::get(&path, "s", "a").unwrap(), None);
            super::set(&path, "s", "a", "p").unwrap();
            super::set(&path, "s", "b", "q").unwrap();
            assert_eq!(super::get(&path, "s", "a").unwrap().as_deref(), Some("p"));
            assert_eq!(super::find(&path, "s").unwrap(), vec![("a".into(), "p".into()), ("b".into(), "q".into())]);
            assert!(super::delete(&path, "s", "a").unwrap());
            assert!(!super::delete(&path, "s", "a").unwrap());
            std::fs::remove_file(path).unwrap();
        }
    }
}

/// The keychain service an item is kept under. With `TABBI_KEYCHAIN_PREFIX` set (test runs), every service gets
/// that prefix, so tests neither read nor change the passwords the user's own Tabby saved.
fn keychain_service(name: String) -> String {
    match std::env::var("TABBI_KEYCHAIN_PREFIX") {
        Ok(prefix) if !prefix.is_empty() => prefix + &name,
        _ => name,
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn keychain_get(service: String, account: String) -> Result<Option<String>, String> {
    let service = keychain_service(service);
    if let Some(path) = file::path() {
        return file::get(&path, &service, &account);
    }
    blocking(move || imp::get(&service, &account)).await
}

#[tauri::command]
pub async fn keychain_set(service: String, account: String, password: String) -> Result<(), String> {
    let service = keychain_service(service);
    if let Some(path) = file::path() {
        return file::set(&path, &service, &account, &password);
    }
    blocking(move || imp::set(&service, &account, &password)).await
}

#[tauri::command]
pub async fn keychain_delete(service: String, account: String) -> Result<bool, String> {
    let service = keychain_service(service);
    if let Some(path) = file::path() {
        return file::delete(&path, &service, &account);
    }
    blocking(move || imp::delete(&service, &account)).await
}

#[derive(serde::Serialize)]
pub struct Credential {
    account: String,
    password: String,
}

#[tauri::command]
pub async fn keychain_find(service: String) -> Result<Vec<Credential>, String> {
    let service = keychain_service(service);
    if let Some(path) = file::path() {
        let found = file::find(&path, &service)?;
        return Ok(found.into_iter().map(|(account, password)| Credential { account, password }).collect());
    }
    let found = blocking(move || imp::find(&service)).await?;
    Ok(found.into_iter().map(|(account, password)| Credential { account, password }).collect())
}
