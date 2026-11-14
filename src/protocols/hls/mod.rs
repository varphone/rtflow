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

impl HlsConfig {
    /// Reject degenerate segment duration; `segment_count` is clamped by
    /// [`segment_ring::SegmentRing`].
    pub fn validate(&self) -> crate::Result<()> {
        if self.segment_duration.is_zero() {
            return Err(crate::Error::Limit("HLS segment duration"));
        }
        Ok(())
    }
}

impl Default for HlsConfig {
    fn default() -> Self {
        Self {
            // 1 s segments: with a 1 s GOP this is the theoretical minimum
            // server-side cut latency (average GOP/2 = 0.5 s to the next
            // segment). The 3-segment window still satisfies the HLS
            // spec's 3×TARGETDURATION lookback requirement.
            segment_duration: Duration::from_secs(1),
            // Embedded budget: lookback segments are retained in RAM
            // after finalization (1s×bitrate×count peak).
            segment_count: 3,
            live: true,
        }
    }
}

/// Named HLS handler set. Prefer this over positional tuples.
pub struct HlsHandlers {
    pub playlist: Handler,
    pub segment: Handler,
    pub init: Handler,
}

/// Named-constructor variant of [`create_hls_handlers`].
pub fn create_hls_router(config: HlsConfig, mount: &crate::mount::MountHandle) -> HlsHandlers {
    let (playlist, segment, init) = create_hls_handlers(config, mount);
    HlsHandlers {
        playlist,
        segment,
        init,
    }
}

/// Create HLS HTTP handlers backed by a mount startup snapshot and tied to its lifetime.
pub fn create_hls_handlers(
    config: HlsConfig,
    mount: &crate::mount::MountHandle,
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

    // Mirror served-segment counts into the global metrics registry when
    // the mount's event sink is a MetricsEventSink.
    #[cfg(feature = "metrics")]
    let metrics = crate::metrics::registry_from_sink(&mount.inner.event_sink);

    let ring_clone = ring.clone();
    let seg_config = SegmenterConfig {
        segment_duration: config.segment_duration,
        lookback_count: config.segment_count,
    };
    tokio::spawn(async move {
        let mut segmenter = Segmenter::new(seg_config, &description);
        let mut segments = segmenter.subscribe();
        // Skip broadcast replays of frames the startup snapshot already
        // delivered (duplicate keyframes would corrupt segment timing).
        let startup_keys = crate::mount::startup_dedup_keys(&startup);
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
                        Err(crate::mount::FrameRecvError::Lagged(count)) => {
                            tracing::debug!(count, "HLS input lagged; waiting for a keyframe");
                            segmenter.reset_after_discontinuity();
                            awaiting_keyframe = has_video;
                            continue;
                        }
                        Err(crate::mount::FrameRecvError::Closed) => break,
                    },
                }
            };
            if awaiting_keyframe
                && matches!(
                    unit.codec,
                    crate::media::Codec::H264 | crate::media::Codec::H265
                )
            {
                if !unit.is_keyframe {
                    continue;
                }
                awaiting_keyframe = false;
            }
            // Skip broadcast replays of frames the startup snapshot already
            // delivered (duplicate keyframes would corrupt segment timing).
            if startup_keys.contains(&(unit.track_id, unit.rtp_timestamp)) {
                continue;
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

    if !is_live {
        // HlsConfig.live=false promises a VOD playlist, but segments are
        // still served from the sliding window ring and get evicted (404).
        // Refuse honestly instead of advertising an unusable VOD mode.
        tracing::warn!(
            "HLS VOD mode (HlsConfig.live=false) is not implemented;              segments would be evicted from the sliding window.              Use live mode (default)."
        );
        let reject: Handler = Arc::new(|_req, mut writer| {
            Box::pin(async move {
                writer
                    .write_body(
                        501,
                        "text/plain",
                        b"HLS VOD mode is not implemented; use live mode",
                    )
                    .await?;
                Ok(writer)
            })
        });
        return (reject.clone(), reject.clone(), reject);
    }

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
        #[cfg(feature = "metrics")]
        let metrics = metrics.clone();
        let r = ring_s.clone();
        Box::pin(async move {
            let seq = _req
                .path
                .rsplit('/')
                .next()
                .and_then(|value| value.trim_end_matches(".m4s").parse::<u64>().ok())
                .unwrap_or(u64::MAX);
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &metrics {
                use std::sync::atomic::Ordering;
                metrics.hls_segments_served.fetch_add(1, Ordering::Relaxed);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_segment_duration_is_rejected() {
        let config = HlsConfig {
            segment_duration: Duration::ZERO,
            ..HlsConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
