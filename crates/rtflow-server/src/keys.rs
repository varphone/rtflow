//! API-issued stream keys.
//!
//! A key is `kid.secret`; only the Argon2id hash of `secret` is persisted.
//! The secret is returned exactly once from [`KeyStore::issue`].
use crate::{
    config::{Config, glob_match},
    store::{self, Store, StoreError},
};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::Path,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const STORE_FILE: &str = "keys.json";
const STORE_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct KeySpec {
    pub name: String,
    pub roles: Vec<String>,
    /// Path globs this key may use; empty means every configured path.
    pub paths: Vec<String>,
    /// `rtsp`, `rtmp`, `srt`, `http`, `webrtc`; empty means all.
    pub protocols: Vec<String>,
    #[serde(with = "humantime_serde")]
    pub ttl: Duration,
    pub max_connections: usize,
}

impl Default for KeySpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            roles: vec!["publish".into(), "read".into()],
            paths: Vec::new(),
            protocols: Vec::new(),
            ttl: Duration::from_secs(24 * 3600),
            max_connections: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KeyRecord {
    pub kid: String,
    pub name: String,
    pub roles: Vec<String>,
    pub paths: Vec<String>,
    pub protocols: Vec<String>,
    pub max_connections: usize,
    pub created_at: u64,
    pub expires_at: u64,
    pub enabled: bool,
    pub(crate) hash: String,
    #[serde(default = "initial_revision")]
    pub revision: u64,
}

fn initial_revision() -> u64 {
    1
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyStatus {
    pub kid: String,
    pub name: String,
    pub roles: Vec<String>,
    pub paths: Vec<String>,
    pub protocols: Vec<String>,
    pub max_connections: usize,
    pub created_at: u64,
    pub expires_at: u64,
    pub enabled: bool,
}

impl From<&KeyRecord> for KeyStatus {
    fn from(record: &KeyRecord) -> Self {
        Self {
            kid: record.kid.clone(),
            name: record.name.clone(),
            roles: record.roles.clone(),
            paths: record.paths.clone(),
            protocols: record.protocols.clone(),
            max_connections: record.max_connections,
            created_at: record.created_at,
            expires_at: record.expires_at,
            enabled: record.enabled,
        }
    }
}

pub struct IssuedKey {
    pub status: KeyStatus,
    /// Returned once to the caller; never stored in plaintext.
    pub secret: String,
}

#[derive(Deserialize, Serialize)]
struct StoreFile {
    version: u32,
    keys: Vec<KeyRecord>,
}

pub struct KeyStore {
    store: Arc<Store>,
    keys: RwLock<Vec<KeyRecord>>,
}

impl KeyStore {
    pub fn new(config: &Config) -> Result<Self, KeyError> {
        let store = Arc::new(
            Store::open(&config.storage, &config.server.state_dir)
                .map_err(key_store_error)?,
        );
        Self::with_store(config, store)
    }

    pub(crate) fn with_store(config: &Config, store: Arc<Store>) -> Result<Self, KeyError> {
        let mut keys = store::keys::load_all(&store).map_err(key_store_error)?;
        if keys.is_empty() {
            let file = config.server.state_dir.join(STORE_FILE);
            keys = load_file(&file)?;
            for key in &keys {
                store::keys::insert(&store, key).map_err(key_store_error)?;
            }
        }
        Ok(Self {
            store,
            keys: RwLock::new(keys),
        })
    }

    pub fn list(&self) -> Vec<KeyStatus> {
        self.keys
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(KeyStatus::from)
            .collect()
    }

    pub fn issue(&self, spec: KeySpec) -> Result<IssuedKey, KeyError> {
        let now = unix_now();
        let ttl = if spec.ttl.is_zero() {
            Duration::from_secs(24 * 3600)
        } else {
            spec.ttl
        };
        let mut kid_bytes = [0u8; 12];
        getrandom::getrandom(&mut kid_bytes)
            .map_err(|error| KeyError::Crypto(error.to_string()))?;
        let kid = format!("rk_{}", URL_SAFE_NO_PAD.encode(kid_bytes));
        let mut secret_bytes = [0u8; 32];
        getrandom::getrandom(&mut secret_bytes)
            .map_err(|error| KeyError::Crypto(error.to_string()))?;
        let secret = URL_SAFE_NO_PAD.encode(secret_bytes);
        let salt = SaltString::encode_b64(&kid_bytes)
            .map_err(|error| KeyError::Crypto(error.to_string()))?;
        let hash = Argon2::default()
            .hash_password(secret.as_bytes(), &salt)
            .map_err(|error| KeyError::Crypto(error.to_string()))?
            .to_string();
        let record = KeyRecord {
            kid: kid.clone(),
            name: if spec.name.is_empty() {
                "stream key".into()
            } else {
                spec.name
            },
            roles: spec.roles,
            paths: spec.paths,
            protocols: spec.protocols,
            max_connections: spec.max_connections,
            created_at: now,
            expires_at: now.saturating_add(ttl.as_secs()),
            enabled: true,
            hash,
            revision: 1,
        };
        store::keys::insert(&self.store, &record).map_err(key_store_error)?;
        self.keys
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record.clone());
        Ok(IssuedKey {
            status: KeyStatus::from(&record),
            secret,
        })
    }

    pub fn revoke(&self, kid: &str) -> Result<(), KeyError> {
        let mut keys = self
            .keys
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(key) = keys.iter_mut().find(|key| key.kid == kid) else {
            return Err(KeyError::NotFound(kid.into()));
        };
        let next_revision = key.revision.saturating_add(1);
        store::keys::set_enabled(&self.store, kid, false, next_revision)
            .map_err(key_store_error)?;
        key.enabled = false;
        key.revision = next_revision;
        Ok(())
    }

    pub fn verify(&self, token: &str, path: &str, role: &str, protocol: &str) -> Option<KeyRecord> {
        let (kid, secret) = token.rsplit_once('.')?;
        let keys = self
            .keys
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = keys.iter().find(|key| key.kid == kid && key.enabled)?;
        if key.expires_at <= unix_now() {
            return None;
        }
        if !key.roles.iter().any(|allowed| allowed == role) {
            return None;
        }
        if !key.protocols.is_empty()
            && !key
                .protocols
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(protocol))
        {
            return None;
        }
        if !key.paths.is_empty()
            && !key
                .paths
                .iter()
                .any(|pattern| glob_match(pattern, path.trim_start_matches('/')))
        {
            return None;
        }
        let parsed = PasswordHash::new(&key.hash).ok()?;
        Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .ok()?;
        Some(key.clone())
    }
}

fn load_file(path: &Path) -> Result<Vec<KeyRecord>, KeyError> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    let file: StoreFile = serde_json::from_str(&text)
        .map_err(|error| KeyError::Corrupt(format!("{}: {error}", path.display())))?;
    if file.version != STORE_VERSION {
        return Err(KeyError::Corrupt(format!(
            "{}: unsupported version {}",
            path.display(),
            file.version
        )));
    }
    Ok(file.keys)
}

fn key_store_error(error: StoreError) -> KeyError {
    match error {
        StoreError::NotFound(_) => KeyError::NotFound(String::new()),
        other => KeyError::Io(other.to_string()),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("key {0} not found")]
    NotFound(String),
    #[error("key store: {0}")]
    Io(String),
    #[error("key store is corrupt: {0}")]
    Corrupt(String),
    #[error("crypto: {0}")]
    Crypto(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_verify_revoke() {
        let dir = std::env::temp_dir().join(format!("rtflow-keys-{}", std::process::id()));
        let mut config = Config::default();
        config.server.state_dir = dir.clone();
        let store = KeyStore::new(&config).unwrap();
        let issued = store
            .issue(KeySpec {
                ttl: Duration::from_secs(60),
                ..KeySpec::default()
            })
            .unwrap();
        let token = format!("{}.{}", issued.status.kid, issued.secret);
        assert!(
            store
                .verify(&token, "live/cam", "publish", "rtmp")
                .is_some()
        );
        assert!(store.verify(&token, "live/cam", "read", "rtmp").is_some());
        store.revoke(&issued.status.kid).unwrap();
        assert!(
            store
                .verify(&token, "live/cam", "publish", "rtmp")
                .is_none()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scope_is_enforced() {
        let dir = std::env::temp_dir().join(format!("rtflow-keys-scope-{}", std::process::id()));
        let mut config = Config::default();
        config.server.state_dir = dir.clone();
        let store = KeyStore::new(&config).unwrap();
        let issued = store
            .issue(KeySpec {
                roles: vec!["publish".into()],
                paths: vec!["live/*".into()],
                protocols: vec!["rtmp".into()],
                ..KeySpec::default()
            })
            .unwrap();
        let token = format!("{}.{}", issued.status.kid, issued.secret);
        assert!(
            store
                .verify(&token, "live/cam", "publish", "rtmp")
                .is_some()
        );
        assert!(store.verify(&token, "live/cam", "read", "rtmp").is_none());
        assert!(
            store
                .verify(&token, "private/cam", "publish", "rtmp")
                .is_none()
        );
        assert!(store.verify(&token, "live/cam", "publish", "srt").is_none());
        let _ = fs::remove_dir_all(dir);
    }
}
