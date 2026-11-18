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
pub mod vod;

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

    let video_tracks: Vec<rtflow_media::TrackDescription> = description
        .tracks
        .iter()
        .filter(|track| track.kind == rtflow_media::MediaKind::Video)
        .cloned()
        .collect();
    let audio_track = description
        .tracks
        .iter()
        .find(|track| track.kind == rtflow_media::MediaKind::Audio)
        .cloned();
    let video_descriptions: Vec<MediaDescription> = video_tracks
        .iter()
        .map(|track| {
            let mut filtered = description.clone();
            filtered.tracks = vec![track.clone()];
            filtered
        })
        .collect();
    let audio_description = audio_track.as_ref().map(|track| {
        let mut filtered = description.clone();
        filtered.tracks = vec![track.clone()];
        filtered
    });
    let video_rings: Vec<Arc<SegmentRing>> = video_tracks
        .iter()
        .map(|_| Arc::new(SegmentRing::new(config.segment_count)))
        .collect();
    let audio_ring = audio_track
        .as_ref()
        .map(|_| Arc::new(SegmentRing::new(config.segment_count)));
    let rings = Arc::new(handler::DashRings {
        video: video_tracks
            .iter()
            .zip(&video_rings)
            .map(|(track, ring)| handler::DashTrackRing {
                id: track.id,
                track: track.clone(),
                ring: ring.clone(),
            })
            .collect(),
        audio: audio_track
            .as_ref()
            .zip(audio_ring.as_ref())
            .map(|(track, ring)| handler::DashTrackRing {
                id: track.id,
                track: track.clone(),
                ring: ring.clone(),
            }),
    });

    let video_rings_task = video_rings.clone();
    let audio_ring_task = audio_ring.clone();
    let state_task = dash_state.clone();
    let seg_config = SegmenterConfig {
        segment_duration: config.segment_duration,
        lookback_count: config.segment_count,
    };
    let has_video = !video_tracks.is_empty();
    tokio::spawn(async move {
        let mut video_segmenters: Vec<Segmenter> = video_descriptions
            .iter()
            .map(|description| Segmenter::new(seg_config.clone(), description))
            .collect();
        let mut video_receivers: Vec<_> = video_segmenters
            .iter()
            .map(|segmenter| segmenter.subscribe())
            .collect();
        let video_index: HashMap<u8, usize> = video_tracks
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id, index))
            .collect();
        // Audio is cut at video boundaries; without video the segmenter's
        // 2× hard cut is used, so halve the target to get ~segment_duration.
        let audio_segment_duration = if has_video {
            seg_config.segment_duration.saturating_mul(2)
        } else {
            seg_config.segment_duration / 2
        };
        let mut audio_segmenter = audio_description.map(|description| {
            Segmenter::new(
                SegmenterConfig {
                    segment_duration: audio_segment_duration,
                    lookback_count: seg_config.lookback_count,
                },
                &description,
            )
        });
        let mut audio_receiver = audio_segmenter
            .as_ref()
            .map(|segmenter| segmenter.subscribe());
        let audio_track_id = audio_track.as_ref().map(|track| track.id);

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
            video_tracks = video_tracks.len(),
            has_audio = audio_track.is_some(),
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
                            for segmenter in &mut video_segmenters {
                                segmenter.reset_after_discontinuity();
                            }
                            if let Some(segmenter) = &mut audio_segmenter {
                                segmenter.reset_after_discontinuity();
                            }
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
                let mut offset = Duration::ZERO;
                for ring in &video_rings_task {
                    if let Some(segment) = ring.snapshot().last() {
                        offset = offset.max(segment.timestamp.saturating_add(segment.duration));
                    }
                }
                if let Some(ring) = &audio_ring_task
                    && let Some(segment) = ring.snapshot().last()
                {
                    offset = offset.max(segment.timestamp.saturating_add(segment.duration));
                }
                state_task.set_timeline_offset(offset);
                for segmenter in &mut video_segmenters {
                    segmenter.reset_after_discontinuity();
                }
                if let Some(segmenter) = &mut audio_segmenter {
                    segmenter.reset_after_discontinuity();
                }
                awaiting_keyframe = has_video;
                if has_video && video_index.get(&unit.track_id).is_some() && !unit.is_keyframe {
                    continue;
                }
                awaiting_keyframe = false;
            }

            if !fmtp_applied {
                let fmtp = mount_source
                    .announce_fmtp
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if !fmtp.is_empty() {
                    tracing::debug!(tracks = fmtp.len(), "DASH applying announce fmtp");
                    for segmenter in &mut video_segmenters {
                        segmenter.update_fmtp(&fmtp);
                    }
                    if let Some(segmenter) = &mut audio_segmenter {
                        segmenter.update_fmtp(&fmtp);
                    }
                    fmtp_applied = true;
                }
            }

            let mut unit = (*unit).clone();
            let base = *base_pts.get_or_insert(unit.pts);
            unit.pts = unit.pts.saturating_sub(base);
            if let Some(dts) = unit.dts {
                unit.dts = Some(dts.saturating_sub(base));
            }
            if let Some(index) = video_index.get(&unit.track_id).copied() {
                video_segmenters[index].push(unit);
            } else if audio_track_id == Some(unit.track_id)
                && let Some(segmenter) = &mut audio_segmenter
            {
                segmenter.push(unit);
            }

            for (index, receiver) in video_receivers.iter_mut().enumerate() {
                while let Ok(mut segment) = receiver.try_recv() {
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
                        keyframe_count,
                        track_id = video_tracks[index].id,
                        ring_len = video_rings_task[index].len(),
                        "DASH video segment produced"
                    );
                    video_rings_task[index].push(segment);
                    if let Some(segmenter) = &mut audio_segmenter {
                        segmenter.cut_current();
                    }
                }
            }
            if let Some(receiver) = audio_receiver.as_mut() {
                while let Ok(mut segment) = receiver.try_recv() {
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
                        "DASH audio segment produced"
                    );
                    if let Some(ring) = &audio_ring_task {
                        ring.push(segment);
                    }
                }
            }
        }

        tracing::info!(
            frame_count,
            keyframe_count,
            segment_count,
            "DASH segmenter task exiting"
        );
        for segmenter in &mut video_segmenters {
            segmenter.flush();
        }
        if let Some(segmenter) = &mut audio_segmenter {
            segmenter.flush();
        }
        for (index, receiver) in video_receivers.iter_mut().enumerate() {
            while let Ok(mut segment) = receiver.try_recv() {
                apply_offset(&state_task, &mut segment);
                video_rings_task[index].push(segment);
            }
        }
        if let Some(receiver) = audio_receiver.as_mut() {
            while let Ok(mut segment) = receiver.try_recv() {
                apply_offset(&state_task, &mut segment);
                if let Some(ring) = &audio_ring_task {
                    ring.push(segment);
                }
            }
        }
    });

    let rings_mpd = rings.clone();
    let desc_mpd = description.clone();
    let mpd_config = config.mpd.clone();
    let state_mpd = dash_state.clone();
    tracing::debug!("DASH handlers created");
    let mpd_handler: Handler = Arc::new(move |req, writer| {
        let rings = rings_mpd.clone();
        let description = desc_mpd.clone();
        let config = mpd_config.clone();
        let state = state_mpd.clone();
        Box::pin(async move {
            handler::serve_mpd(req, writer, &rings, &description, &config, &state).await
        })
    });

    let rings_seg = rings.clone();
    let segment_handler: Handler = Arc::new(move |req, writer| {
        #[cfg(feature = "metrics")]
        let metrics = metrics.clone();
        let rings = rings_seg.clone();
        Box::pin(async move {
            let file = req.path.rsplit('/').next().unwrap_or("");
            let (selector, sequence) = parse_segment_name(file);
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &metrics {
                use std::sync::atomic::Ordering;
                metrics.dash_segments_served.fetch_add(1, Ordering::Relaxed);
            }
            handler::serve_segment(req, writer, &rings, sequence, selector).await
        })
    });

    let rings_init = rings.clone();
    let init_handler: Handler = Arc::new(move |req, writer| {
        let rings = rings_init.clone();
        Box::pin(async move { handler::serve_init_segment(req, writer, &rings).await })
    });

    (mpd_handler, segment_handler, init_handler)
}

fn parse_segment_name(file: &str) -> (handler::DashTrackSelector, u64) {
    if let Some(rest) = file.strip_prefix("audio-")
        && let Some(sequence) = rest
            .strip_suffix(".m4s")
            .and_then(|value| value.parse::<u64>().ok())
    {
        return (handler::DashTrackSelector::Audio, sequence);
    }
    if let Some(rest) = file.strip_prefix("video-") {
        let body = rest.strip_suffix(".m4s").unwrap_or(rest);
        if let Some((id, sequence)) = body.split_once('-')
            && let Ok(id) = id.parse::<u8>()
            && let Ok(sequence) = sequence.parse::<u64>()
        {
            return (handler::DashTrackSelector::Video(Some(id)), sequence);
        }
        if let Ok(sequence) = body.parse::<u64>() {
            return (handler::DashTrackSelector::Video(None), sequence);
        }
    }
    if let Some(sequence) = file
        .strip_prefix("segment_")
        .and_then(|value| value.strip_suffix(".m4s"))
        .and_then(|value| value.parse::<u64>().ok())
    {
        return (handler::DashTrackSelector::Video(None), sequence);
    }
    (handler::DashTrackSelector::Video(None), u64::MAX)
}

fn apply_offset(state: &DashState, segment: &mut Segment) {
    let offset = state.timeline_offset();
    if !offset.is_zero() {
        segment.timestamp = segment.timestamp.saturating_add(offset);
    }
}

/// Extract the H.264 codec string (e.g., "avc1.640028") from an init segment's avcC box.
///
/// Searches for the `avcC` box marker and reads profile/compat/level bytes.
pub(crate) fn extract_h264_codec_string(init: &[u8]) -> Option<String> {
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

/// Extract an HEVC codec string from an init segment's hvcC box.
///
/// Format follows ISO/IEC 14496-15 Annex E: profile space/idc, reversed
/// compatibility flags, tier/level and the non-zero constraint bytes. The
/// sample entry written by the segmenter is hvc1, so the codec prefix is hvc1.
pub(crate) fn extract_h265_codec_string(init: &[u8]) -> Option<String> {
    let position = init.windows(4).position(|window| window == b"hvcC")?;
    let payload = &init[position + 4..];
    if payload.len() < 13 || payload[0] != 1 {
        return None;
    }
    let profile_space = (payload[1] >> 6) & 0x03;
    let tier = (payload[1] >> 5) & 0x01;
    let profile_idc = payload[1] & 0x1f;
    let compatibility = u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]);
    let compatibility = compatibility.reverse_bits();
    let level = payload[12];
    let space = if profile_space > 0 {
        ((b'A' + profile_space - 1) as char).to_string()
    } else {
        String::new()
    };
    let mut codec = format!(
        "hvc1.{space}{profile_idc}.{compatibility:X}.{}{level}",
        if tier == 1 { 'H' } else { 'L' }
    );
    let mut constraint_end = 6;
    for (index, byte) in payload[6..12].iter().enumerate() {
        if *byte != 0 {
            constraint_end = 6 + index + 1;
        }
    }
    for byte in &payload[6..constraint_end] {
        codec.push_str(&format!(".{byte:X}"));
    }
    Some(codec)
}

/// Extract video dimensions (width, height) from an init segment's avc1/hvc1 box.
///
/// The width/height fields are at fixed offsets within the visual sample entry:
/// offset 32 = width (u16), offset 34 = height (u16) from the box start.
pub(crate) fn extract_video_dimensions(init: &[u8]) -> Option<(u16, u16)> {
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
    fn extracts_h265_codec_string() {
        let mut init = b"\0\0\0\0hvcC".to_vec();
        let mut payload = [0u8; 13];
        payload[0] = 1;
        payload[1] = 0x01; // profile_space 0, tier 0, profile_idc 1
        payload[12] = 93;
        init.extend_from_slice(&payload);
        assert_eq!(
            extract_h265_codec_string(&init).as_deref(),
            Some("hvc1.1.0.L93")
        );
        assert!(extract_h265_codec_string(b"no hvcC here").is_none());
    }

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
