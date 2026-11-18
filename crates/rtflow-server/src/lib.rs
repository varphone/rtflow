//! Out-of-the-box streaming server application.
pub mod api;
pub mod audit;
pub mod auth;
pub mod config;
pub mod control;
pub mod discovery;
pub mod hooks;
pub mod keys;
pub mod media;
pub mod metrics;
pub mod paths;
pub mod proxy_control;
mod pull_policy;
mod pull_schedule;
pub mod recording;
pub mod server;
pub mod srt_pull;
pub mod srt_push;
mod srt_supervisor;
mod store;
pub mod testing;
pub mod vod;

pub use config::{Config, ConfigError};

/// Storage metadata for CLI/diagnostics (does not start the server).
pub fn storage_info(config: &Config) -> Result<(u32, u64), String> {
    let store = store::Store::open(&config.storage, &config.server.state_dir)
        .map_err(|error| error.to_string())?;
    let info = store.info().map_err(|error| error.to_string())?;
    Ok((info.schema_version, info.bytes))
}

/// Consistent SQLite backup (`VACUUM INTO`), creating the DB if needed.
pub fn storage_backup(config: &Config, path: &std::path::Path) -> Result<(), String> {
    let store = store::Store::open(&config.storage, &config.server.state_dir)
        .map_err(|error| error.to_string())?;
    store.backup(path).map_err(|error| error.to_string())
}
