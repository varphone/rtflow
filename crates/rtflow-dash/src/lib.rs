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
//! use rtflow_dash::{create_dash_handlers, DashConfig};
//! use rtflow_mount::{MountConfig, MountHandle};
//!
//! fn example(mount: &MountHandle) {
//!     let (mpd_handler, segment_handler, init_handler) =
//!         create_dash_handlers(DashConfig::default(), mount);
//!     let _ = (mpd_handler, segment_handler, init_handler);
//!     let _ = MountConfig::Local {
//!         description: mount.description(),
//!     };
//! }
//! ```
pub mod handler;
pub mod mpd;

use handler::DashState;
use mpd::MpdConfig;
use rtflow_hls::segment_ring::SegmentRing;
use rtflow_hls::segmenter::{Segmenter, SegmenterConfig};
use rtflow_http::Handler;
use rtflow_media::MediaDescription;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

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

impl DashConfig {
    /// Reject degenerate segment duration; `segment_count` is clamped by
    /// the HLS `SegmentRing`.
    pub fn validate(&self) -> rtflow_core::error::Result<()> {
        if self.segment_duration.is_zero() {
            return Err(rtflow_core::error::Error::Limit("DASH segment duration"));
        }
        if self.segment_count == 0 {
            return Err(rtflow_core::error::Error::Limit("DASH segment count"));
        }
        self.mpd.validate()
    }
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
///
/// Named DASH handler set. Prefer this over positional tuples.
pub struct DashHandlers {
    pub manifest: Handler,
    pub segment: Handler,
    pub init: Handler,
}

/// Named-constructor variant of [`create_dash_handlers`].
pub fn create_dash_router(config: DashConfig, mount: &rtflow_mount::MountHandle) -> DashHandlers {
    let (manifest, segment, init) = create_dash_handlers(config, mount);
    DashHandlers {
        manifest,
        segment,
        init,
    }
}

pub fn create_dash_handlers(
    config: DashConfig,
    mount: &rtflow_mount::MountHandle,
) -> (Handler, Handler, Handler) {
    let ring = Arc::new(SegmentRing::new(config.segment_count));
    // Mirror served-segment counts into the global metrics registry when
    // the mount's event sink is a MetricsEventSink.
    #[cfg(feature = "metrics")]
    let metrics = rtflow_core::metrics::registry_from_sink(&mount.mount().event_sink);
    let dash_state = Arc::new(DashState::new());
    let mut description: MediaDescription = mount.description();
    let announced_fmtp = mount.announce_fmtp();
    for track in &mut description.tracks {
        if let Some(fmtp) = announced_fmtp.get(&track.id) {
            track.fmtp = Some(fmtp.clone());
        }
    }
    let (mut receiver, startup) = mount.subscribe_with_startup();
    let cancel = mount.cancellation_token();
    let mount_source = mount.announce_fmtp_source();

    // Spawn segmenter task
    let ring_clone = ring.clone();
    let state_clone = dash_state.clone();
    let seg_config = SegmenterConfig {
        segment_duration: config.segment_duration,
        lookback_count: config.segment_count,
    };
    let desc_segmenter = description.clone();
    tokio::spawn(async move {
        let mut segmenter = Segmenter::new(seg_config, &desc_segmenter);
        let mut segments = segmenter.subscribe();
        // Skip broadcast replays of frames the startup snapshot already
        // delivered (duplicate keyframes would corrupt segment timing).
        let startup_keys = rtflow_mount::startup_dedup_keys(&startup);
        let mut startup = VecDeque::from(startup);
        let has_video = desc_segmenter
            .tracks
            .iter()
            .any(|track| track.kind == rtflow_media::MediaKind::Video);
        let mut awaiting_keyframe = has_video;
        let mut fmtp_applied = false;
        let mut frame_count: u64 = 0;
        let mut keyframe_count: u64 = 0;
        let mut segment_count: u64 = 0;
        // Timestamp normalization: subtract first frame's PTS so media segment
        // tfdt values start near 0, matching the MPD SegmentTimeline.
        let mut base_pts: Option<Duration> = None;
        let mut last_pts: HashMap<u8, Duration> = HashMap::new();
        let mut init_hash: Option<u64> = None;
        tracing::info!(
            tracks = desc_segmenter.tracks.len(),
            has_video,
            startup_frames = startup.len(),
            "DASH segmenter task started"
        );

        loop {
            let unit = if let Some(unit) = startup.pop_front() {
                unit
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    result = receiver.recv() => match result {
                        Ok(unit) => unit,
                        Err(rtflow_mount::FrameRecvError::Lagged(count)) => {
                            tracing::warn!(count, "DASH input lagged; waiting for keyframe");
                            segmenter.reset_after_discontinuity();
                            awaiting_keyframe = has_video;
                            continue;
                        }
                        Err(rtflow_mount::FrameRecvError::Closed) => break,
                    },
                }
            };

            frame_count += 1;
            if frame_count == 1 {
                tracing::debug!(
                    codec = ?unit.codec,
                    is_keyframe = unit.is_keyframe,
                    track_id = unit.track_id,
                    "DASH first frame received"
                );
            }

            if awaiting_keyframe
                && matches!(
                    unit.codec,
                    rtflow_media::Codec::H264 | rtflow_media::Codec::H265
                )
            {
                if !unit.is_keyframe {
                    continue;
                }
                tracing::info!(frame_count, "DASH first keyframe detected");
                awaiting_keyframe = false;
            }

            // Skip broadcast replays of frames the startup snapshot already
            // delivered (duplicate keyframes would corrupt segment timing).
            if startup_keys.contains(&(unit.track_id, unit.rtp_timestamp)) {
                continue;
            }

            if unit.is_keyframe {
                keyframe_count += 1;
            }

            // Apply announce fmtp once available (publisher may ANNOUNCE after handlers created)
            if !fmtp_applied {
                let fmtp = mount_source
                    .announce_fmtp
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if !fmtp.is_empty() {
                    tracing::debug!(tracks = fmtp.len(), "DASH applying announce fmtp");
                    segmenter.update_fmtp(&fmtp);
                    fmtp_applied = true;
                }
            }

            // Normalize timestamps relative to first frame (so tfdt starts near 0)
            // Detect publisher reconnection: a large backward PTS jump means a
            // new timestamp epoch. Compare against the last PTS of the same
            // track, not the normalization base (which is the first PTS and
            // therefore never moves).
            let previous_pts = last_pts.insert(unit.track_id, unit.pts);
            if let Some(previous) = previous_pts
                && unit.pts.saturating_add(Duration::from_secs(5)) < previous
            {
                tracing::info!(
                    old_pts_ms = previous.as_millis() as u64,
                    new_pts_ms = unit.pts.as_millis() as u64,
                    "DASH timestamp discontinuity detected (publisher reconnect?), resetting"
                );
                base_pts = None;
                last_pts.clear();
                // Keep the original availability start time: a reconnect is
                // the same presentation. New segments are placed after the
                // last segment on the timeline so `S@t` never goes backwards.
                let offset = ring_clone
                    .snapshot()
                    .last()
                    .map(|segment| segment.timestamp.saturating_add(segment.duration))
                    .unwrap_or_default();
                state_clone.set_timeline_offset(offset);
                segmenter.reset_after_discontinuity();
                awaiting_keyframe = has_video;
                // Skip non-keyframe data until we get a clean starting point
                if has_video && !unit.is_keyframe {
                    continue;
                }
                awaiting_keyframe = false;
            }
            let mut unit = (*unit).clone();
            let base = *base_pts.get_or_insert(unit.pts);
            unit.pts = unit.pts.saturating_sub(base);
            if let Some(dts) = unit.dts {
                unit.dts = Some(dts.saturating_sub(base));
            }

            segmenter.push(unit);
            while let Ok(mut segment) = segments.try_recv() {
                segment_count += 1;
                tracing::debug!(
                    segment_count,
                    sequence = segment.sequence,
                    keyframe_count,
                    ring_len = ring_clone.len(),
                    "DASH segment produced"
                );
                let hash = hash_bytes(&segment.init);
                match init_hash {
                    None => {
                        state_clone.set_availability_start(SystemTime::now());
                        state_clone.set_base_timestamp(Duration::ZERO);
                        state_clone.set_timeline_offset(Duration::ZERO);
                        update_dash_meta(&state_clone, &segment.init);
                        init_hash = Some(hash);
                    }
                    Some(previous) if previous != hash && !ring_clone.is_empty() => {
                        // Codec configuration changed: start a new
                        // presentation. Players refetch the MPD and init.
                        tracing::info!("DASH init segment changed; starting new presentation");
                        ring_clone.clear();
                        state_clone.reset_timeline();
                        state_clone.set_availability_start(SystemTime::now());
                        state_clone.set_base_timestamp(segment.timestamp);
                        state_clone.set_timeline_offset(Duration::ZERO);
                        update_dash_meta(&state_clone, &segment.init);
                        init_hash = Some(hash);
                    }
                    _ => {}
                }
                let offset = state_clone.timeline_offset();
                if !offset.is_zero() {
                    segment.timestamp = segment.timestamp.saturating_add(offset);
                }
                ring_clone.push(segment);
            }
        }

        tracing::info!(
            frame_count,
            keyframe_count,
            segment_count,
            "DASH segmenter task exiting"
        );
        segmenter.flush();
        while let Ok(mut segment) = segments.try_recv() {
            let offset = state_clone.timeline_offset();
            if !offset.is_zero() {
                segment.timestamp = segment.timestamp.saturating_add(offset);
            }
            ring_clone.push(segment);
        }
    });

    // MPD handler
    let ring_mpd = ring.clone();
    let desc_mpd = description.clone();
    let mpd_config = config.mpd.clone();
    let state_mpd = dash_state.clone();
    tracing::debug!("DASH handlers created");
    let mpd_handler: Handler = Arc::new(move |req, writer| {
        let r = ring_mpd.clone();
        let d = desc_mpd.clone();
        let c = mpd_config.clone();
        let s = state_mpd.clone();
        Box::pin(async move {
            tracing::debug!(ring_len = r.len(), "DASH MPD requested");
            handler::serve_mpd(req, writer, &r, &d, &c, &s).await
        })
    });

    // Segment handler
    let ring_seg = ring.clone();
    let segment_handler: Handler = Arc::new(move |req, writer| {
        #[cfg(feature = "metrics")]
        let metrics = metrics.clone();
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
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &metrics {
                use std::sync::atomic::Ordering;
                metrics.dash_segments_served.fetch_add(1, Ordering::Relaxed);
            }
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

fn update_dash_meta(state: &DashState, init: &[u8]) {
    if let Some(codec) = extract_h264_codec_string(init) {
        tracing::debug!(codec = %codec, "DASH init segment codec");
        state.set_codec_string(codec);
    } else if !init.is_empty() {
        tracing::warn!(init_len = init.len(), "DASH init segment has no valid avcC");
    }
    if let Some((width, height)) = extract_video_dimensions(init) {
        tracing::debug!(width, height, "DASH init segment dimensions");
        state.set_video_dimensions(width, height);
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Extract the H.264 codec string (e.g., "avc1.640028") from an init segment's avcC box.
///
/// Searches for the `avcC` box marker and reads profile/compat/level bytes.
fn extract_h264_codec_string(init: &[u8]) -> Option<String> {
    // Find "avcC" marker in the init segment
    let marker = b"avcC";
    let pos = init.windows(4).position(|window| window == marker)?;
    // avcC payload starts right after the 4-byte marker
    let payload = &init[pos + 4..];
    if payload.len() < 4 {
        return None;
    }
    // payload[0] = configurationVersion (should be 1)
    // payload[1] = AVCProfileIndication
    // payload[2] = profile_compatibility
    // payload[3] = AVCLevelIndication
    if payload[0] != 1 {
        return None;
    }
    Some(format!(
        "avc1.{:02X}{:02X}{:02X}",
        payload[1], payload[2], payload[3]
    ))
}

/// Extract video dimensions (width, height) from an init segment's avc1/hvc1 box.
///
/// The width/height fields are at fixed offsets within the visual sample entry:
/// offset 32 = width (u16), offset 34 = height (u16) from the box start.
fn extract_video_dimensions(init: &[u8]) -> Option<(u16, u16)> {
    // Search for "avc1" or "hvc1" marker
    let pos = init.windows(4).position(|w| w == b"avc1" || w == b"hvc1")?;
    // The box starts 4 bytes before the marker (size field)
    // width is at offset 32 from box start = pos - 4 + 32 = pos + 28
    // height is at offset 34 from box start = pos + 30
    let width_offset = pos + 28;
    let height_offset = pos + 30;
    if init.len() < height_offset + 2 {
        return None;
    }
    let width = u16::from_be_bytes([init[width_offset], init[width_offset + 1]]);
    let height = u16::from_be_bytes([init[height_offset], init[height_offset + 1]]);
    if width > 0 && height > 0 {
        Some((width, height))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_durations_are_rejected() {
        let config = DashConfig {
            segment_duration: Duration::ZERO,
            ..DashConfig::default()
        };
        assert!(config.validate().is_err());
        let mut config = DashConfig::default();
        config.mpd.timescale = 0;
        assert!(config.validate().is_err());
        let config = DashConfig {
            segment_count: 0,
            ..DashConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
