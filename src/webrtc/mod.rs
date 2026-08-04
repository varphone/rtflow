//! WebRTC protocol support with WHIP/WHEP endpoints.
//!
//! Provides WebRTC ingestion (WHIP) and egress (WHEP) over HTTP,
//! using webrtc-ice and webrtc-dtls for connectivity and security.
//!
//! # Architecture
//!
//! - **Signaling**: WHIP/WHEP over HTTP (SDP offer/answer)
//! - **ICE**: Host candidates via webrtc-ice (LAN/embedded scenarios)
//! - **DTLS**: DTLS 1.2 via webrtc-dtls with SRTP extension
//! - **SRTP**: Reuses rtflow's `src/srtp/` module
//! - **RTP**: Reuses rtflow's `src/rtp/` packetizer/depacketizer
//!
//! # Example
//!
//! ```no_run
//! use rtflow::webrtc::{WebRtcConfig, create_webrtc_handlers};
//! use rtflow::mount::MountRegistry;
//! use std::sync::Arc;
//!
//! # async fn example() {
//! let mounts = Arc::new(MountRegistry::new(100));
//! let config = WebRtcConfig::default();
//! let (whip_handler, whep_handler) = create_webrtc_handlers(config, mounts);
//! // Register handlers with HTTP server router...
//! # }
//! ```
pub mod dtls;
pub mod ice;
pub mod opus_payload;
pub mod sdp_ext;
pub mod session;
pub mod whep;
pub mod whip;

use crate::http::Handler;
use crate::mount::MountRegistry;
use session::SessionRegistry;
use std::sync::Arc;
use std::time::Duration;
use whep::WhepConfig;
use whip::WhipConfig;

/// WebRTC configuration.
#[derive(Clone, Debug)]
pub struct WebRtcConfig {
    /// WHIP (ingest) configuration.
    pub whip: WhipConfig,
    /// WHEP (playback) configuration.
    pub whep: WhepConfig,
    /// Session timeout duration.
    pub session_timeout: Duration,
    /// Optional metrics registry for protocol-level counters.
    #[cfg(feature = "metrics")]
    pub metrics: Option<Arc<crate::metrics::MetricsRegistry>>,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            whip: WhipConfig::default(),
            whep: WhepConfig::default(),
            session_timeout: Duration::from_secs(60),
            #[cfg(feature = "metrics")]
            metrics: None,
        }
    }
}

/// Create WebRTC HTTP handlers for WHIP and WHEP.
///
/// Returns two handlers:
/// 1. WHIP handler (for `/whip/*` routes)
/// 2. WHEP handler (for `/whep/*` routes)
pub fn create_webrtc_handlers(
    config: WebRtcConfig,
    mounts: Arc<MountRegistry>,
) -> (Handler, Handler) {
    // Create shared session registry
    let sessions = match SessionRegistry::new(config.session_timeout) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!("Failed to create WebRTC session registry: {e}");
            // Return dummy handlers that return 500
            let error_handler: Handler = Arc::new(|_req, mut writer| {
                Box::pin(async move {
                    writer
                        .write_body(500, "text/plain", b"WebRTC initialization failed")
                        .await?;
                    Ok(writer)
                })
            });
            return (error_handler.clone(), error_handler);
        }
    };

    // Periodically reap expired sessions so abandoned WHIP/WHEP sessions
    // cannot accumulate in the registry.
    {
        let cleanup_sessions = sessions.clone();
        let cleanup_interval = (config.session_timeout / 2).max(Duration::from_secs(1));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(cleanup_interval);
            loop {
                ticker.tick().await;
                let removed = cleanup_sessions.cleanup_expired().await;
                if removed > 0 {
                    tracing::info!("WebRTC: reaped {removed} expired session(s)");
                }
            }
        });
    }

    // WHIP handler
    let whip_config = config.whip.clone();
    let whip_mounts = mounts.clone();
    let whip_sessions = sessions.clone();
    let whip_handler: Handler = Arc::new(move |req, writer| {
        let mounts = whip_mounts.clone();
        let config = whip_config.clone();
        let sessions = whip_sessions.clone();
        let body = req.body.clone();
        Box::pin(async move {
            match req.method {
                crate::http::Method::Post => {
                    whip::handle_whip_post(req, writer, &body, mounts, &config, sessions).await
                }
                crate::http::Method::Delete => {
                    whip::handle_whip_delete(req, writer, sessions).await
                }
                _ => whip::handle_whip_options(req, writer).await,
            }
        })
    });

    // WHEP handler
    let whep_config = config.whep.clone();
    let whep_mounts = mounts.clone();
    let whep_sessions = sessions.clone();
    let whep_handler: Handler = Arc::new(move |req, writer| {
        let mounts = whep_mounts.clone();
        let config = whep_config.clone();
        let sessions = whep_sessions.clone();
        let body = req.body.clone();
        Box::pin(async move {
            match req.method {
                crate::http::Method::Post => {
                    whep::handle_whep_post(req, writer, &body, mounts, &config, sessions).await
                }
                crate::http::Method::Delete => {
                    whep::handle_whep_delete(req, writer, sessions).await
                }
                _ => whep::handle_whep_options(req, writer).await,
            }
        })
    });

    (whip_handler, whep_handler)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults() {
        let config = WebRtcConfig::default();
        assert_eq!(config.whip.public_ip, "127.0.0.1");
        assert_eq!(config.whep.video_payload_type, 96);
        assert_eq!(config.session_timeout, Duration::from_secs(60));
    }
}
