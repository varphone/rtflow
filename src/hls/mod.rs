pub mod handler;
pub mod playlist;
pub mod segment_ring;
pub mod segmenter;

use crate::http::Handler;
use crate::media::MediaDescription;
use segment_ring::SegmentRing;
use segmenter::{Segmenter, SegmenterConfig};
use std::time::Duration;
use std::{collections::VecDeque, sync::Arc};
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

/// Create HLS HTTP handlers backed by a mount startup snapshot and tied to its lifetime.
pub fn create_hls_handlers(
    config: HlsConfig,
    mount: &crate::server::MountHandle,
) -> (Handler, Handler, Handler) {
    let ring = Arc::new(SegmentRing::new(config.segment_count));
    let target_duration = config.segment_duration;
    let is_live = config.live;
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

    let ring_clone = ring.clone();
    let seg_config = SegmenterConfig {
        segment_duration: config.segment_duration,
        lookback_count: config.segment_count,
    };
    tokio::spawn(async move {
        let mut segmenter = Segmenter::new(seg_config, &description);
        let mut segments = segmenter.subscribe();
        let mut startup = VecDeque::from(startup);
        let has_video = description
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
                            tracing::debug!(count, "HLS input lagged; waiting for a keyframe");
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

    let ring_p = ring.clone();
    let playlist_asset_prefix = asset_prefix.clone();
    let playlist_handler: Handler = Arc::new(move |_req, writer| {
        let r = ring_p.clone();
        let asset_prefix = playlist_asset_prefix.clone();
        Box::pin(async move {
            handler::serve_playlist(_req, writer, &r, is_live, target_duration, &asset_prefix).await
        })
    });

    let ring_s = ring.clone();
    let segment_handler: Handler = Arc::new(move |_req, writer| {
        let r = ring_s.clone();
        Box::pin(async move {
            let seq = _req
                .path
                .rsplit('/')
                .next()
                .and_then(|value| value.trim_end_matches(".m4s").parse::<u64>().ok())
                .unwrap_or(u64::MAX);
            handler::serve_segment(_req, writer, &r, seq).await
        })
    });

    let ring_i = ring.clone();
    let init_handler: Handler = Arc::new(move |_req, writer| {
        let r = ring_i.clone();
        Box::pin(async move { handler::serve_init_segment(_req, writer, &r).await })
    });

    (playlist_handler, segment_handler, init_handler)
}
