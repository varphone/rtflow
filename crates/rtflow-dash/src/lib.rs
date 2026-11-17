//! MPEG-DASH (Dynamic Adaptive Streaming over HTTP) output support.
//!
//! Generates DASH MPD manifests and serves per-track fMP4 segments via HTTP,
//! reusing the HLS segmenter implementation for segment production.
//!
//! # Architecture
//!
//! Like SRS, DASH packages each track independently:
//! - one `hls::segmenter::Segmenter` per media kind, fed a filtered description
//! - one `hls::segment_ring::SegmentRing` per track
//! - `video-init.mp4`/`audio-init.mp4` and `video-N.m4s`/`audio-N.m4s`
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
use rtflow_hls::segmenter::{Segment, Segmenter, SegmenterConfig};
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
    let video_ring = Arc::new(SegmentRing::new(config.segment_count));
    let audio_ring = Arc::new(SegmentRing::new(config.segment_count));
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

    let video_description = only_kind(&description, rtflow_media::MediaKind::Video);
    let audio_description = only_kind(&description, rtflow_media::MediaKind::Audio);
    let has_video = !video_description.tracks.is_empty();
    let has_audio = !audio_description.tracks.is_empty();
    let track_kinds: std::collections::HashMap<u8, rtflow_media::MediaKind> = description
        .tracks
        .iter()
        .map(|track| (track.id, track.kind))
        .collect();

    let video_ring_task = video_ring.clone();
    let audio_ring_task = audio_ring.clone();
    let state_task = dash_state.clone();
    let seg_config = SegmenterConfig {
        segment_duration: config.segment_duration,
        lookback_count: config.segment_count,
    };
    tokio::spawn(async move {
        // DASH packages per-track fMP4 segments. The HLS segmenter's
        // filtered description makes each segmenter write one init and one
        // media track, matching SRS's video-N.m4s / audio-N.m4s layout.
        let mut video_segmenter = Segmenter::new(seg_config.clone(), &video_description);
        // An audio-only segmenter cuts at 2× its configured duration. Halve
        // the target so audio segments are roughly the same length as video
        // segments instead of roughly twice as long.
        let mut audio_segmenter = Segmenter::new(
            SegmenterConfig {
                segment_duration: seg_config.segment_duration / 2,
                lookback_count: seg_config.lookback_count,
            },
            &audio_description,
        );
        let mut video_segments = video_segmenter.subscribe();
        let mut audio_segments = audio_segmenter.subscribe();
        let startup_keys = rtflow_mount::startup_dedup_keys(&startup);
        let mut startup = VecDeque::from(startup);
        let mut awaiting_keyframe = has_video;
        let mut fmtp_applied = false;
        let mut frame_count: u64 = 0;
        let mut keyframe_count: u64 = 0;
        let mut segment_count: u64 = 0;
        let mut base_pts: Option<Duration> = None;
        let mut last_pts: HashMap<u8, Duration> = HashMap::new();
        let mut initialized = false;
        tracing::info!(
            video_tracks = video_description.tracks.len(),
            audio_tracks = audio_description.tracks.len(),
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
                            video_segmenter.reset_after_discontinuity();
                            audio_segmenter.reset_after_discontinuity();
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

            if startup_keys.contains(&(unit.track_id, unit.rtp_timestamp)) {
                continue;
            }
            if unit.is_keyframe {
                keyframe_count += 1;
            }

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
                let offset = video_ring_task
                    .snapshot()
                    .last()
                    .map(|segment| segment.timestamp.saturating_add(segment.duration))
                    .unwrap_or_default()
                    .max(
                        audio_ring_task
                            .snapshot()
                            .last()
                            .map(|segment| segment.timestamp.saturating_add(segment.duration))
                            .unwrap_or_default(),
                    );
                state_task.set_timeline_offset(offset);
                video_segmenter.reset_after_discontinuity();
                audio_segmenter.reset_after_discontinuity();
                awaiting_keyframe = has_video;
                let is_video_unit = track_kinds
                    .get(&unit.track_id)
                    .is_some_and(|kind| *kind == rtflow_media::MediaKind::Video);
                if has_video && is_video_unit {
                    if !unit.is_keyframe {
                        continue;
                    }
                    awaiting_keyframe = false;
                }
            }

            if !fmtp_applied {
                let fmtp = mount_source
                    .announce_fmtp
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if !fmtp.is_empty() {
                    tracing::debug!(tracks = fmtp.len(), "DASH applying announce fmtp");
                    video_segmenter.update_fmtp(&fmtp);
                    audio_segmenter.update_fmtp(&fmtp);
                    fmtp_applied = true;
                }
            }

            let mut unit = (*unit).clone();
            let base = *base_pts.get_or_insert(unit.pts);
            unit.pts = unit.pts.saturating_sub(base);
            if let Some(dts) = unit.dts {
                unit.dts = Some(dts.saturating_sub(base));
            }
            match track_kinds.get(&unit.track_id).copied() {
                Some(rtflow_media::MediaKind::Video) if has_video => {
                    video_segmenter.push(unit);
                }
                Some(rtflow_media::MediaKind::Audio) if has_audio => {
                    audio_segmenter.push(unit);
                }
                _ => {}
            }

            while let Ok(mut segment) = video_segments.try_recv() {
                segment_count += 1;
                if !initialized {
                    state_task.set_availability_start(SystemTime::now());
                    state_task.set_base_timestamp(Duration::ZERO);
                    state_task.set_timeline_offset(Duration::ZERO);
                    initialized = true;
                }
                apply_offset(&state_task, &mut segment);
                update_dash_meta(&state_task, &segment.init);
                tracing::debug!(
                    segment_count,
                    sequence = segment.sequence,
                    keyframe_count,
                    ring_len = video_ring_task.len(),
                    "DASH video segment produced"
                );
                video_ring_task.push(segment);
            }
            while let Ok(mut segment) = audio_segments.try_recv() {
                segment_count += 1;
                if !initialized {
                    state_task.set_availability_start(SystemTime::now());
                    state_task.set_base_timestamp(Duration::ZERO);
                    state_task.set_timeline_offset(Duration::ZERO);
                    initialized = true;
                }
                apply_offset(&state_task, &mut segment);
                tracing::debug!(
                    segment_count,
                    sequence = segment.sequence,
                    ring_len = audio_ring_task.len(),
                    "DASH audio segment produced"
                );
                audio_ring_task.push(segment);
            }
        }

        tracing::info!(
            frame_count,
            keyframe_count,
            segment_count,
            "DASH segmenter task exiting"
        );
        video_segmenter.flush();
        audio_segmenter.flush();
        while let Ok(mut segment) = video_segments.try_recv() {
            apply_offset(&state_task, &mut segment);
            update_dash_meta(&state_task, &segment.init);
            video_ring_task.push(segment);
        }
        while let Ok(mut segment) = audio_segments.try_recv() {
            apply_offset(&state_task, &mut segment);
            audio_ring_task.push(segment);
        }
    });

    let ring_mpd = video_ring.clone();
    let audio_ring_mpd = audio_ring.clone();
    let desc_mpd = description.clone();
    let mpd_config = config.mpd.clone();
    let state_mpd = dash_state.clone();
    tracing::debug!("DASH handlers created");
    let mpd_handler: Handler = Arc::new(move |req, writer| {
        let video = ring_mpd.clone();
        let audio = audio_ring_mpd.clone();
        let description = desc_mpd.clone();
        let config = mpd_config.clone();
        let state = state_mpd.clone();
        Box::pin(async move {
            tracing::debug!(
                video_len = video.len(),
                audio_len = audio.len(),
                "DASH MPD requested"
            );
            handler::serve_mpd(req, writer, &video, &audio, &description, &config, &state).await
        })
    });

    let video_ring_seg = video_ring.clone();
    let audio_ring_seg = audio_ring.clone();
    let segment_handler: Handler = Arc::new(move |req, writer| {
        #[cfg(feature = "metrics")]
        let metrics = metrics.clone();
        let video = video_ring_seg.clone();
        let audio = audio_ring_seg.clone();
        Box::pin(async move {
            let file = req.path.rsplit('/').next().unwrap_or("");
            let is_audio = file.starts_with("audio-");
            let sequence = file
                .strip_prefix("video-")
                .or_else(|| file.strip_prefix("audio-"))
                .or_else(|| file.strip_prefix("segment_"))
                .and_then(|name| name.strip_suffix(".m4s"))
                .and_then(|name| name.parse::<u64>().ok())
                .unwrap_or(u64::MAX);
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &metrics {
                use std::sync::atomic::Ordering;
                metrics.dash_segments_served.fetch_add(1, Ordering::Relaxed);
            }
            handler::serve_segment(req, writer, &video, &audio, sequence, is_audio).await
        })
    });

    let video_ring_init = video_ring.clone();
    let audio_ring_init = audio_ring.clone();
    let init_handler: Handler = Arc::new(move |req, writer| {
        let video = video_ring_init.clone();
        let audio = audio_ring_init.clone();
        Box::pin(async move { handler::serve_init_segment(req, writer, &video, &audio).await })
    });

    (mpd_handler, segment_handler, init_handler)
}

fn only_kind(description: &MediaDescription, kind: rtflow_media::MediaKind) -> MediaDescription {
    let mut filtered = description.clone();
    filtered.tracks.retain(|track| track.kind == kind);
    filtered
}

fn apply_offset(state: &DashState, segment: &mut Segment) {
    let offset = state.timeline_offset();
    if !offset.is_zero() {
        segment.timestamp = segment.timestamp.saturating_add(offset);
    }
}

fn update_dash_meta(state: &DashState, init: &[u8]) {
    if let Some(codec) = extract_h264_codec_string(init) {
        tracing::debug!(codec = %codec, "DASH init segment codec");
        state.set_codec_string(codec);
    }
    if let Some((width, height)) = extract_video_dimensions(init) {
        tracing::debug!(width, height, "DASH init segment dimensions");
        state.set_video_dimensions(width, height);
    }
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
