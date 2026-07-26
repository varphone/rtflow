use super::segmenter::Segment;
use std::time::Duration;

/// Generate a live HLS M3U8 playlist.
pub fn write_live_playlist(segments: &[Segment], target_duration: Duration) -> String {
    if segments.is_empty() {
        return String::new();
    }

    let target_secs = target_duration.as_secs().max(1);
    let mut playlist = String::with_capacity(512);

    playlist.push_str("#EXTM3U\n");
    playlist.push_str("#EXT-X-VERSION:7\n");
    playlist.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_secs));

    // Media sequence is the sequence number of the first segment
    if let Some(first) = segments.first() {
        playlist.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", first.sequence));
    }

    playlist.push_str("#EXT-X-DISCONTINUITY-SEQUENCE:0\n");

    for segment in segments {
        let duration = segment.duration.as_secs_f64();
        playlist.push_str(&format!("#EXTINF:{:.3},\n", duration));
        playlist.push_str(&format!("segment-{}.m4s\n", segment.sequence));
    }

    playlist
}

/// Generate a VOD HLS M3U8 playlist (with EXT-X-ENDLIST).
pub fn write_vod_playlist(segments: &[Segment]) -> String {
    if segments.is_empty() {
        return String::new();
    }

    let max_duration = segments
        .iter()
        .map(|s| s.duration.as_secs())
        .max()
        .unwrap_or(1)
        .max(1);

    let mut playlist = String::with_capacity(512);

    playlist.push_str("#EXTM3U\n");
    playlist.push_str("#EXT-X-VERSION:7\n");
    playlist.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", max_duration));
    playlist.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    playlist.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");

    for segment in segments {
        let duration = segment.duration.as_secs_f64();
        playlist.push_str(&format!("#EXTINF:{:.3},\n", duration));
        playlist.push_str(&format!("segment-{}.m4s\n", segment.sequence));
    }

    playlist.push_str("#EXT-X-ENDLIST\n");
    playlist
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn make_segment(sequence: u64, duration_ms: u64) -> Segment {
        Segment {
            sequence,
            duration: Duration::from_millis(duration_ms),
            init: Bytes::new(),
            data: Bytes::new(),
            starts_with_keyframe: true,
            timestamp: Duration::from_millis(sequence * 2000),
        }
    }

    #[test]
    fn live_playlist_format() {
        let segments = vec![
            make_segment(0, 2000),
            make_segment(1, 2500),
            make_segment(2, 2000),
        ];
        let playlist = write_live_playlist(&segments, Duration::from_secs(3));

        assert!(playlist.starts_with("#EXTM3U"));
        assert!(playlist.contains("#EXT-X-VERSION:7"));
        assert!(playlist.contains("#EXT-X-TARGETDURATION:3"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(playlist.contains("#EXTINF:2.000,"));
        assert!(playlist.contains("#EXTINF:2.500,"));
        assert!(playlist.contains("segment-0.m4s"));
        assert!(playlist.contains("segment-2.m4s"));
        assert!(!playlist.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn vod_playlist_format() {
        let segments = vec![make_segment(0, 2000), make_segment(1, 2000)];
        let playlist = write_vod_playlist(&segments);

        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(playlist.contains("#EXT-X-ENDLIST"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
    }

    #[test]
    fn empty_playlist() {
        let playlist = write_live_playlist(&[], Duration::from_secs(2));
        assert!(playlist.is_empty());
    }

    #[test]
    fn live_playlist_sliding_window() {
        // Simulate sliding window with high sequence numbers
        let segments = vec![
            make_segment(100, 2000),
            make_segment(101, 2000),
            make_segment(102, 2000),
        ];
        let playlist = write_live_playlist(&segments, Duration::from_secs(2));

        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:100"));
        assert!(playlist.contains("segment-100.m4s"));
        assert!(playlist.contains("segment-102.m4s"));
    }
}
