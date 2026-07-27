//! MPEG-DASH (Dynamic Adaptive Streaming over HTTP) output support.
//!
//! Generates DASH MPD manifests and serves fMP4 segments via HTTP,
//! reusing the HLS segmenter infrastructure for segment production.
//!
//! # Architecture
//!
//! DASH output shares the same fMP4 segmenter as HLS:
//! - `hls::segmenter::Segmenter` produces init + media segments
//! - `hls::segment_ring::SegmentRing` provides the sliding window
//! - DASH adds MPD XML generation on top
//!
//! # Example
//!
//! ```no_run
//! use rtflow::dash::{create_dash_handlers, DashConfig};
//! use rtflow::server::RtspServer;
//!
//! # async fn example(server: &RtspServer) -> rtflow::Result<()> {
//! let mount = server.register_mount("/live/cam", /* config */ todo!()).await?;
//! let (mpd_handler, segment_handler, init_handler) =
//!     create_dash_handlers(DashConfig::default(), &mount);
//! // Register handlers with HTTP server router...
//! # Ok(())
//! # }
//! ```
pub mod handler;
pub mod mpd;

use crate::hls::segment_ring::SegmentRing;
use crate::hls::segmenter::{Segmenter, SegmenterConfig};
use crate::http::Handler;
use crate::media::MediaDescription;
use mpd::MpdConfig;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// DASH output configuration.
#[derive(Clone, Debug)]
pub struct DashConfig {
    /// Target segment duration.
    pub segment_duration: Duration,
    /// Number of segments to keep in the sliding window.
    pub segment_count: usize,
    /// MPD-specific configuration.
    pub mpd: MpdConfig,
}

impl Default for DashConfig {
    fn default() -> Self {
        Self {
            segment_duration: Duration::from_secs(2),
            segment_count: 5,
            mpd: MpdConfig::default(),
        }
    }
}

/// Create DASH HTTP handlers backed by a mount's live stream.
///
/// Returns three handlers:
/// 1. MPD manifest handler (`/manifest.mpd`)
/// 2. Media segment handler (`/segment_$Number$.m4s`)
/// 3. Init segment handler (`/init.mp4`)
pub fn create_dash_handlers(
    config: DashConfig,
    mount: &crate::mount::MountHandle,
) -> (Handler, Handler, Handler) {
    let ring = Arc::new(SegmentRing::new(config.segment_count));
    let mut description: MediaDescription = mount.description.clone();
    let announced_fmtp = mount.announce_fmtp();
    for track in &mut description.tracks {
        if let Some(fmtp) = announced_fmtp.get(&track.id) {
            track.fmtp = Some(fmtp.clone());
        }
    }
    let (mut receiver, startup) = mount.subscribe_with_startup();
    let cancel = mount.cancellation_token();
    let asset_prefix: Arc<str> = Arc::from(mount.path());

    // Spawn segmenter task
    let ring_clone = ring.clone();
    let seg_config = SegmenterConfig {
        segment_duration: config.segment_duration,
        lookback_count: config.segment_count,
    };
    let desc_segmenter = description.clone();
    tokio::spawn(async move {
        let mut segmenter = Segmenter::new(seg_config, &desc_segmenter);
        let mut segments = segmenter.subscribe();
        let mut startup = VecDeque::from(startup);
        let has_video = desc_segmenter
            .tracks
            .iter()
            .any(|track| track.kind == crate::media::MediaKind::Video);
        let mut awaiting_keyframe = has_video;

        loop {
            let unit = if let Some(unit) = startup.pop_front() {
                unit
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    result = receiver.recv() => match result {
                        Ok(unit) => unit,
                        Err(broadcast::error::RecvError::Lagged(count)) => {
                            tracing::debug!(count, "DASH input lagged; waiting for keyframe");
                            segmenter.reset_after_discontinuity();
                            awaiting_keyframe = has_video;
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                }
            };

            if awaiting_keyframe {
                if !matches!(
                    unit.codec,
                    crate::media::Codec::H264 | crate::media::Codec::H265
                ) || !unit.is_keyframe
                {
                    continue;
                }
                awaiting_keyframe = false;
            }

            segmenter.push(unit);
            while let Ok(segment) = segments.try_recv() {
                ring_clone.push(segment);
            }
        }

        segmenter.flush();
        while let Ok(segment) = segments.try_recv() {
            ring_clone.push(segment);
        }
    });

    // MPD handler
    let ring_mpd = ring.clone();
    let desc_mpd = description.clone();
    let mpd_config = config.mpd.clone();
    let prefix_mpd = asset_prefix.clone();
    let mpd_handler: Handler = Arc::new(move |req, writer| {
        let r = ring_mpd.clone();
        let d = desc_mpd.clone();
        let c = mpd_config.clone();
        let p = prefix_mpd.clone();
        Box::pin(async move {
            handler::serve_mpd(req, writer, &r, &d, &c, &p).await
        })
    });

    // Segment handler
    let ring_seg = ring.clone();
    let segment_handler: Handler = Arc::new(move |req, writer| {
        let r = ring_seg.clone();
        Box::pin(async move {
            let seq = req
                .path
                .rsplit('/')
                .next()
                .and_then(|name| {
                    name.strip_prefix("segment_")
                        .and_then(|n| n.strip_suffix(".m4s"))
                        .and_then(|n| n.parse::<u64>().ok())
                })
                .unwrap_or(u64::MAX);
            handler::serve_segment(req, writer, &r, seq).await
        })
    });

    // Init segment handler
    let ring_init = ring.clone();
    let init_handler: Handler = Arc::new(move |req, writer| {
        let r = ring_init.clone();
        Box::pin(async move { handler::serve_init_segment(req, writer, &r).await })
    });

    (mpd_handler, segment_handler, init_handler)
}
