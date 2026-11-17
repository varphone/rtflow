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
        }
    }
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
    video: &[Segment],
    audio: &[Segment],
    description: &MediaDescription,
    config: &MpdConfig,
    availability_start: SystemTime,
    base_timestamp: Duration,
    codec_override: Option<&str>,
    video_dimensions: Option<(u16, u16)>,
) -> String {
    generate_mpd_q(
        video,
        audio,
        description,
        config,
        availability_start,
        base_timestamp,
        codec_override,
        video_dimensions,
        "",
    )
}

/// Query-suffixed variant of [`generate_mpd`] (e.g. `?token=...`).
#[allow(clippy::too_many_arguments)]
pub fn generate_mpd_q(
    video: &[Segment],
    audio: &[Segment],
    description: &MediaDescription,
    config: &MpdConfig,
    availability_start: SystemTime,
    base_timestamp: Duration,
    codec_override: Option<&str>,
    video_dimensions: Option<(u16, u16)>,
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

    if let Some(track) = description
        .tracks
        .iter()
        .find(|track| track.kind == MediaKind::Video)
        && !video.is_empty()
    {
        push_video_adaptation_set(
            &mut xml,
            video,
            track,
            config,
            base_timestamp,
            codec_override,
            video_dimensions,
            &suffix,
        );
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
    xml.push_str(&format!(
        "  <UTCTiming schemeIdUri=\"urn:mpeg:dash:utc:direct:2014\" value=\"{publish_time}\"/>\n"
    ));
    xml.push_str("</MPD>\n");
    xml
}

#[allow(clippy::too_many_arguments)]
fn push_video_adaptation_set(
    xml: &mut String,
    segments: &[Segment],
    track: &rtflow_media::TrackDescription,
    config: &MpdConfig,
    base_timestamp: Duration,
    codec_override: Option<&str>,
    video_dimensions: Option<(u16, u16)>,
    suffix: &str,
) {
    let timescale = track.clock_rate.max(1);
    let start_number = segments
        .first()
        .map(|segment| segment.sequence)
        .unwrap_or(0);
    let codecs = codec_override.unwrap_or_else(|| video_codec_string(track.codec));
    let (width, height) = video_dimensions
        .map(|(width, height)| (width as u32, height as u32))
        .unwrap_or_else(|| (track.width.unwrap_or(1920), track.height.unwrap_or(1080)));
    let frame_rate = track
        .frame_rate
        .map(|(numerator, denominator)| format!("{numerator}/{denominator}"))
        .unwrap_or_else(|| "30".to_string());
    let bandwidth = estimated_bandwidth(segments, true);

    xml.push_str(
        "    <AdaptationSet mimeType=\"video/mp4\" segmentAlignment=\"true\" startWithSAP=\"1\">\n",
    );
    xml.push_str(&format!(
        "      <Representation id=\"video\" bandwidth=\"{bandwidth}\" codecs=\"{codecs}\" width=\"{width}\" height=\"{height}\" frameRate=\"{frame_rate}\">\n"
    ));
    xml.push_str(&format!(
        "        <SegmentTemplate timescale=\"{timescale}\" startNumber=\"{start_number}\" initialization=\"video-init.mp4{suffix}\" media=\"video-$Number$.m4s{suffix}\">\n"
    ));
    push_timeline(xml, segments, base_timestamp, timescale);
    xml.push_str("        </SegmentTemplate>\n");
    xml.push_str("      </Representation>\n");
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

    #[test]
    fn generates_per_track_mpd() {
        let video = vec![segment(1, 0), segment(2, 2)];
        let audio = vec![segment(1, 0), segment(2, 2)];
        let mpd = generate_mpd(
            &video,
            &audio,
            &test_description(),
            &MpdConfig::default(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            Duration::ZERO,
            None,
            None,
        );

        assert!(mpd.contains("<?xml version=\"1.0\""));
        assert!(mpd.contains("type=\"dynamic\""));
        assert!(mpd.contains("urn:mpeg:dash:profile:isoff-live:2011"));
        assert_eq!(mpd.matches("<AdaptationSet").count(), 2);
        assert_eq!(mpd.matches("<SegmentTemplate").count(), 2);
        assert!(mpd.contains("initialization=\"video-init.mp4\""));
        assert!(mpd.contains("media=\"video-$Number$.m4s\""));
        assert!(mpd.contains("initialization=\"audio-init.mp4\""));
        assert!(mpd.contains("media=\"audio-$Number$.m4s\""));
        assert!(mpd.contains("codecs=\"avc1.42E01E\""));
        assert!(mpd.contains("codecs=\"mp4a.40.2\""));
        assert!(mpd.contains("<Representation id=\"video\""));
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
    fn query_is_propagated_to_init_and_media() {
        let video = vec![segment(7, 0)];
        let audio = vec![segment(7, 0)];
        let mpd = generate_mpd_q(
            &video,
            &audio,
            &test_description(),
            &MpdConfig::default(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            Duration::ZERO,
            Some("avc1.640028"),
            None,
            "token=abc",
        );
        assert!(mpd.contains("initialization=\"video-init.mp4?token=abc\""));
        assert!(mpd.contains("media=\"video-$Number$.m4s?token=abc\""));
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
            None,
            None,
        );
        assert_eq!(mpd.matches("<AdaptationSet").count(), 1);
        assert!(mpd.contains("mimeType=\"audio/mp4\""));
        assert!(!mpd.contains(" video-init.mp4"));
        assert!(mpd.contains("audioSamplingRate=\"44100\""));
    }

    #[test]
    fn format_duration_works() {
        assert_eq!(format_duration(Duration::from_secs(2)), "PT2.000S");
        assert_eq!(format_duration(Duration::from_millis(1500)), "PT1.500S");
        assert_eq!(format_duration(Duration::from_secs(30)), "PT30.000S");
    }
}
