//! MPEG-DASH MPD (Media Presentation Description) XML generation.
//!
//! Generates dynamic (live) MPD documents using SegmentTemplate with $Number$
//! substitution, conforming to the DASH-IF IOP live profile.
use crate::hls::segmenter::Segment;
use crate::media::{MediaDescription, MediaKind};
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
    /// Timescale for SegmentTemplate (ticks per second).
    pub timescale: u32,
    /// Suggested presentation delay (live edge offset).
    pub suggested_presentation_delay: Option<Duration>,
}

impl MpdConfig {
    pub fn validate(&self) -> crate::Result<()> {
        if self.segment_duration.is_zero() {
            return Err(crate::Error::Limit("DASH MPD segment duration"));
        }
        if self.timescale == 0 {
            return Err(crate::Error::Limit("DASH MPD timescale"));
        }
        Ok(())
    }
}

impl Default for MpdConfig {
    fn default() -> Self {
        Self {
            min_buffer_time: Duration::from_secs(2),
            time_shift_buffer_depth: Duration::from_secs(30),
            segment_duration: Duration::from_secs(2),
            timescale: crate::media::clock::VIDEO_CLOCK_RATE,
            suggested_presentation_delay: None,
        }
    }
}

/// Generate a dynamic MPD XML document.
///
/// `availability_start` is the fixed wall-clock instant when the first segment
/// was produced (stream start). It MUST remain stable across MPD refreshes so
/// that clients can derive the current segment number from elapsed time.
///
/// Uses `SegmentTimeline` to explicitly enumerate available segments, which is
/// the most reliable approach for a sliding-window live server.
#[allow(clippy::too_many_arguments)]
pub fn generate_mpd(
    segments: &[Segment],
    description: &MediaDescription,
    config: &MpdConfig,
    asset_prefix: &str,
    availability_start: SystemTime,
    base_timestamp: Duration,
    codec_override: Option<&str>,
    video_dimensions: Option<(u16, u16)>,
) -> String {
    generate_mpd_q(
        segments,
        description,
        config,
        asset_prefix,
        availability_start,
        base_timestamp,
        codec_override,
        video_dimensions,
        "",
    )
}

/// Like [`generate_mpd`], with a query suffix appended to every embedded
/// segment URL (e.g. `?token=...` for token-protected deployments).
/// Append a query string to an asset base path (`/x/y` + `?a=b`).
fn query_suffix(_asset_prefix: &str, query: &str) -> String {
    if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    }
}

/// Query-suffixed variant of [`generate_mpd`] (e.g. `?token=...`).
#[allow(clippy::too_many_arguments)]
pub fn generate_mpd_q(
    segments: &[Segment],
    description: &MediaDescription,
    config: &MpdConfig,
    asset_prefix: &str,
    availability_start: SystemTime,
    base_timestamp: Duration,
    codec_override: Option<&str>,
    video_dimensions: Option<(u16, u16)>,
    query: &str,
) -> String {
    let mut xml = String::with_capacity(4096);

    // Fixed availability start time (when the stream went live). Before
    // the first segment exists the state is UNIX_EPOCH; emitting
    // 1970-01-01 would break player time math, so fall back to now.
    let effective_start = if availability_start == SystemTime::UNIX_EPOCH {
        SystemTime::now()
    } else {
        availability_start
    };
    let ast_duration = effective_start
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let availability_start_str = iso8601_from_duration(ast_duration);

    // publishTime = current wall-clock time (when this MPD was generated)
    let now_duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let publish_time = iso8601_from_duration(now_duration);

    // Suggested presentation delay
    let suggested_delay = config
        .suggested_presentation_delay
        .unwrap_or(config.segment_duration * 3);

    // Maximum segment duration
    let max_segment_duration = segments
        .iter()
        .map(|s| s.duration)
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

    // Group tracks by kind
    let has_video = description
        .tracks
        .iter()
        .any(|t| t.kind == MediaKind::Video);
    let has_audio = description
        .tracks
        .iter()
        .any(|t| t.kind == MediaKind::Audio);

    // Compute normalized segment timeline (timestamps relative to first-ever segment = AST)
    let base_ts = base_timestamp;

    // Video AdaptationSet
    if has_video {
        let video_track = description
            .tracks
            .iter()
            .find(|t| t.kind == MediaKind::Video)
            .unwrap();

        let start_number = segments.first().map(|s| s.sequence).unwrap_or(0);

        xml.push_str(&format!(
            "    <AdaptationSet mimeType=\"video/mp4\" \
             codecs=\"{}\" \
             segmentAlignment=\"true\" \
             startWithSAP=\"1\">\n",
            codec_override.unwrap_or_else(|| video_codec_string(video_track.codec)),
        ));

        xml.push_str(&format!(
            "      <SegmentTemplate timescale=\"{}\" \
             startNumber=\"{}\" \
             initialization=\"{}{}/init.mp4\" \
             media=\"{}{}/segment_$Number$.m4s\">\n",
            config.timescale,
            start_number,
            asset_prefix,
            query_suffix(asset_prefix, query),
            asset_prefix,
            query_suffix(asset_prefix, query),
        ));

        // SegmentTimeline: explicitly list available segments
        xml.push_str("        <SegmentTimeline>\n");
        for segment in segments {
            let relative_ts = segment.timestamp.saturating_sub(base_ts);
            let t_ticks = duration_to_ticks(relative_ts, config.timescale);
            let d_ticks = duration_to_ticks(segment.duration, config.timescale).max(1);
            xml.push_str(&format!(
                "          <S t=\"{}\" d=\"{}\"/>\n",
                t_ticks, d_ticks,
            ));
        }
        xml.push_str("        </SegmentTimeline>\n");
        xml.push_str("      </SegmentTemplate>\n");

        // Dynamic Representation attributes
        // Prefer dimensions from init segment parsing, fall back to track description
        let (width, height) = video_dimensions
            .map(|(w, h)| (w as u32, h as u32))
            .unwrap_or_else(|| {
                (
                    video_track.width.unwrap_or(1920),
                    video_track.height.unwrap_or(1080),
                )
            });
        let frame_rate = video_track
            .frame_rate
            .map(|(n, d)| format!("{n}/{d}"))
            .unwrap_or_else(|| "30".to_string());

        xml.push_str(&format!(
            "      <Representation id=\"video\" bandwidth=\"2000000\" \
             width=\"{}\" height=\"{}\" frameRate=\"{}\"/>\n",
            width, height, frame_rate,
        ));

        xml.push_str("    </AdaptationSet>\n");
    }

    // Audio AdaptationSet
    if has_audio {
        let audio_track = description
            .tracks
            .iter()
            .find(|t| t.kind == MediaKind::Audio)
            .unwrap();

        let start_number = segments.first().map(|s| s.sequence).unwrap_or(0);
        let audio_timescale = audio_track.clock_rate;

        xml.push_str(&format!(
            "    <AdaptationSet mimeType=\"audio/mp4\" \
             codecs=\"{}\" \
             segmentAlignment=\"true\" \
             startWithSAP=\"1\">\n",
            audio_codec_string(audio_track.codec),
        ));

        xml.push_str(&format!(
            "      <SegmentTemplate timescale=\"{}\" \
             startNumber=\"{}\" \
             initialization=\"{}/init.mp4\" \
             media=\"{}/segment_$Number$.m4s\">\n",
            audio_timescale, start_number, asset_prefix, asset_prefix,
        ));

        // SegmentTimeline for audio
        xml.push_str("        <SegmentTimeline>\n");
        for segment in segments {
            let relative_ts = segment.timestamp.saturating_sub(base_ts);
            let t_ticks = duration_to_ticks(relative_ts, audio_timescale);
            let d_ticks = duration_to_ticks(segment.duration, audio_timescale).max(1);
            xml.push_str(&format!(
                "          <S t=\"{}\" d=\"{}\"/>\n",
                t_ticks, d_ticks,
            ));
        }
        xml.push_str("        </SegmentTimeline>\n");
        xml.push_str("      </SegmentTemplate>\n");

        xml.push_str(&format!(
            "      <Representation id=\"audio\" bandwidth=\"128000\" \
             audioSamplingRate=\"{}\">\n",
            audio_track.clock_rate,
        ));

        let channels = audio_track.channels.unwrap_or(2);
        xml.push_str(&format!(
            "        <AudioChannelConfiguration \
             schemeIdUri=\"urn:mpeg:dash:23003:3:audio_channel_configuration:2011\" \
             value=\"{}\"/>\n",
            channels,
        ));
        xml.push_str("      </Representation>\n");
        xml.push_str("    </AdaptationSet>\n");
    }

    xml.push_str("  </Period>\n");
    xml.push_str("</MPD>\n");

    xml
}

/// Convert a Duration to ticks at the given timescale.
fn duration_to_ticks(d: Duration, timescale: u32) -> u64 {
    crate::media::clock::duration_to_ticks(d, timescale)
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

fn video_codec_string(codec: crate::media::Codec) -> &'static str {
    match codec {
        crate::media::Codec::H264 => "avc1.42E01E",
        // fMP4 sample entry is hvc1 (parameter sets in-band), so the codecs
        // string must match: hev1 would make Chrome/Safari reject the
        // stream as mislabeled.
        crate::media::Codec::H265 => "hvc1.1.6.L93.B0",
        _ => "avc1.42E01E",
    }
}

fn audio_codec_string(codec: crate::media::Codec) -> &'static str {
    match codec {
        crate::media::Codec::Aac => "mp4a.40.2",
        crate::media::Codec::Opus => "opus",
        // RFC 6381 audio object types for G.711.
        crate::media::Codec::Pcma => "alaw",
        crate::media::Codec::Pcmu => "ulaw",
        _ => "mp4a.40.2",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Codec, TrackDescription};
    use bytes::Bytes;

    fn test_description() -> MediaDescription {
        MediaDescription {
            session_name: "test".into(),
            tracks: vec![
                TrackDescription {
                    id: 0,
                    kind: MediaKind::Video,
                    codec: Codec::H264,
                    clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
                    channels: None,
                    payload_type: 96,
                    fmtp: None,
                    control: "trackID=0".into(),
                    width: Some(1280),
                    height: Some(720),
                    frame_rate: Some((30, 1)),
                    #[cfg(feature = "srtp")]
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
                    #[cfg(feature = "srtp")]
                    crypto: None,
                    direction: None,
                    ptime: None,
                    rtcp: None,
                    bandwidth_as: None,
                    rtx: None,
                },
            ],
            #[cfg(feature = "multicast")]
            connection: None,
        }
    }

    #[test]
    fn generates_valid_mpd() {
        let segments = vec![
            Segment {
                sequence: 1,
                duration: Duration::from_secs(2),
                init: Bytes::new(),
                data: Bytes::new(),
                starts_with_keyframe: true,
                discontinuity: false,
                discontinuity_sequence: None,
                timestamp: Duration::ZERO,
            },
            Segment {
                sequence: 2,
                duration: Duration::from_secs(2),
                init: Bytes::new(),
                data: Bytes::new(),
                starts_with_keyframe: true,
                discontinuity: false,
                discontinuity_sequence: None,
                timestamp: Duration::from_secs(2),
            },
        ];

        let desc = test_description();
        let config = MpdConfig::default();
        let mpd = generate_mpd(
            &segments,
            &desc,
            &config,
            "/live/stream",
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            Duration::ZERO,
            None,
            None,
        );

        assert!(mpd.contains("<?xml version=\"1.0\""));
        assert!(mpd.contains("type=\"dynamic\""));
        assert!(mpd.contains("urn:mpeg:dash:profile:isoff-live:2011"));
        assert!(mpd.contains("startNumber=\"1\""));
        assert!(mpd.contains("/live/stream/init.mp4"));
        assert!(mpd.contains("/live/stream/segment_$Number$.m4s"));
        assert!(mpd.contains("avc1.42E01E"));
        assert!(mpd.contains("mp4a.40.2"));
        // SegmentTimeline
        assert!(mpd.contains("<SegmentTimeline>"));
        assert!(mpd.contains("<S t=\"0\" d=\"180000\"/>"));
        assert!(mpd.contains("<S t=\"180000\" d=\"180000\"/>"));
        // Dynamic attributes
        assert!(mpd.contains("suggestedPresentationDelay="));
        assert!(mpd.contains("maximumSegmentDuration="));
        assert!(mpd.contains("publishTime="));
        // Dynamic Representation from TrackDescription
        assert!(mpd.contains("width=\"1280\""));
        assert!(mpd.contains("height=\"720\""));
        assert!(mpd.contains("frameRate=\"30/1\""));
    }

    #[test]
    fn format_duration_works() {
        assert_eq!(format_duration(Duration::from_secs(2)), "PT2.000S");
        assert_eq!(format_duration(Duration::from_millis(1500)), "PT1.500S");
        assert_eq!(format_duration(Duration::from_secs(30)), "PT30.000S");
    }
}
