use rtflow_core::{
    error::{Error, Result},
    identity::StreamEpoch,
};
use rtflow_media::{AccessUnit, MediaDescription, MediaKind, TrackDescription};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

#[derive(Clone, Copy, Debug)]
pub struct ContinuityPolicy {
    pub preserve_rtp_timestamps: bool,
    pub preserve_presentation_time: bool,
    pub maximum_gap: Duration,
}

impl Default for ContinuityPolicy {
    fn default() -> Self {
        Self {
            preserve_rtp_timestamps: true,
            preserve_presentation_time: true,
            maximum_gap: Duration::from_millis(500),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineSnapshot {
    pub epoch: StreamEpoch,
    pub tracks: usize,
}

struct TrackTimeline {
    clock_rate: u32,
    new_epoch: bool,
    timestamp_offset: u32,
    time_offset_ns: i128,
    previous_input_timestamp: Option<u32>,
    last_output_timestamp: Option<u32>,
    last_output_pts: Option<Duration>,
    estimated_step: Option<u32>,
}

pub struct TimelineNormalizer {
    policy: ContinuityPolicy,
    epoch: StreamEpoch,
    tracks: HashMap<u8, TrackTimeline>,
}

impl TimelineNormalizer {
    pub fn new(description: &MediaDescription, policy: ContinuityPolicy) -> Result<Self> {
        if policy.maximum_gap == Duration::ZERO {
            return Err(Error::Limit("proxy timeline maximum gap"));
        }
        let mut tracks = HashMap::with_capacity(description.tracks.len());
        for track in &description.tracks {
            if track.clock_rate == 0 {
                return Err(Error::Media("proxy track clock rate is zero".into()));
            }
            tracks.insert(
                track.id,
                TrackTimeline {
                    clock_rate: track.clock_rate,
                    new_epoch: true,
                    timestamp_offset: 0,
                    time_offset_ns: 0,
                    previous_input_timestamp: None,
                    last_output_timestamp: None,
                    last_output_pts: None,
                    estimated_step: None,
                },
            );
        }
        Ok(Self {
            policy,
            epoch: StreamEpoch::new(0),
            tracks,
        })
    }

    pub fn begin_epoch(&mut self) -> StreamEpoch {
        self.epoch = StreamEpoch::new(self.epoch.get().saturating_add(1));
        for track in self.tracks.values_mut() {
            track.new_epoch = true;
            track.previous_input_timestamp = None;
        }
        self.epoch
    }

    pub fn normalize(&mut self, frame: &mut AccessUnit) -> Result<()> {
        let track = self.tracks.get_mut(&frame.track_id).ok_or_else(|| {
            Error::Media(format!("proxy timeline has no track {}", frame.track_id))
        })?;
        let input_timestamp = frame.rtp_timestamp;
        if track.new_epoch {
            if let Some(last_timestamp) = track.last_output_timestamp {
                let maximum_step = duration_to_ticks(self.policy.maximum_gap, track.clock_rate);
                let step = track
                    .estimated_step
                    .unwrap_or(1)
                    .clamp(1, maximum_step.max(1));
                let next_timestamp = last_timestamp.wrapping_add(step);
                track.timestamp_offset = next_timestamp.wrapping_sub(input_timestamp);
                if let Some(last_pts) = track.last_output_pts {
                    let desired =
                        last_pts.saturating_add(ticks_to_duration(step, track.clock_rate));
                    track.time_offset_ns = duration_ns(desired) - duration_ns(frame.pts);
                }
            } else {
                track.timestamp_offset = 0;
                track.time_offset_ns = 0;
            }
            track.new_epoch = false;
        } else if let Some(previous) = track.previous_input_timestamp {
            let step = input_timestamp.wrapping_sub(previous);
            let maximum_step = duration_to_ticks(self.policy.maximum_gap, track.clock_rate);
            if step > 0 && step <= maximum_step {
                track.estimated_step = Some(match track.estimated_step {
                    Some(estimate) => ((u64::from(estimate) * 7 + u64::from(step)) / 8) as u32,
                    None => step,
                });
            }
        }

        if self.policy.preserve_rtp_timestamps {
            frame.rtp_timestamp = input_timestamp.wrapping_add(track.timestamp_offset);
        }
        if self.policy.preserve_presentation_time {
            frame.pts = apply_time_offset(frame.pts, track.time_offset_ns);
            frame.dts = frame
                .dts
                .map(|value| apply_time_offset(value, track.time_offset_ns));
        }
        track.previous_input_timestamp = Some(input_timestamp);
        track.last_output_timestamp = Some(frame.rtp_timestamp);
        track.last_output_pts = Some(frame.pts);
        Ok(())
    }

    pub fn snapshot(&self) -> TimelineSnapshot {
        TimelineSnapshot {
            epoch: self.epoch,
            tracks: self.tracks.len(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StartupCacheConfig {
    pub enabled: bool,
    /// Per-track byte budget.
    pub max_bytes: usize,
    /// Total byte budget across all tracks of the mount.
    pub max_bytes_total: usize,
    pub max_frames_per_track: usize,
}

impl Default for StartupCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: crate::DEFAULT_MAX_QUEUED_BYTES_PER_SESSION,
            max_bytes_total: crate::DEFAULT_MAX_QUEUED_BYTES_PER_SESSION.saturating_mul(4),
            max_frames_per_track: 256,
        }
    }
}

struct TrackCache {
    kind: MediaKind,
    frames: VecDeque<Arc<AccessUnit>>,
    bytes: usize,
    starts_with_keyframe: bool,
}

pub struct KeyframeCache {
    config: StartupCacheConfig,
    tracks: HashMap<u8, TrackCache>,
}

impl KeyframeCache {
    pub fn new(description: &MediaDescription, config: StartupCacheConfig) -> Result<Self> {
        if config.enabled
            && (config.max_bytes == 0
                || config.max_bytes_total == 0
                || config.max_frames_per_track == 0)
        {
            return Err(Error::Limit("proxy startup cache"));
        }
        Ok(Self {
            config,
            tracks: description
                .tracks
                .iter()
                .map(|track| {
                    (
                        track.id,
                        TrackCache {
                            kind: track.kind,
                            frames: VecDeque::new(),
                            bytes: 0,
                            starts_with_keyframe: false,
                        },
                    )
                })
                .collect(),
        })
    }

    pub fn push(&mut self, frame: Arc<AccessUnit>) {
        if !self.config.enabled {
            return;
        }
        let Some(track) = self.tracks.get_mut(&frame.track_id) else {
            return;
        };
        if track.kind == MediaKind::Video && frame.is_keyframe {
            track.frames.clear();
            track.bytes = 0;
            track.starts_with_keyframe = true;
        }
        if track.kind == MediaKind::Video && !track.starts_with_keyframe {
            return;
        }
        track.bytes = track.bytes.saturating_add(frame.data.len());
        track.frames.push_back(frame);
        while track.frames.len() > self.config.max_frames_per_track {
            pop_front(track);
        }
        if track.bytes > self.config.max_bytes {
            // Roll off the oldest frames instead of dropping the whole
            // cache: a burst of large keyframes must not wipe the startup
            // snapshot and force new subscribers to wait for the next GOP.
            while track.bytes > self.config.max_bytes && !track.frames.is_empty() {
                pop_front(track);
            }
            // Preserve the "starts with a keyframe" invariant: a snapshot
            // that begins with a non-keyframe cannot be decoded from cold.
            if track.kind == MediaKind::Video {
                while track.frames.front().is_some_and(|frame| !frame.is_keyframe) {
                    pop_front(track);
                }
            }
            if track.frames.is_empty() {
                track.starts_with_keyframe = false;
            }
        }
        self.enforce_total_budget();
    }

    /// Evict from the largest track until the mount-wide byte budget holds.
    fn enforce_total_budget(&mut self) {
        if !self.config.enabled {
            return;
        }
        let mut total: usize = self.tracks.values().map(|track| track.bytes).sum();
        while total > self.config.max_bytes_total {
            let Some(id) = self
                .tracks
                .iter()
                .filter(|(_, track)| !track.frames.is_empty())
                .max_by_key(|(_, track)| track.bytes)
                .map(|(id, _)| *id)
            else {
                break;
            };
            let Some(track) = self.tracks.get_mut(&id) else {
                break;
            };
            if let Some(frame) = track.frames.pop_front() {
                track.bytes = track.bytes.saturating_sub(frame.data.len());
                total = total.saturating_sub(frame.data.len());
            }
            if track.kind == MediaKind::Video {
                while track.frames.front().is_some_and(|frame| !frame.is_keyframe) {
                    if let Some(frame) = track.frames.pop_front() {
                        track.bytes = track.bytes.saturating_sub(frame.data.len());
                        total = total.saturating_sub(frame.data.len());
                    }
                }
            }
            if track.frames.is_empty() {
                track.starts_with_keyframe = false;
            }
        }
    }

    pub fn snapshot(&self, track_id: u8) -> Vec<Arc<AccessUnit>> {
        self.tracks
            .get(&track_id)
            .map(|track| track.frames.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn snapshot_all(&self) -> Vec<Arc<AccessUnit>> {
        let mut frames = self
            .tracks
            .values()
            .flat_map(|track| track.frames.iter().cloned())
            .collect::<Vec<_>>();
        frames.sort_by_key(|frame| (frame.pts, frame.track_id));
        frames
    }

    pub fn clear(&mut self) {
        for track in self.tracks.values_mut() {
            track.frames.clear();
            track.bytes = 0;
            track.starts_with_keyframe = false;
        }
    }

    pub fn usage(&self) -> (usize, usize) {
        self.tracks.values().fold((0, 0), |(frames, bytes), track| {
            (frames + track.frames.len(), bytes + track.bytes)
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrackCompatibility {
    pub compatible: bool,
    pub differences: Vec<String>,
}

impl TrackCompatibility {
    pub fn compare(expected: &MediaDescription, candidate: &MediaDescription) -> Self {
        let mut differences = Vec::new();
        if expected.tracks.len() != candidate.tracks.len() {
            differences.push(format!(
                "track count changed from {} to {}",
                expected.tracks.len(),
                candidate.tracks.len()
            ));
        }
        for expected_track in &expected.tracks {
            match candidate.track(expected_track.id) {
                Some(candidate_track) => {
                    compare_track(expected_track, candidate_track, &mut differences)
                }
                None => differences.push(format!("track {} is missing", expected_track.id)),
            }
        }
        Self {
            compatible: differences.is_empty(),
            differences,
        }
    }
}

fn compare_track(expected: &TrackDescription, candidate: &TrackDescription, out: &mut Vec<String>) {
    if expected.codec != candidate.codec {
        out.push(format!("track {} codec changed", expected.id));
    }
    if expected.clock_rate != candidate.clock_rate {
        out.push(format!("track {} clock rate changed", expected.id));
    }
    if expected.channels != candidate.channels {
        out.push(format!("track {} channel count changed", expected.id));
    }
}

fn pop_front(track: &mut TrackCache) {
    if let Some(frame) = track.frames.pop_front() {
        track.bytes = track.bytes.saturating_sub(frame.data.len());
    }
}

fn duration_to_ticks(duration: Duration, clock_rate: u32) -> u32 {
    rtflow_media::clock::duration_to_ticks(duration, clock_rate).clamp(1, u64::from(u32::MAX))
        as u32
}

fn ticks_to_duration(ticks: u32, clock_rate: u32) -> Duration {
    rtflow_media::clock::ticks_to_duration(u64::from(ticks), clock_rate)
}

fn duration_ns(value: Duration) -> i128 {
    value.as_nanos().min(i128::MAX as u128) as i128
}

fn apply_time_offset(value: Duration, offset_ns: i128) -> Duration {
    let adjusted = duration_ns(value).saturating_add(offset_ns).max(0) as u128;
    Duration::from_nanos(adjusted.min(u128::from(u64::MAX)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rtflow_media::{Codec, MediaKind, TrackDescription};
    use std::sync::Arc;

    fn description(codec: Codec) -> MediaDescription {
        MediaDescription {
            session_name: Arc::from("proxy"),
            tracks: vec![TrackDescription {
                id: 0,
                kind: MediaKind::Video,
                codec,
                clock_rate: rtflow_media::clock::VIDEO_CLOCK_RATE,
                channels: None,
                payload_type: 96,
                fmtp: None,
                control: Arc::from("trackID=0"),
                width: None,
                height: None,
                frame_rate: None,
                crypto: None,
                direction: None,
                ptime: None,
                rtcp: None,
                bandwidth_as: None,
                rtx: None,
            }],
            connection: None,
        }
    }

    fn frame(timestamp: u32, pts_ms: u64, keyframe: bool, data: Bytes) -> AccessUnit {
        AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(pts_ms),
            dts: None,
            rtp_timestamp: timestamp,
            is_keyframe: keyframe,
            data,
        }
    }

    #[test]
    fn reconnect_keeps_timestamp_and_pts_monotonic() {
        let media = description(Codec::H264);
        let mut timeline = TimelineNormalizer::new(&media, ContinuityPolicy::default()).unwrap();
        timeline.begin_epoch();
        let mut first = frame(9_000, 100, true, Bytes::from_static(b"a"));
        let mut second = frame(12_000, 133, false, Bytes::from_static(b"b"));
        timeline.normalize(&mut first).unwrap();
        timeline.normalize(&mut second).unwrap();
        assert_eq!((first.rtp_timestamp, second.rtp_timestamp), (9_000, 12_000));

        timeline.begin_epoch();
        let mut restarted = frame(0, 0, true, Bytes::from_static(b"c"));
        timeline.normalize(&mut restarted).unwrap();
        assert_eq!(restarted.rtp_timestamp, 15_000);
        assert!(restarted.pts > second.pts);
        assert_eq!(timeline.snapshot().epoch, StreamEpoch::new(2));
    }

    #[test]
    fn cache_starts_at_keyframe_and_shares_payload_storage() {
        let media = description(Codec::H264);
        let mut cache = KeyframeCache::new(&media, StartupCacheConfig::default()).unwrap();
        cache.push(std::sync::Arc::new(frame(
            1,
            0,
            false,
            Bytes::from_static(b"ignored"),
        )));
        assert!(cache.snapshot(0).is_empty());

        let key_data = Bytes::from_static(b"key");
        let pointer = key_data.as_ptr();
        cache.push(std::sync::Arc::new(frame(2, 33, true, key_data)));
        cache.push(std::sync::Arc::new(frame(
            3,
            66,
            false,
            Bytes::from_static(b"delta"),
        )));
        let snapshot = cache.snapshot(0);
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot[0].is_keyframe);
        assert_eq!(snapshot[0].data.as_ptr(), pointer);
    }

    #[test]
    fn incompatible_codec_is_reported_before_failover() {
        let expected = description(Codec::H264);
        let candidate = description(Codec::H265);
        let result = TrackCompatibility::compare(&expected, &candidate);
        assert!(!result.compatible);
        assert_eq!(result.differences, vec!["track 0 codec changed"]);
    }
}
