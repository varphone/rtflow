use bytes::Bytes;
use std::{collections::HashMap, time::Duration};
use tokio::sync::broadcast;

use rtflow_media::{AccessUnit, Codec, MediaDescription, MediaKind};

mod entries;
mod init;
mod media;
mod sps;
pub(crate) mod ts;
pub(crate) mod util;

use self::init::build_init_segment;
use self::media::build_media_segment;
use self::ts::TsPipeline;
use self::util::{annex_b_to_length_prefixed, codec_config_from_annex_b, codec_config_from_fmtp};

/// Hard safety bound for a segment whose timestamps never advance.
const MAX_SAMPLES_PER_SEGMENT: usize = 10_000;

/// Configuration for the fMP4 segmenter.
#[derive(Clone, Debug)]
pub struct SegmenterConfig {
    /// Target segment duration.
    pub segment_duration: Duration,
    /// Maximum number of segments to keep in the ring buffer.
    pub lookback_count: usize,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        Self {
            segment_duration: Duration::from_secs(2),
            // Embedded budget (docs/audits/embedded-resource.md P1-6):
            // lookback segments are retained in RAM after finalization,
            // so 2s×bitrate×lookback caps the HLS writer peak (5→3 saves
            // ~2×segment bytes, e.g. 2MB at 1080p 4Mbps).
            lookback_count: 3,
        }
    }
}

/// Container written for a completed segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentFormat {
    /// fMP4 (init + moof/mdat), used by VOD and static HLS.
    Fmp4,
    /// MPEG-TS (PAT/PMT/PES), used by live HLS.
    MpegTs,
}

/// A completed HLS segment.
#[derive(Clone, Debug)]
pub struct Segment {
    /// Sequence number (monotonically increasing).
    pub sequence: u64,
    /// Actual duration of this segment.
    pub duration: Duration,
    /// Init segment data (ftyp + moov).
    pub init: Bytes,
    /// Media segment data (moof + mdat).
    pub data: Bytes,
    /// Whether this segment starts with a keyframe.
    pub starts_with_keyframe: bool,
    /// Whether a decoder discontinuity precedes this segment.
    pub discontinuity: bool,
    /// Global discontinuity sequence number of this segment (Some when
    /// `discontinuity`), for EXT-X-DISCONTINUITY-SEQUENCE alignment.
    pub discontinuity_sequence: Option<u32>,
    /// Timestamp of the first sample in the segment.
    pub timestamp: Duration,
    /// Container format of `data`.
    pub format: SegmentFormat,
}

struct TrackState {
    kind: MediaKind,
    timescale: u32,
    samples: Vec<SampleEntry>,
    last_dts: Option<Duration>,
    /// Last PTS pushed for this track, for reset detection against the
    /// track's own timeline (audio and video clocks are independent; a
    /// global comparison would mistake their long-term baseline drift
    /// for a stream reset).
    last_pts: Option<Duration>,
    codec_config: Option<Bytes>,
}

struct SampleEntry {
    data: Bytes,
    pts: Duration,
    dts: Duration,
    is_keyframe: bool,
    duration: Duration,
}

pub struct Segmenter {
    config: SegmenterConfig,
    description: MediaDescription,
    tracks: Vec<TrackState>,
    track_indices: HashMap<u8, usize>,
    current_segment_pts: Option<Duration>,
    sequence: u64,
    tx: broadcast::Sender<Segment>,
    init_segment: Option<Bytes>,
    next_discontinuity: bool,
    /// Monotonic counter of discontinuities ever generated.
    discontinuity_counter: u32,
    ts_pipeline: Option<TsPipeline>,
    mpeg_ts: bool,
}

impl Segmenter {
    pub fn new(config: SegmenterConfig, description: &MediaDescription) -> Self {
        let (tx, _) = broadcast::channel(config.lookback_count.max(2));
        let track_indices = description
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id, index))
            .collect();
        let tracks = description
            .tracks
            .iter()
            .map(|t| TrackState {
                kind: t.kind,
                timescale: t.clock_rate,
                samples: Vec::new(),
                last_dts: None,
                last_pts: None,
                codec_config: None,
            })
            .collect();

        Self {
            config,
            description: description.clone(),
            tracks,
            track_indices,
            current_segment_pts: None,
            sequence: 0,
            tx,
            init_segment: None,
            next_discontinuity: false,
            discontinuity_counter: 0,
            ts_pipeline: None,
            mpeg_ts: false,
        }
    }

    /// Create a live segmenter that emits MPEG-TS segments via the
    /// `transmux` streaming TS-HLS segmenter. Falls back to fMP4 when the
    /// description lacks a usable AVC/AAC configuration.
    pub fn new_mpegts(config: SegmenterConfig, description: &MediaDescription) -> Self {
        let ts_pipeline = TsPipeline::new(description, &config);
        let mut segmenter = Self::new(config, description);
        segmenter.mpeg_ts = ts_pipeline.is_some();
        segmenter.ts_pipeline = ts_pipeline;
        segmenter
    }

    /// Subscribe to completed segments.
    pub fn subscribe(&self) -> broadcast::Receiver<Segment> {
        self.tx.subscribe()
    }

    /// Update track fmtp parameters (e.g., from a late ANNOUNCE SDP).
    ///
    /// If a track's `codec_config` has not yet been extracted from the stream,
    /// this also attempts to initialize it from `sprop-parameter-sets` in the fmtp.
    pub fn update_fmtp(&mut self, fmtp_map: &HashMap<u8, std::sync::Arc<str>>) {
        for track_desc in &mut self.description.tracks {
            if let Some(fmtp) = fmtp_map.get(&track_desc.id) {
                track_desc.fmtp = Some(fmtp.clone());
            }
        }
        // Try to initialize codec_config from fmtp for tracks that don't have it yet
        for (i, track_desc) in self.description.tracks.iter().enumerate() {
            if let Some(state) = self.tracks.get_mut(i)
                && state.codec_config.is_none()
            {
                state.codec_config = codec_config_from_fmtp(track_desc);
                tracing::debug!(
                    track_id = track_desc.id,
                    codec_config_len = state.codec_config.as_ref().map(|c| c.len()).unwrap_or(0),
                    "Segmenter update_fmtp codec_config"
                );
            }
        }
    }

    /// Push an AccessUnit into the segmenter. Segments are cut immediately before
    /// the first keyframe at or after the target duration, so every new video
    /// segment begins at a random access point.
    /// Set the next sequence number before the first push. Used when a
    /// segmenter is recreated for a long-lived live mount: the playlist's
    /// EXT-X-MEDIA-SEQUENCE must never go backwards.
    pub fn set_sequence(&mut self, sequence: u64) {
        self.sequence = sequence;
        if let Some(pipeline) = &mut self.ts_pipeline {
            pipeline.set_sequence(sequence);
        }
    }

    pub fn push(&mut self, unit: AccessUnit) {
        if self.mpeg_ts
            && let Some(pipeline) = &mut self.ts_pipeline
        {
            for segment in pipeline.push(unit) {
                let _ = self.tx.send(segment);
            }
            return;
        }
        let Some(&track_index) = self.track_indices.get(&unit.track_id) else {
            return;
        };
        let pts = unit.pts;
        let has_video = self
            .tracks
            .iter()
            .any(|track| track.kind == MediaKind::Video);
        // Timestamps can stall (broken encoder/ingest). A pure PTS-based cut
        // then never fires and `samples` grows without bound; force a cut at
        // the hard sample cap for audio-only streams. Video must never be cut
        // mid-GOP: a segment that does not start at an IDR is undecodable.
        if self.tracks[track_index].samples.len() >= MAX_SAMPLES_PER_SEGMENT && !has_video {
            self.finalize_segment(pts);
        }
        // A reset of this track's own timeline (timestamp wrap / source
        // restart) is detected against the track's last PTS. A global
        // comparison against the segment start would mistake independent
        // audio/video clock baselines for a reset once they drift >30 s
        // apart — which produced a discontinuity storm and 0-length
        // segments on long-running multi-track streams.
        let track_reset = self.tracks[track_index]
            .last_pts
            .is_some_and(|last| last.saturating_sub(pts) > Duration::from_secs(30));
        // The reset flag must land on the segment *after* the cut (the one
        // starting at the restart frame), not on the finalized pre-reset
        // segment: players reset their decoder where they see the flag.
        let mut delayed_discontinuity = false;
        let starts_new_segment = if track_reset {
            delayed_discontinuity = true;
            true
        } else {
            self.current_segment_pts.is_some_and(|start| {
                if pts < start {
                    // A small backward delta is just the difference
                    // between track start baselines (audio vs video) and
                    // must not cut a segment.
                    return false;
                }
                let elapsed = pts.saturating_sub(start);
                if elapsed < self.config.segment_duration {
                    return false;
                }
                let hard_limit = self
                    .config
                    .segment_duration
                    .saturating_mul(2)
                    .max(Duration::from_millis(1));
                if elapsed >= hard_limit && !has_video {
                    // Hard time-based cut is only valid for audio-only
                    // streams. Cutting a video segment before an IDR makes
                    // every following segment undecodable (HLS.js black
                    // screen, ffprobe "missing reference picture").
                    return true;
                }
                // Regular cut: align with a video random-access point.
                unit.is_keyframe && matches!(unit.codec, Codec::H264 | Codec::H265)
            })
        };
        if unit.is_keyframe && matches!(unit.codec, Codec::H264 | Codec::H265) {
            tracing::debug!(?pts, ?self.current_segment_pts, "HLS video random-access point");
        }
        // Learn the codec configuration *before* a possible cut: the first
        // keyframe of a recording that started mid-GOP both carries the
        // SPS/PPS and triggers finalization of the previous segment. Building
        // the init segment first would cache an empty avcC/hvcC.
        if self.tracks[track_index].codec_config.is_none() {
            self.tracks[track_index].codec_config =
                codec_config_from_annex_b(unit.codec, &unit.data);
        }
        if starts_new_segment {
            self.finalize_segment(pts);
        }
        // Apply a pending reset flag to the *new* segment only.
        if delayed_discontinuity {
            self.next_discontinuity = true;
        }
        if self.current_segment_pts.is_none() {
            self.current_segment_pts = Some(pts);
        }

        let track = &mut self.tracks[track_index];
        track.last_pts = Some(pts);
        let dts = unit.dts.unwrap_or(pts);
        let sample_duration = track
            .last_dts
            .map(|last| dts.saturating_sub(last))
            .filter(|duration| !duration.is_zero())
            .unwrap_or_else(|| Duration::from_millis(40));
        track.last_dts = Some(dts);
        let data = match unit.codec {
            Codec::H264 | Codec::H265 => annex_b_to_length_prefixed(unit.codec, &unit.data),
            _ => unit.data,
        };
        track.samples.push(SampleEntry {
            data,
            pts,
            dts,
            is_keyframe: unit.is_keyframe,
            duration: sample_duration,
        });
    }

    fn finalize_segment(&mut self, end_pts: Duration) {
        // Check if we have any samples
        let has_samples = self.tracks.iter().any(|t| !t.samples.is_empty());
        if !has_samples {
            return;
        }

        // Build init segment if not cached
        if self.init_segment.is_none() {
            self.init_segment = Some(build_init_segment(&self.description, &self.tracks));
        }

        // Build media segment
        let segment_data = build_media_segment(&mut self.tracks, self.sequence);
        let segment_pts = self.current_segment_pts.unwrap_or(end_pts);
        let segment_duration = end_pts.saturating_sub(segment_pts);

        let starts_with_keyframe = self.tracks.iter().any(|track| {
            track.kind == MediaKind::Video
                && track
                    .samples
                    .first()
                    .is_some_and(|sample| sample.is_keyframe)
        });

        let discontinuity_flag = std::mem::take(&mut self.next_discontinuity);
        let segment_discontinuity_sequence = if discontinuity_flag {
            self.discontinuity_counter += 1;
            Some(self.discontinuity_counter)
        } else {
            None
        };
        let segment = Segment {
            sequence: self.sequence,
            duration: segment_duration,
            init: self.init_segment.clone().unwrap_or_default(),
            data: segment_data,
            starts_with_keyframe,
            discontinuity: discontinuity_flag,
            discontinuity_sequence: segment_discontinuity_sequence,
            timestamp: segment_pts,
            format: SegmentFormat::Fmp4,
        };

        tracing::debug!(sequence = segment.sequence, ?segment.duration, "HLS segment finalized");
        let _ = self.tx.send(segment);
        self.sequence += 1;
        self.current_segment_pts = Some(end_pts);

        // Clear samples for next segment
        for track in &mut self.tracks {
            track.samples.clear();
        }
    }

    /// Finalize the current segment at the last sample's end.
    ///
    /// DASH uses this to reap an audio segment when its video counterpart is
    /// cut, keeping the two tracks' segment boundaries close without changing
    /// the HLS segmenter's own keyframe-driven cuts.
    pub fn cut_current(&mut self) {
        if self.mpeg_ts {
            return;
        }
        let end = self
            .tracks
            .iter()
            .flat_map(|track| track.samples.iter())
            .map(|sample| sample.pts.saturating_add(sample.duration))
            .max();
        if let Some(end) = end {
            self.finalize_segment(end);
        }
    }

    /// Drop incomplete samples after a transport gap and mark the next segment.
    pub fn reset_after_discontinuity(&mut self) {
        if let Some(pipeline) = &mut self.ts_pipeline {
            pipeline.mark_discontinuity();
        }
        for track in &mut self.tracks {
            track.samples.clear();
            track.last_dts = None;
            track.last_pts = None;
            track.codec_config = None;
        }
        self.current_segment_pts = None;
        self.init_segment = None;
        self.next_discontinuity = true;
    }

    /// Flush any remaining samples as a final segment.
    pub fn flush(&mut self) {
        if self.mpeg_ts
            && let Some(pipeline) = &mut self.ts_pipeline
        {
            for segment in pipeline.flush() {
                let _ = self.tx.send(segment);
            }
            return;
        }
        let has_samples = self.tracks.iter().any(|t| !t.samples.is_empty());
        if !has_samples {
            return;
        }

        if self.init_segment.is_none() {
            self.init_segment = Some(build_init_segment(&self.description, &self.tracks));
        }

        let segment_data = build_media_segment(&mut self.tracks, self.sequence);
        let segment_pts = self.current_segment_pts.unwrap_or_default();

        let end_pts = self
            .tracks
            .iter()
            .flat_map(|track| track.samples.iter())
            .map(|sample| sample.pts.saturating_add(sample.duration))
            .max()
            .unwrap_or(segment_pts);
        let discontinuity_flag = std::mem::take(&mut self.next_discontinuity);
        let segment_discontinuity_sequence = if discontinuity_flag {
            self.discontinuity_counter += 1;
            Some(self.discontinuity_counter)
        } else {
            None
        };
        let segment = Segment {
            sequence: self.sequence,
            duration: end_pts.saturating_sub(segment_pts),
            init: self.init_segment.clone().unwrap_or_default(),
            data: segment_data,
            starts_with_keyframe: self.tracks.iter().any(|track| {
                track.kind == MediaKind::Video
                    && track
                        .samples
                        .first()
                        .is_some_and(|sample| sample.is_keyframe)
            }),
            discontinuity: discontinuity_flag,
            discontinuity_sequence: segment_discontinuity_sequence,
            timestamp: segment_pts,
            format: SegmentFormat::Fmp4,
        };

        let _ = self.tx.send(segment);
        self.sequence += 1;

        for track in &mut self.tracks {
            track.samples.clear();
        }
    }
}
