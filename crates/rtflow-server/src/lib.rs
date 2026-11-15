//! Out-of-the-box streaming server application.
pub mod api;
pub mod auth;
pub mod config;
pub mod discovery;
pub mod hooks;
pub mod keys;
pub mod paths;
pub mod recording;
pub mod server;
pub mod vod;

pub use config::{Config, ConfigError};
