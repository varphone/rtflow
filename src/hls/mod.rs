pub mod handler;
pub mod playlist;
pub mod segment_ring;
pub mod segmenter;

use crate::http::{Handler, Method};
use crate::media::{AccessUnit, MediaDescription};
use segment_ring::SegmentRing;
use segmenter::{Segmenter, SegmenterConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// HLS configuration.
#[derive(Clone, Debug)]
pub struct HlsConfig {
    /// Target segment duration.
    pub segment_duration: Duration,
    /// Number of segments to keep in the sliding window.
    pub segment_count: usize,
    /// Whether this is a live or VOD stream.
    pub live: bool,
}

impl Default for HlsConfig {
    fn default() -> Self {
        Self {
            segment_duration: Duration::from_secs(2),
            segment_count: 5,
            live: true,
        }
    }
}

/// Create HLS HTTP handlers for a mount.
/// Returns handlers for playlist, segment, and init segment endpoints.
pub fn create_hls_handlers(
    config: HlsConfig,
    description: MediaDescription,
    receiver: broadcast::Receiver<AccessUnit>,
) -> (Handler, Handler, Handler) {
    let ring = Arc::new(SegmentRing::new(config.segment_count));
    let target_duration = config.segment_duration;
    let is_live = config.live;

    // Spawn background segmenter task
    let ring_clone = ring.clone();
    let desc_clone = description.clone();
    let mut rx = receiver.resubscribe();
    let seg_config = SegmenterConfig {
        segment_duration: config.segment_duration,
        lookback_count: config.segment_count,
    };

    tokio::spawn(async move {
        let mut segmenter = Segmenter::new(seg_config, &desc_clone);
        let mut seg_rx = segmenter.subscribe();

        // Spawn a task to push frames into the segmenter
        let segmenter = Arc::new(tokio::sync::Mutex::new(segmenter));
        let seg_for_push = segmenter.clone();

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(unit) => {
                        seg_for_push.lock().await.push(unit);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Receive completed segments and push to ring
        loop {
            match seg_rx.recv().await {
                Ok(segment) => {
                    ring_clone.push(segment);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Playlist handler
    let ring_p = ring.clone();
    let playlist_handler: Handler = Arc::new(move |_req, writer| {
        let r = ring_p.clone();
        Box::pin(async move {
            handler::serve_playlist(_req, writer, &r, is_live, target_duration).await
        })
    });

    // Segment handler (/{seq}.m4s)
    let ring_s = ring.clone();
    let segment_handler: Handler = Arc::new(move |_req, writer| {
        let r = ring_s.clone();
        Box::pin(async move {
            let seq_str = _req.path.rsplit('/').next().unwrap_or("0");
            let seq = seq_str
                .trim_end_matches(".m4s")
                .parse::<u64>()
                .unwrap_or(0);
            handler::serve_segment(_req, writer, &r, seq).await
        })
    });

    // Init segment handler
    let ring_i = ring.clone();
    let init_handler: Handler = Arc::new(move |_req, writer| {
        let r = ring_i.clone();
        Box::pin(async move {
            handler::serve_init_segment(_req, writer, &r).await
        })
    });

    (playlist_handler, segment_handler, init_handler)
}
