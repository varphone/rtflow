//! SQLite business-data store.
//!
//! Service configuration stays in TOML; paths, stream keys, proxy mounts and
//! SRT endpoints live here so the admin API/UI can mutate them transactionally
//! and survive restarts.
pub(crate) mod keys;
pub(crate) mod paths;
pub(crate) mod proxies;
pub(crate) mod secret;
pub(crate) mod srt;

mod schema;

use crate::config::{ConfigError, StorageSection};
use rusqlite::Connection;
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};
use thiserror::Error;

pub(crate) use secret::SecretBox;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("invalid data: {0}")]
    Data(String),
    #[error("secret key mismatch")]
    SecretKeyMismatch,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
}

pub(crate) type StoreResult<T> = Result<T, StoreError>;

impl From<ConfigError> for StoreError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error.to_string())
    }
}

pub(crate) struct Store {
    path: PathBuf,
    connection: Mutex<Connection>,
    secrets: SecretBox,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StoreInfo {
    pub schema_version: u32,
    pub bytes: u64,
    pub path_len: usize,
}

impl Store {
    pub(crate) fn open(config: &StorageSection, state_dir: &Path) -> StoreResult<Self> {
        let path = config.database_path(state_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        let key = resolve_master_key(config, state_dir)?;
        let connection = Connection::open(&path)?;
        configure(&connection, config.busy_timeout)?;
        let mut store = Self {
            path,
            connection: Mutex::new(connection),
            secrets: SecretBox::new(key),
        };
        store.migrate()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&store.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> StoreResult<Self> {
        let connection = Connection::open_in_memory()?;
        configure(&connection, Duration::from_secs(1))?;
        let mut store = Self {
            path: PathBuf::from(":memory:"),
            connection: Mutex::new(connection),
            secrets: SecretBox::new([7u8; 32]),
        };
        store.migrate()?;
        Ok(store)
    }

    pub(crate) fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn seal(&self, aad: &str, plaintext: &[u8]) -> StoreResult<Vec<u8>> {
        Ok(self.secrets.seal(aad, plaintext))
    }

    pub(crate) fn open_secret(&self, aad: &str, ciphertext: &[u8]) -> StoreResult<Vec<u8>> {
        self.secrets
            .open(aad, ciphertext)
            .ok_or(StoreError::SecretKeyMismatch)
    }

    pub(crate) fn info(&self) -> StoreResult<StoreInfo> {
        let connection = self.connection();
        let schema_version: u32 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        Ok(StoreInfo {
            schema_version,
            bytes,
            path_len: self.path.as_os_str().len(),
        })
    }

    pub(crate) fn backup(&self, destination: &Path) -> StoreResult<()> {
        let destination = destination.to_string_lossy();
        self.connection()
            .execute("VACUUM INTO ?1", [destination.as_ref()])?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(destination.as_ref(), std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    fn migrate(&mut self) -> StoreResult<()> {
        let connection = self
            .connection
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current > schema::VERSION {
            return Err(StoreError::Data(format!(
                "database schema {current} is newer than supported {}",
                schema::VERSION
            )));
        }
        if current < schema::VERSION {
            let transaction = connection.transaction()?;
            transaction.execute_batch(schema::MIGRATION_V1)?;
            transaction.pragma_update(None, "user_version", schema::VERSION)?;
            transaction.commit()?;
        }
        Ok(())
    }
}

fn configure(connection: &Connection, busy: Duration) -> StoreResult<()> {
    connection.busy_timeout(busy)?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;
         PRAGMA trusted_schema=OFF;",
    )?;
    Ok(())
}

static KEY_LOCK: Mutex<()> = Mutex::new(());

fn resolve_master_key(config: &StorageSection, state_dir: &Path) -> StoreResult<[u8; 32]> {
    let _guard = KEY_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Ok(value) = std::env::var(&config.master_key_env)
        && !value.trim().is_empty()
    {
        return decode_key(value.trim());
    }
    let file = config
        .master_key_file
        .clone()
        .unwrap_or_else(|| state_dir.join("master.key"));
    if let Ok(text) = std::fs::read_to_string(&file) {
        return decode_key(text.trim());
    }
    if config.require_master_key {
        return Err(StoreError::Config(format!(
            "master key not found in {} or {}",
            config.master_key_env,
            file.display()
        )));
    }
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key)
        .map_err(|error| StoreError::Config(format!("cannot generate master key: {error}")))?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let encoded = hex(&key);
    let temporary = file.with_extension(format!("key.tmp.{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write as _;
    let mut handle = options.open(&temporary)?;
    handle.write_all(encoded.as_bytes())?;
    handle.sync_all()?;
    drop(handle);
    std::fs::rename(&temporary, &file)?;
    tracing::warn!(path = %file.display(), "generated a new storage master key; back it up");
    Ok(key)
}

fn decode_key(text: &str) -> StoreResult<[u8; 32]> {
    let bytes = if text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let mut bytes = Vec::with_capacity(32);
        let raw = text.as_bytes();
        for index in (0..64).step_by(2) {
            let high = hex_digit(raw[index]);
            let low = hex_digit(raw[index + 1]);
            bytes.push(high.unwrap_or(0) << 4 | low.unwrap_or(0));
        }
        bytes
    } else {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|_| StoreError::Config("master key must be 64 hex chars or base64".into()))?
    };
    bytes
        .try_into()
        .map_err(|_| StoreError::Config("master key must decode to 32 bytes".into()))
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests;
