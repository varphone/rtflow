//! MPEG-DASH MPD (Media Presentation Description) XML generation.
//!
//! Generates dynamic (live) MPD documents using SegmentTemplate with $Number$
//! substitution, conforming to the DASH-IF IOP live profile.
use rtflow_hls::segmenter::Segment;
use rtflow_media::{MediaDescription, MediaKind};
use std::time::{Duration, SystemTime};

/// DASH MPD configuration.
#[derive(Clone, Debug)]
pub struct MpdConfig {
    /// Minimum buffer time suggested to clients.
    pub min_buffer_time: Duration,
    /// Time shift buffer depth (sliding window duration).
    pub time_shift_buffer_depth: Duration,
    /// Segment duration (for SegmentTemplate@duration).
    pub segment_duration: Duration,
    /// Fallback timescale for SegmentTemplate when a track has no clock rate.
    pub timescale: u32,
    /// Suggested presentation delay (live edge offset).
    pub suggested_presentation_delay: Option<Duration>,
    /// Optional HTTP ISO-8601 time source for `UTCTiming`.
    pub utc_timing_url: Option<String>,
}

impl MpdConfig {
    pub fn validate(&self) -> rtflow_core::error::Result<()> {
        if self.segment_duration.is_zero() {
            return Err(rtflow_core::error::Error::Limit(
                "DASH MPD segment duration",
            ));
        }
        if self.timescale == 0 {
            return Err(rtflow_core::error::Error::Limit("DASH MPD timescale"));
        }
        if self.min_buffer_time.is_zero() {
            return Err(rtflow_core::error::Error::Limit("DASH MPD min buffer"));
        }
        if self.time_shift_buffer_depth.is_zero() {
            return Err(rtflow_core::error::Error::Limit(
                "DASH MPD time shift buffer",
            ));
        }
        if self
            .suggested_presentation_delay
            .is_some_and(|delay| delay.is_zero())
        {
            return Err(rtflow_core::error::Error::Limit(
                "DASH MPD suggested presentation delay",
            ));
        }
        Ok(())
    }
}

impl Default for MpdConfig {
    fn default() -> Self {
        Self {
            min_buffer_time: Duration::from_secs(2),
            time_shift_buffer_depth: Duration::from_secs(10),
            segment_duration: Duration::from_secs(2),
            timescale: rtflow_media::clock::VIDEO_CLOCK_RATE,
            suggested_presentation_delay: None,
            utc_timing_url: None,
        }
    }
}

/// One video rendition in the DASH MPD.
///
/// `id` is the source track id used in the segment URLs; `codec` and
/// `dimensions` are derived from the track's init segment when available.
pub struct MpdVideoTrack<'a> {
    pub id: u8,
    pub track: &'a rtflow_media::TrackDescription,
    pub segments: &'a [Segment],
    pub codec: Option<String>,
    pub dimensions: Option<(u16, u16)>,
}

/// Generate a dynamic MPD XML document.
///
/// HLS and DASH share the HLS segmenter's fMP4 implementation, but DASH
/// packages each track independently (like SRS): one AdaptationSet per track,
/// `video-init.mp4`/`audio-init.mp4` and `video-N.m4s`/`audio-N.m4s`.
/// Segment URLs are relative to the MPD URL so the same manifest works at
/// `/dash/{mount}/...` and behind a reverse proxy.
#[allow(clippy::too_many_arguments)]
pub fn generate_mpd(
    video: &[MpdVideoTrack<'_>],
    audio: &[Segment],
    description: &MediaDescription,
    config: &MpdConfig,
    availability_start: SystemTime,
    base_timestamp: Duration,
) -> String {
    generate_mpd_q(
        video,
        audio,
        description,
        config,
        availability_start,
        base_timestamp,
        "",
    )
}

/// Query-suffixed variant of [`generate_mpd`] (e.g. `?token=...`).
#[allow(clippy::too_many_arguments)]
pub fn generate_mpd_q(
    video: &[MpdVideoTrack<'_>],
    audio: &[Segment],
    description: &MediaDescription,
    config: &MpdConfig,
    availability_start: SystemTime,
    base_timestamp: Duration,
    query: &str,
) -> String {
    let mut xml = String::with_capacity(4096);

    let effective_start = if availability_start == SystemTime::UNIX_EPOCH {
        SystemTime::now()
    } else {
        availability_start
    };
    let ast_duration = effective_start
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let availability_start_str = iso8601_from_duration(ast_duration);

    let now_duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let publish_time = iso8601_from_duration(now_duration);

    let suggested_delay = config
        .suggested_presentation_delay
        .unwrap_or(config.segment_duration * 3);
    let max_segment_duration = video
        .iter()
        .flat_map(|track| track.segments.iter())
        .chain(audio)
        .map(|segment| segment.duration)
        .max()
        .unwrap_or(config.segment_duration);

    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" \
         type=\"dynamic\" \
         profiles=\"urn:mpeg:dash:profile:isoff-live:2011\" \
         minBufferTime=\"{}\" \
         timeShiftBufferDepth=\"{}\" \
         suggestedPresentationDelay=\"{}\" \
         maximumSegmentDuration=\"{}\" \
         minimumUpdatePeriod=\"{}\" \
         availabilityStartTime=\"{}\" \
         publishTime=\"{}\">\n",
        format_duration(config.min_buffer_time),
        format_duration(config.time_shift_buffer_depth),
        format_duration(suggested_delay),
        format_duration(max_segment_duration),
        format_duration(config.segment_duration),
        availability_start_str,
        publish_time,
    ));

    xml.push_str("  <Period id=\"0\" start=\"PT0S\">\n");
    let suffix = query_suffix(query);

    if !video.is_empty() {
        push_video_adaptation_set(&mut xml, video, config, base_timestamp, &suffix);
    }
    if let Some(track) = description
        .tracks
        .iter()
        .find(|track| track.kind == MediaKind::Audio)
        && !audio.is_empty()
    {
        push_audio_adaptation_set(&mut xml, audio, track, config, base_timestamp, &suffix);
    }

    xml.push_str("  </Period>\n");
    if let Some(url) = &config.utc_timing_url {
        xml.push_str(&format!(
            "  <UTCTiming schemeIdUri=\"urn:mpeg:dash:utc:http-iso:2014\" value=\"{url}\"/>\n"
        ));
    } else {
        xml.push_str(&format!(
            "  <UTCTiming schemeIdUri=\"urn:mpeg:dash:utc:direct:2014\" value=\"{publish_time}\"/>\n"
        ));
    }
    xml.push_str("</MPD>\n");
    xml
}

/// Generate a static MPD for DASH VOD.
#[allow(clippy::too_many_arguments)]
pub fn generate_static_mpd(
    video: &[MpdVideoTrack<'_>],
    audio: &[Segment],
    description: &MediaDescription,
    config: &MpdConfig,
    duration: Duration,
    query: &str,
) -> String {
    let mut xml = String::with_capacity(4096);
    let duration = if duration.is_zero() {
        video
            .iter()
            .flat_map(|track| track.segments.iter())
            .chain(audio)
            .map(|segment| segment.timestamp.saturating_add(segment.duration))
            .max()
            .unwrap_or_default()
    } else {
        duration
    };
    let suffix = query_suffix(query);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" type=\"static\" profiles=\"urn:mpeg:dash:profile:isoff-live:2011\" minBufferTime=\"{}\" mediaPresentationDuration=\"{}\" >\n",
        format_duration(config.min_buffer_time),
        format_duration(duration)
    ));
    xml.push_str(&format!(
        "  <Period id=\"0\" start=\"PT0S\" duration=\"{}\">\n",
        format_duration(duration)
    ));
    if !video.is_empty() {
        push_video_adaptation_set(&mut xml, video, config, Duration::ZERO, &suffix);
    }
    if let Some(track) = description
        .tracks
        .iter()
        .find(|track| track.kind == MediaKind::Audio)
        && !audio.is_empty()
    {
        push_audio_adaptation_set(&mut xml, audio, track, config, Duration::ZERO, &suffix);
    }
    xml.push_str("  </Period>\n");
    xml.push_str("</MPD>\n");
    xml
}

/// Push one video AdaptationSet containing one Representation per rendition.
fn push_video_adaptation_set(
    xml: &mut String,
    video: &[MpdVideoTrack<'_>],
    config: &MpdConfig,
    base_timestamp: Duration,
    suffix: &str,
) {
    if video.is_empty() {
        return;
    }
    // With multiple independent renditions their segment boundaries are not
    // guaranteed to be aligned; do not claim alignment in the MPD.
    let alignment = if video.len() > 1 { "false" } else { "true" };
    xml.push_str(&format!(
        "    <AdaptationSet mimeType=\"video/mp4\" segmentAlignment=\"{alignment}\" startWithSAP=\"1\">\n"
    ));
    for rendition in video {
        let segments = rendition.segments;
        if segments.is_empty() {
            continue;
        }
        let track = rendition.track;
        let timescale = if track.clock_rate > 0 {
            track.clock_rate
        } else {
            config.timescale
        };
        let start_number = segments
            .first()
            .map(|segment| segment.sequence)
            .unwrap_or(0);
        let codecs = rendition
            .codec
            .as_deref()
            .unwrap_or_else(|| video_codec_string(track.codec));
        let (width, height) = rendition
            .dimensions
            .map(|(width, height)| (width as u32, height as u32))
            .unwrap_or_else(|| (track.width.unwrap_or(1920), track.height.unwrap_or(1080)));
        let frame_rate = video_frame_rate(segments, timescale)
            .or(track.frame_rate)
            .map(|(numerator, denominator)| format!("{numerator}/{denominator}"))
            .unwrap_or_else(|| "30".to_string());
        let bandwidth = estimated_bandwidth(segments, true);
        let id = rendition.id;

        xml.push_str(&format!(
            "      <Representation id=\"video-{id}\" bandwidth=\"{bandwidth}\" codecs=\"{codecs}\" width=\"{width}\" height=\"{height}\" frameRate=\"{frame_rate}\">\n"
        ));
        xml.push_str(&format!(
            "        <SegmentTemplate timescale=\"{timescale}\" startNumber=\"{start_number}\" initialization=\"video-{id}-init.mp4{suffix}\" media=\"video-{id}-$Number$.m4s{suffix}\">\n"
        ));
        push_timeline(xml, segments, base_timestamp, timescale);
        xml.push_str("        </SegmentTemplate>\n");
        xml.push_str("      </Representation>\n");
    }
    xml.push_str("    </AdaptationSet>\n");
}

fn push_audio_adaptation_set(
    xml: &mut String,
    segments: &[Segment],
    track: &rtflow_media::TrackDescription,
    config: &MpdConfig,
    base_timestamp: Duration,
    suffix: &str,
) {
    let timescale = if track.clock_rate > 0 {
        track.clock_rate
    } else {
        config.timescale
    };
    let start_number = segments
        .first()
        .map(|segment| segment.sequence)
        .unwrap_or(0);
    let codecs = audio_codec_string(track.codec);
    let bandwidth = estimated_bandwidth(segments, false);
    let channels = track.channels.unwrap_or(2);

    xml.push_str(
        "    <AdaptationSet mimeType=\"audio/mp4\" segmentAlignment=\"true\" startWithSAP=\"1\">\n",
    );
    xml.push_str(&format!(
        "      <Representation id=\"audio\" bandwidth=\"{bandwidth}\" codecs=\"{codecs}\" audioSamplingRate=\"{}\">\n",
        track.clock_rate
    ));
    xml.push_str(&format!(
        "        <AudioChannelConfiguration schemeIdUri=\"urn:mpeg:dash:23003:3:audio_channel_configuration:2011\" value=\"{channels}\"/>\n"
    ));
    xml.push_str(&format!(
        "        <SegmentTemplate timescale=\"{timescale}\" startNumber=\"{start_number}\" initialization=\"audio-init.mp4{suffix}\" media=\"audio-$Number$.m4s{suffix}\">\n"
    ));
    push_timeline(xml, segments, base_timestamp, timescale);
    xml.push_str("        </SegmentTemplate>\n");
    xml.push_str("      </Representation>\n");
    xml.push_str("    </AdaptationSet>\n");
}

fn push_timeline(xml: &mut String, segments: &[Segment], base_timestamp: Duration, timescale: u32) {
    xml.push_str("          <SegmentTimeline>\n");
    for segment in segments {
        let relative_ts = segment.timestamp.saturating_sub(base_timestamp);
        let t_ticks = duration_to_ticks(relative_ts, timescale);
        let d_ticks = duration_to_ticks(segment.duration, timescale).max(1);
        xml.push_str(&format!(
            "            <S t=\"{t_ticks}\" d=\"{d_ticks}\"/>\n"
        ));
    }
    xml.push_str("          </SegmentTimeline>\n");
}

fn query_suffix(query: &str) -> String {
    if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    }
}

/// Estimate average video frame rate from the `trun` sample durations in
/// fMP4 media segments. Falls back to the track description when the data is
/// unavailable or not fMP4.
fn video_frame_rate(segments: &[Segment], timescale: u32) -> Option<(u32, u32)> {
    if timescale == 0 {
        return None;
    }
    let mut samples: u64 = 0;
    let mut duration_ticks: u64 = 0;
    for segment in segments {
        let Some((count, ticks)) = trun_stats(&segment.data) else {
            continue;
        };
        samples += count as u64;
        duration_ticks += ticks;
    }
    if samples == 0 || duration_ticks == 0 {
        return None;
    }
    let fps = samples as f64 * f64::from(timescale) / duration_ticks as f64;
    if !(1.0..=240.0).contains(&fps) {
        return None;
    }
    Some((fps.round() as u32, 1))
}

/// Sum sample count and sample durations over all `trun` boxes in an fMP4
/// media segment.
fn trun_stats(data: &[u8]) -> Option<(u32, u64)> {
    let mut count = 0u32;
    let mut ticks = 0u64;
    let mut offset = 0usize;
    while offset + 16 <= data.len() {
        let Some(position) = data[offset..]
            .windows(4)
            .position(|window| window == b"trun")
        else {
            break;
        };
        let type_pos = offset + position;
        let box_start = type_pos.checked_sub(4)?;
        let size = u32::from_be_bytes(data[box_start..box_start + 4].try_into().ok()?) as usize;
        let box_end = box_start.checked_add(size)?;
        if size < 16 || box_end > data.len() {
            offset = type_pos + 4;
            continue;
        }
        let flags = u32::from_be_bytes(data[type_pos + 4..type_pos + 8].try_into().ok()?);
        let sample_count = u32::from_be_bytes(data[type_pos + 8..type_pos + 12].try_into().ok()?);
        let sample_duration = flags & 0x000100 != 0;
        let sample_size = flags & 0x000200 != 0;
        let sample_flags = flags & 0x000400 != 0;
        let sample_cts = flags & 0x000800 != 0;
        let _data_offset = flags & 0x000001 != 0;
        // run data starts right after the optional data_offset field, which
        // occupies the 4 bytes immediately after sample_count.
        let mut cursor = type_pos + 16;
        let mut sample_ticks = 0u64;
        for _ in 0..sample_count {
            if cursor + 4 > box_end {
                return None;
            }
            if sample_duration {
                sample_ticks += u64::from(u32::from_be_bytes(
                    data[cursor..cursor + 4].try_into().ok()?,
                ));
                cursor += 4;
            }
            if sample_size {
                cursor += 4;
            }
            if sample_flags {
                cursor += 4;
            }
            if sample_cts {
                cursor += 4;
            }
            if cursor > box_end {
                return None;
            }
        }
        count += sample_count;
        ticks += sample_ticks;
        offset = box_end;
    }
    (count > 0).then_some((count, ticks))
}

fn estimated_bandwidth(segments: &[Segment], video: bool) -> u64 {
    let bytes: u64 = segments
        .iter()
        .map(|segment| segment.data.len() as u64)
        .sum();
    let seconds: f64 = segments
        .iter()
        .map(|segment| segment.duration.as_secs_f64())
        .sum();
    if bytes == 0 || seconds <= 0.0 {
        return if video { 2_000_000 } else { 128_000 };
    }
    ((bytes as f64 * 8.0 / seconds).ceil() as u64).max(1)
}

/// Convert a Duration to ticks at the given timescale.
fn duration_to_ticks(d: Duration, timescale: u32) -> u64 {
    rtflow_media::clock::duration_to_ticks(d, timescale)
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let frac = d.subsec_millis();
    format!("PT{}.{:03}S", secs, frac)
}

/// Format current time as ISO 8601 UTC.
#[allow(dead_code)]
fn iso8601_now() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    iso8601_from_duration(now)
}

/// Format a duration since epoch as ISO 8601 UTC.
fn iso8601_from_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let days = total_secs / 86400;
    let time_secs = total_secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since 1970-01-01 -> approximate Y/M/D
    let (year, month, day) = days_to_ymd(days);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn video_codec_string(codec: rtflow_media::Codec) -> &'static str {
    match codec {
        rtflow_media::Codec::H264 => "avc1.42E01E",
        // fMP4 sample entry is hvc1 (parameter sets in-band), so the codecs
        // string must match: hev1 would make Chrome/Safari reject the
        // stream as mislabeled.
        rtflow_media::Codec::H265 => "hvc1.1.6.L93.B0",
        _ => "avc1.42E01E",
    }
}

fn audio_codec_string(codec: rtflow_media::Codec) -> &'static str {
    match codec {
        rtflow_media::Codec::Aac => "mp4a.40.2",
        rtflow_media::Codec::Opus => "opus",
        // RFC 6381 audio object types for G.711.
        rtflow_media::Codec::Pcma => "alaw",
        rtflow_media::Codec::Pcmu => "ulaw",
        _ => "mp4a.40.2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rtflow_media::{Codec, TrackDescription};

    fn test_description() -> MediaDescription {
        MediaDescription {
            session_name: "test".into(),
            tracks: vec![
                TrackDescription {
                    id: 0,
                    kind: MediaKind::Video,
                    codec: Codec::H264,
                    clock_rate: rtflow_media::clock::VIDEO_CLOCK_RATE,
                    channels: None,
                    payload_type: 96,
                    fmtp: None,
                    control: "trackID=0".into(),
                    width: Some(1280),
                    height: Some(720),
                    frame_rate: Some((30, 1)),
                    crypto: None,
                    direction: None,
                    ptime: None,
                    rtcp: None,
                    bandwidth_as: None,
                    rtx: None,
                },
                TrackDescription {
                    id: 1,
                    kind: MediaKind::Audio,
                    codec: Codec::Aac,
                    clock_rate: 44100,
                    channels: Some(2),
                    payload_type: 97,
                    fmtp: None,
                    control: "trackID=1".into(),
                    width: None,
                    height: None,
                    frame_rate: None,
                    crypto: None,
                    direction: None,
                    ptime: None,
                    rtcp: None,
                    bandwidth_as: None,
                    rtx: None,
                },
            ],
            connection: None,
        }
    }

    fn segment(sequence: u64, timestamp_secs: u64) -> Segment {
        Segment {
            sequence,
            duration: Duration::from_secs(2),
            init: Bytes::new(),
            data: Bytes::new(),
            starts_with_keyframe: true,
            discontinuity: false,
            discontinuity_sequence: None,
            timestamp: Duration::from_secs(timestamp_secs),
            format: rtflow_hls::segmenter::SegmentFormat::Fmp4,
        }
    }

    fn video_rep<'a>(
        description: &'a MediaDescription,
        segments: &'a [Segment],
        codec: Option<&str>,
    ) -> MpdVideoTrack<'a> {
        let track = description
            .tracks
            .iter()
            .find(|track| track.kind == MediaKind::Video)
            .expect("video track");
        MpdVideoTrack {
            id: track.id,
            track,
            segments,
            codec: codec.map(str::to_owned),
            dimensions: None,
        }
    }

    #[test]
    fn generates_per_track_mpd() {
        let description = test_description();
        let video = vec![segment(1, 0), segment(2, 2)];
        let audio = vec![segment(1, 0), segment(2, 2)];
        let video_rep = video_rep(&description, &video, None);
        let mpd = generate_mpd(
            &[video_rep],
            &audio,
            &description,
            &MpdConfig::default(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            Duration::ZERO,
        );

        assert!(mpd.contains("<?xml version=\"1.0\""));
        assert!(mpd.contains("type=\"dynamic\""));
        assert!(mpd.contains("urn:mpeg:dash:profile:isoff-live:2011"));
        assert_eq!(mpd.matches("<AdaptationSet").count(), 2);
        assert_eq!(mpd.matches("<SegmentTemplate").count(), 2);
        assert!(mpd.contains("initialization=\"video-0-init.mp4\""));
        assert!(mpd.contains("media=\"video-0-$Number$.m4s\""));
        assert!(mpd.contains("initialization=\"audio-init.mp4\""));
        assert!(mpd.contains("media=\"audio-$Number$.m4s\""));
        assert!(mpd.contains("codecs=\"avc1.42E01E\""));
        assert!(mpd.contains("codecs=\"mp4a.40.2\""));
        assert!(mpd.contains("<Representation id=\"video-0\""));
        assert!(mpd.contains("<Representation id=\"audio\""));
        assert!(mpd.contains("startNumber=\"1\""));
        assert!(mpd.contains("<UTCTiming"));
        assert!(mpd.contains("<S t=\"0\" d=\"180000\"/>"));
        assert!(mpd.contains("<S t=\"180000\" d=\"180000\"/>"));
        assert!(mpd.contains("<S t=\"0\" d=\"88200\"/>"));
        assert!(mpd.contains("<S t=\"88200\" d=\"88200\"/>"));
        assert!(mpd.contains("suggestedPresentationDelay="));
        assert!(mpd.contains("maximumSegmentDuration="));
        assert!(mpd.contains("publishTime="));
        assert!(mpd.contains("width=\"1280\""));
        assert!(mpd.contains("height=\"720\""));
        assert!(mpd.contains("frameRate=\"30/1\""));
        assert!(mpd.contains("audioSamplingRate=\"44100\""));
    }

    #[test]
    fn multiple_video_tracks_become_representations() {
        let mut description = test_description();
        let mut second = description.tracks[0].clone();
        second.id = 1;
        second.width = Some(640);
        second.height = Some(360);
        description.tracks.push(second);
        let first_segments = vec![segment(1, 0)];
        let second_segments = vec![segment(1, 0)];
        let first = MpdVideoTrack {
            id: 0,
            track: &description.tracks[0],
            segments: &first_segments,
            codec: None,
            dimensions: None,
        };
        let second = MpdVideoTrack {
            id: 1,
            track: &description.tracks[1],
            segments: &second_segments,
            codec: None,
            dimensions: None,
        };
        let mpd = generate_mpd(
            &[first, second],
            &[],
            &description,
            &MpdConfig::default(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            Duration::ZERO,
        );
        assert_eq!(mpd.matches("<AdaptationSet").count(), 1);
        assert_eq!(mpd.matches("<Representation id=\"video-").count(), 2);
        assert!(mpd.contains("initialization=\"video-0-init.mp4\""));
        assert!(mpd.contains("initialization=\"video-1-init.mp4\""));
        assert!(mpd.contains("video-0-$Number$.m4s"));
        assert!(mpd.contains("video-1-$Number$.m4s"));
    }

    #[test]
    fn http_utc_timing_url_is_emitted() {
        let description = test_description();
        let segments = vec![segment(1, 0)];
        let video = video_rep(&description, &segments, None);
        let mut config = MpdConfig::default();
        config.utc_timing_url = Some("/api/v1/time".into());
        let mpd = generate_mpd(
            &[video],
            &[],
            &description,
            &config,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            Duration::ZERO,
        );
        assert!(mpd.contains("urn:mpeg:dash:utc:http-iso:2014"), "{mpd}");
        assert!(mpd.contains("value=\"/api/v1/time\""), "{mpd}");
    }

    #[test]
    fn estimates_frame_rate_from_trun() {
        let samples = 2u32;
        let tick = 3600u32;
        let trun_size = 20 + samples * 16;
        let mut data = Vec::new();
        data.extend_from_slice(&trun_size.to_be_bytes());
        data.extend_from_slice(b"trun");
        data.extend_from_slice(&0x000f01u32.to_be_bytes());
        data.extend_from_slice(&samples.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        for _ in 0..samples {
            data.extend_from_slice(&tick.to_be_bytes());
            data.extend_from_slice(&0u32.to_be_bytes());
            data.extend_from_slice(&0u32.to_be_bytes());
            data.extend_from_slice(&0i32.to_be_bytes());
        }
        let mut segment = segment(1, 0);
        segment.data = Bytes::from(data);
        assert_eq!(video_frame_rate(&[segment], 90_000), Some((25, 1)));
    }

    #[test]
    fn query_is_propagated_to_init_and_media() {
        let description = test_description();
        let video = vec![segment(7, 0)];
        let audio = vec![segment(7, 0)];
        let video_rep = video_rep(&description, &video, Some("avc1.640028"));
        let mpd = generate_mpd_q(
            &[video_rep],
            &audio,
            &description,
            &MpdConfig::default(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            Duration::ZERO,
            "token=abc",
        );
        assert!(mpd.contains("initialization=\"video-0-init.mp4?token=abc\""));
        assert!(mpd.contains("media=\"video-0-$Number$.m4s?token=abc\""));
        assert!(mpd.contains("initialization=\"audio-init.mp4?token=abc\""));
        assert!(mpd.contains("media=\"audio-$Number$.m4s?token=abc\""));
        assert!(mpd.contains("codecs=\"avc1.640028\""));
    }

    #[test]
    fn audio_only_mpd_uses_audio_mime() {
        let mut description = test_description();
        description
            .tracks
            .retain(|track| track.kind == MediaKind::Audio);
        let mpd = generate_mpd(
            &[],
            &[segment(1, 0)],
            &description,
            &MpdConfig::default(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            Duration::ZERO,
        );
        assert_eq!(mpd.matches("<AdaptationSet").count(), 1);
        assert!(mpd.contains("mimeType=\"audio/mp4\""));
        assert!(!mpd.contains(" video-init.mp4"));
        assert!(mpd.contains("audioSamplingRate=\"44100\""));
    }

    #[test]
    fn generates_static_mpd_for_vod() {
        let description = test_description();
        let video = vec![segment(0, 0), segment(1, 2)];
        let audio = vec![segment(0, 0), segment(1, 2)];
        let video_rep = video_rep(&description, &video, None);
        let mpd = generate_static_mpd(
            &[video_rep],
            &audio,
            &description,
            &MpdConfig::default(),
            Duration::from_secs(4),
            "token=abc",
        );
        assert!(mpd.contains("type=\"static\""));
        assert!(mpd.contains("mediaPresentationDuration=\"PT4.000S\""));
        assert!(mpd.contains("duration=\"PT4.000S\""));
        assert!(mpd.contains("initialization=\"video-0-init.mp4?token=abc\""));
        assert!(mpd.contains("initialization=\"audio-init.mp4?token=abc\""));
        assert!(mpd.contains("media=\"video-0-$Number$.m4s?token=abc\""));
        assert!(mpd.contains("media=\"audio-$Number$.m4s?token=abc\""));
        assert_eq!(mpd.matches("<AdaptationSet").count(), 2);
    }

    #[test]
    fn format_duration_works() {
        assert_eq!(format_duration(Duration::from_secs(2)), "PT2.000S");
        assert_eq!(format_duration(Duration::from_millis(1500)), "PT1.500S");
        assert_eq!(format_duration(Duration::from_secs(30)), "PT30.000S");
    }
}
