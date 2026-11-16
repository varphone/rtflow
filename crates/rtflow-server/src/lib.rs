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
pub mod recording;
pub mod server;
pub mod srt_pull;
pub mod srt_push;
pub mod vod;

pub use config::{Config, ConfigError};
