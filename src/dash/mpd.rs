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

impl Default for MpdConfig {
    fn default() -> Self {
        Self {
            min_buffer_time: Duration::from_secs(2),
            time_shift_buffer_depth: Duration::from_secs(30),
            segment_duration: Duration::from_secs(2),
            timescale: 90000,
            suggested_presentation_delay: None,
        }
    }
}

/// Generate a dynamic MPD XML document.
pub fn generate_mpd(
    segments: &[Segment],
    description: &MediaDescription,
    config: &MpdConfig,
    asset_prefix: &str,
) -> String {
    let mut xml = String::with_capacity(4096);

    // Calculate availability start time (actual server start)
    let availability_start = iso8601_now();

    // Calculate publish time (latest segment time)
    let publish_time = segments
        .last()
        .map(|s| iso8601_from_duration(s.timestamp))
        .unwrap_or_else(|| availability_start.clone());

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
         availabilityStartTime=\"{}\" \
         publishTime=\"{}\">\n",
        format_duration(config.min_buffer_time),
        format_duration(config.time_shift_buffer_depth),
        format_duration(suggested_delay),
        format_duration(max_segment_duration),
        availability_start,
        publish_time,
    ));

    xml.push_str("  <Period id=\"0\">\n");

    // Group tracks by kind
    let has_video = description
        .tracks
        .iter()
        .any(|t| t.kind == MediaKind::Video);
    let has_audio = description
        .tracks
        .iter()
        .any(|t| t.kind == MediaKind::Audio);

    // Video AdaptationSet
    if has_video {
        let video_track = description
            .tracks
            .iter()
            .find(|t| t.kind == MediaKind::Video)
            .unwrap();

        let start_number = segments.first().map(|s| s.sequence).unwrap_or(1);
        let segment_duration_ticks =
            (config.segment_duration.as_secs_f64() * config.timescale as f64) as u64;

        xml.push_str(&format!(
            "    <AdaptationSet mimeType=\"video/mp4\" \
             codecs=\"{}\" \
             segmentAlignment=\"true\" \
             startWithSAP=\"1\">\n",
            video_codec_string(video_track.codec),
        ));

        xml.push_str(&format!(
            "      <SegmentTemplate timescale=\"{}\" \
             duration=\"{}\" \
             startNumber=\"{}\" \
             initialization=\"{}/init.mp4\" \
             media=\"{}/segment_$Number$.m4s\"/>\n",
            config.timescale,
            segment_duration_ticks,
            start_number,
            asset_prefix,
            asset_prefix,
        ));

        // Dynamic Representation attributes
        let width = video_track.width.unwrap_or(1920);
        let height = video_track.height.unwrap_or(1080);
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

        let start_number = segments.first().map(|s| s.sequence).unwrap_or(1);
        let audio_timescale = audio_track.clock_rate;
        let segment_duration_ticks =
            (config.segment_duration.as_secs_f64() * audio_timescale as f64) as u64;

        xml.push_str(&format!(
            "    <AdaptationSet mimeType=\"audio/mp4\" \
             codecs=\"{}\" \
             segmentAlignment=\"true\" \
             startWithSAP=\"1\">\n",
            audio_codec_string(audio_track.codec),
        ));

        xml.push_str(&format!(
            "      <SegmentTemplate timescale=\"{}\" \
             duration=\"{}\" \
             startNumber=\"{}\" \
             initialization=\"{}/init.mp4\" \
             media=\"{}/segment_$Number$.m4s\"/>\n",
            audio_timescale,
            segment_duration_ticks,
            start_number,
            asset_prefix,
            asset_prefix,
        ));

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

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let frac = d.subsec_millis();
    format!("PT{}.{:03}S", secs, frac)
}

/// Format current time as ISO 8601 UTC.
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
        crate::media::Codec::H265 => "hev1.1.6.L93.B0",
        _ => "avc1.42E01E",
    }
}

fn audio_codec_string(codec: crate::media::Codec) -> &'static str {
    match codec {
        crate::media::Codec::Aac => "mp4a.40.2",
        crate::media::Codec::Opus => "opus",
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
                    clock_rate: 90000,
                    channels: None,
                    payload_type: 96,
                    fmtp: None,
                    control: "trackID=0".into(),
                    width: Some(1280),
                    height: Some(720),
                    frame_rate: Some((30, 1)),
                    #[cfg(feature = "srtp")]
                    crypto: None,
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
                timestamp: Duration::ZERO,
            },
            Segment {
                sequence: 2,
                duration: Duration::from_secs(2),
                init: Bytes::new(),
                data: Bytes::new(),
                starts_with_keyframe: true,
                discontinuity: false,
                timestamp: Duration::from_secs(2),
            },
        ];

        let desc = test_description();
        let config = MpdConfig::default();
        let mpd = generate_mpd(&segments, &desc, &config, "/live/stream");

        assert!(mpd.contains("<?xml version=\"1.0\""));
        assert!(mpd.contains("type=\"dynamic\""));
        assert!(mpd.contains("urn:mpeg:dash:profile:isoff-live:2011"));
        assert!(mpd.contains("startNumber=\"1\""));
        assert!(mpd.contains("/live/stream/init.mp4"));
        assert!(mpd.contains("/live/stream/segment_$Number$.m4s"));
        assert!(mpd.contains("avc1.42E01E"));
        assert!(mpd.contains("mp4a.40.2"));
        // New dynamic attributes
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
