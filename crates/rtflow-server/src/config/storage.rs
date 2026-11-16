//! Durable business-data storage settings.
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageSection {
    /// SQLite file. Empty means `<state_dir>/rtflow.db`.
    pub database: Option<PathBuf>,
    #[serde(with = "humantime_serde")]
    pub busy_timeout: Duration,
    /// Environment variable holding the 32-byte (hex/base64) master key.
    pub master_key_env: String,
    /// Optional file with the 32-byte (hex/base64) master key.
    pub master_key_file: Option<PathBuf>,
    /// Refuse to start when no master key source is configured instead of
    /// generating `<state_dir>/master.key`.
    pub require_master_key: bool,
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            database: None,
            busy_timeout: Duration::from_secs(5),
            master_key_env: "RTFLOW_MASTER_KEY".into(),
            master_key_file: None,
            require_master_key: false,
        }
    }
}

impl StorageSection {
    pub fn database_path(&self, state_dir: &Path) -> PathBuf {
        self.database
            .clone()
            .unwrap_or_else(|| state_dir.join("rtflow.db"))
    }

    pub fn validate(&self) -> Result<(), crate::config::ConfigError> {
        if self.busy_timeout.is_zero() {
            return Err(crate::config::ConfigError::Invalid(
                "storage.busy_timeout is zero".into(),
            ));
        }
        if self.master_key_env.trim().is_empty() && self.master_key_file.is_none() {
            return Err(crate::config::ConfigError::Invalid(
                "storage.master_key_env and storage.master_key_file are both empty".into(),
            ));
        }
        Ok(())
    }
}
