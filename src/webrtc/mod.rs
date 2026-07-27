//! WebRTC protocol support with WHIP/WHEP endpoints.
//!
//! Provides WebRTC ingestion (WHIP) and egress (WHEP) over HTTP,
//! using the existing SRTP/RTP infrastructure for media transport.
//!
//! # Architecture
//!
//! - **Signaling**: WHIP/WHEP over HTTP (SDP offer/answer)
//! - **ICE**: Host candidates only (LAN/embedded scenarios)
//! - **DTLS**: Placeholder for webrtc-dtls integration
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
pub mod opus_payload;
pub mod sdp_ext;
pub mod whep;
pub mod whip;

use crate::http::Handler;
use crate::mount::MountRegistry;
use std::sync::Arc;
use whep::WhepConfig;
use whip::WhipConfig;

/// WebRTC configuration.
#[derive(Clone, Debug)]
pub struct WebRtcConfig {
    /// WHIP (ingest) configuration.
    pub whip: WhipConfig,
    /// WHEP (playback) configuration.
    pub whep: WhepConfig,
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            whip: WhipConfig::default(),
            whep: WhepConfig::default(),
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
    // WHIP handler
    let whip_config = config.whip.clone();
    let whip_mounts = mounts.clone();
    let whip_handler: Handler = Arc::new(move |req, writer| {
        let _mounts = whip_mounts.clone();
        let _config = whip_config.clone();
        Box::pin(async move {
            match req.method {
                crate::http::Method::Post => {
                    // Note: In a real implementation, we'd need to read the body
                    // For now, return a placeholder response
                    whip::handle_whip_options(req, writer).await
                }
                crate::http::Method::Delete => {
                    whip::handle_whip_delete(req, writer).await
                }
                _ => {
                    whip::handle_whip_options(req, writer).await
                }
            }
        })
    });

    // WHEP handler
    let whep_config = config.whep.clone();
    let whep_mounts = mounts.clone();
    let whep_handler: Handler = Arc::new(move |req, writer| {
        let _mounts = whep_mounts.clone();
        let _config = whep_config.clone();
        Box::pin(async move {
            match req.method {
                crate::http::Method::Post => {
                    // Note: In a real implementation, we'd need to read the body
                    whep::handle_whep_options(req, writer).await
                }
                crate::http::Method::Delete => {
                    whep::handle_whep_delete(req, writer).await
                }
                _ => {
                    whep::handle_whep_options(req, writer).await
                }
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
    }
}
