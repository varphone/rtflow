//! MPEG-DASH MPD (Media Presentation Description) XML generation.
//!
//! Generates dynamic (live) MPD documents using SegmentTemplate with $Number$
//! substitution, conforming to the DASH-IF IOP live profile.
use crate::hls::segmenter::Segment;
use crate::media::{MediaDescription, MediaKind};
use std::time::Duration;

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
}

impl Default for MpdConfig {
    fn default() -> Self {
        Self {
            min_buffer_time: Duration::from_secs(2),
            time_shift_buffer_depth: Duration::from_secs(30),
            segment_duration: Duration::from_secs(2),
            timescale: 90000,
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

    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" \
         type=\"dynamic\" \
         profiles=\"urn:mpeg:dash:profile:isoff-live:2011\" \
         minBufferTime=\"{}\" \
         timeShiftBufferDepth=\"{}\" \
         availabilityStartTime=\"1970-01-01T00:00:00Z\">\n",
        format_duration(config.min_buffer_time),
        format_duration(config.time_shift_buffer_depth),
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

        xml.push_str(&format!(
            "      <Representation id=\"video\" bandwidth=\"2000000\" \
             width=\"1920\" height=\"1080\" frameRate=\"30\"/>\n",
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
    }

    #[test]
    fn format_duration_works() {
        assert_eq!(format_duration(Duration::from_secs(2)), "PT2.000S");
        assert_eq!(format_duration(Duration::from_millis(1500)), "PT1.500S");
        assert_eq!(format_duration(Duration::from_secs(30)), "PT30.000S");
    }
}
