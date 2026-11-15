//! One-call logging setup for applications built on `rtflow`.
//!
//! The framework emits structured [`tracing`] events (`info` for lifecycle,
//! `debug` for protocol detail, `warn`/`error` for failures). Installing a
//! subscriber is the application's job; [`init`] provides a ready-made
//! environment-variable-driven default:
//!
//! ```no_run
//! rtflow::logging::init();
//! ```
//!
//! `RUST_LOG` accepts the usual `tracing` filter syntax, e.g.
//! `RUST_LOG=rtflow=debug,rtflow_hls=trace`.
//!
//! Applications with their own subscriber (JSON logs, OpenTelemetry, a
//! different writer) should skip this module and install theirs directly.
use std::sync::OnceLock;

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

static INIT: OnceLock<bool> = OnceLock::new();

/// Options for [`init_with`].
#[derive(Clone, Debug)]
pub struct LogConfig {
    /// `RUST_LOG`-style filter. `None` reads `RUST_LOG`, falling back to
    /// `info`.
    pub filter: Option<String>,
    /// Include the event target (crate/module) in each line.
    pub target: bool,
    /// Include file/line of the event.
    pub line_number: bool,
    /// Include the thread id (useful to tell accept loops from media tasks).
    pub thread_ids: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: None,
            target: true,
            line_number: false,
            thread_ids: true,
        }
    }
}

/// Install the default global subscriber. Returns `false` when a subscriber
/// was already installed (including by a previous call in the same process).
///
/// The filter comes from `RUST_LOG`, defaulting to `info`; malformed filters
/// are ignored with a warning rather than panicking at startup.
pub fn init() -> bool {
    init_with(LogConfig::default())
}

/// Install the default subscriber with explicit [`LogConfig`].
pub fn init_with(config: LogConfig) -> bool {
    let mut installed = false;
    INIT.get_or_init(|| {
        let filter = config
            .filter
            .clone()
            .or_else(|| std::env::var("RUST_LOG").ok())
            .unwrap_or_else(|| "info".to_owned());
        let filter = match EnvFilter::try_new(&filter) {
            Ok(filter) => filter,
            Err(error) => {
                eprintln!("rtflow: invalid RUST_LOG filter {filter:?}: {error}; using info");
                EnvFilter::new("info")
            }
        };
        let subscriber = tracing_subscriber::registry().with(filter).with(
            fmt::layer()
                .with_target(config.target)
                .with_line_number(config.line_number)
                .with_thread_ids(config.thread_ids)
                .with_thread_names(config.thread_ids),
        );
        if subscriber.try_init().is_ok() {
            installed = true;
        }
        installed
    });
    *INIT.get().unwrap_or(&false)
}

/// The canonical `RUST_LOG` value for debugging a full pipeline without
/// flooding: lifecycle at info, protocol adapters at debug, per-frame paths
/// only at trace.
pub const DEBUG_FILTER: &str = "info,rtflow_rtsp_server=debug,rtflow_rtsp_client=debug,rtflow_mount=debug,rtflow_hls=debug,rtflow_dash=debug,rtflow_http_flv=debug,rtflow_rtmp=debug,rtflow_srt=debug,rtflow_webrtc=debug";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_debuggable() {
        let config = LogConfig::default();
        assert!(config.target);
        assert!(config.thread_ids);
        assert!(!config.line_number);
        assert!(DEBUG_FILTER.contains("rtflow_rtsp_server=debug"));
        assert!(DEBUG_FILTER.starts_with("info,"));
    }
}
