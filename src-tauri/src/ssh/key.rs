// Derived from russh-napi (Apache-2.0, see LICENSE-2.0.txt).
use std::sync::Arc;

use russh::keys::{HashAlg, PublicKeyBase64};
use serde::Serialize;
use tauri::State;

use super::error::{wrap, Result};
use super::{Object, Ssh};

/// russh-napi's HashAlgorithm: Sha1 = 0, Sha256 = 1, Sha512 = 2.
pub fn hash_algorithm(value: u32) -> Option<HashAlg> {
    match value {
        1 => Some(HashAlg::Sha256),
        2 => Some(HashAlg::Sha512),
        _ => None,
    }
}

/// A public key as the page's SshPublicKey has it (`bytes()` is the base64 decoded).
#[derive(Serialize, Clone)]
pub struct PublicKeyInfo {
    algorithm: String,
    fingerprint: String,
    base64: String,
}

impl From<&russh::keys::PublicKey> for PublicKeyInfo {
    fn from(key: &russh::keys::PublicKey) -> Self {
        PublicKeyInfo {
            algorithm: key.algorithm().to_string(),
            fingerprint: format!("{}", key.fingerprint(HashAlg::Sha256)),
            base64: key.public_key_base64(),
        }
    }
}

/// A key the page passes back, as `<algorithm> <base64>`.
pub fn public_key_from_page(openssh: &str) -> Result<russh::keys::PublicKey> {
    russh::keys::PublicKey::from_openssh(openssh).map_err(|e| wrap(russh::Error::from(russh::keys::Error::from(e))))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedKey {
    id: u32,
    public_key: PublicKeyInfo,
}

#[tauri::command]
pub fn ssh_parse_key(ssh: State<'_, Ssh>, webview: tauri::Webview, data: String, password: Option<String>) -> Result<ParsedKey> {
    let key = russh::keys::decode_secret_key(&data, password.as_deref()).map_err(|e| wrap(russh::Error::from(e)))?;
    let public_key = PublicKeyInfo::from(key.public_key());
    let id = ssh.add(webview.label(), Object::Key(Arc::new(key)));
    Ok(ParsedKey { id, public_key })
}

#[derive(Serialize)]
pub struct SupportedAlgorithms {
    ciphers: Vec<String>,
    kex: Vec<String>,
    macs: Vec<String>,
    compression: Vec<String>,
    keys: Vec<String>,
}

/// What this russh supports (the page has a copy for tabby-ssh, which needs it before any command could answer).
#[tauri::command]
pub fn ssh_supported_algorithms() -> SupportedAlgorithms {
    SupportedAlgorithms {
        ciphers: russh::cipher::ALL_CIPHERS.iter().map(|x| x.as_ref().to_string()).collect(),
        kex: russh::kex::ALL_KEX_ALGORITHMS.iter().map(|x| x.as_ref().to_string()).collect(),
        macs: russh::mac::ALL_MAC_ALGORITHMS.iter().map(|x| x.as_ref().to_string()).collect(),
        compression: russh::compression::ALL_COMPRESSION_ALGORITHMS.iter().map(|x| x.as_ref().to_string()).collect(),
        keys: russh::keys::key::ALL_KEY_TYPES.iter().map(|x| x.as_ref().to_string()).collect(),
    }
}
