use super::segmenter::Segment;
use std::time::Duration;

fn asset_uri(asset_prefix: &str, asset: &str, query: &str) -> String {
    let prefix = asset_prefix.trim_end_matches('/');
    let base = if prefix.is_empty() {
        asset.to_owned()
    } else {
        format!("{prefix}/{asset}")
    };
    if query.is_empty() {
        base
    } else {
        format!("{base}?{query}")
    }
}

/// Generate a live HLS M3U8 playlist.
pub fn write_live_playlist(segments: &[Segment], target_duration: Duration) -> String {
    write_live_playlist_at(segments, target_duration, "")
}

/// Generate a live playlist whose init and media URIs are rooted at `asset_prefix`.
pub fn write_live_playlist_at(
    segments: &[Segment],
    target_duration: Duration,
    asset_prefix: &str,
) -> String {
    write_live_playlist_at_q(segments, target_duration, asset_prefix, "")
}

/// Like [`write_live_playlist_at`], with a query suffix appended to every
/// embedded URI (e.g. `?token=...` for token-protected deployments).
pub fn write_live_playlist_at_q(
    segments: &[Segment],
    target_duration: Duration,
    asset_prefix: &str,
    query: &str,
) -> String {
    if segments.is_empty() {
        // A valid but empty playlist: players poll it until segments
        // appear (an empty 200 body would be rejected by strict parsers).
        return String::from("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n");
    }

    let target_secs = segments
        .iter()
        .map(|segment| segment.duration.as_secs_f64().ceil() as u64)
        .max()
        .unwrap_or(1)
        .max(target_duration.as_secs_f64().ceil() as u64)
        .max(1);
    let mut playlist = String::with_capacity(512);

    playlist.push_str("#EXTM3U\n");
    playlist.push_str("#EXT-X-VERSION:7\n");
    playlist.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_secs));

    // Media sequence is the sequence number of the first segment
    if let Some(first) = segments.first() {
        playlist.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", first.sequence));
    }

    let discontinuity_sequence = segments
        .iter()
        .find_map(|segment| segment.discontinuity_sequence)
        .unwrap_or(0);
    playlist.push_str(&format!(
        "#EXT-X-DISCONTINUITY-SEQUENCE:{discontinuity_sequence}\n"
    ));
    playlist.push_str(&format!(
        "#EXT-X-MAP:URI=\"{}\"\n",
        asset_uri(asset_prefix, "init.m4s", query)
    ));

    for segment in segments {
        if segment.discontinuity {
            playlist.push_str("#EXT-X-DISCONTINUITY\n");
        }
        let duration = segment.duration.as_secs_f64();
        playlist.push_str(&format!("#EXTINF:{:.3},\n", duration));
        playlist.push_str(&format!(
            "{}\n",
            asset_uri(asset_prefix, &format!("{}.m4s", segment.sequence), query)
        ));
    }

    playlist
}

/// Generate a VOD HLS M3U8 playlist (with EXT-X-ENDLIST).
pub fn write_vod_playlist(segments: &[Segment]) -> String {
    write_vod_playlist_at(segments, "")
}

/// Generate a VOD playlist whose init and media URIs are rooted at `asset_prefix`.
pub fn write_vod_playlist_at(segments: &[Segment], asset_prefix: &str) -> String {
    write_vod_playlist_at_q(segments, asset_prefix, "")
}

/// Like [`write_vod_playlist_at`], with a query suffix on every URI.
pub fn write_vod_playlist_at_q(segments: &[Segment], asset_prefix: &str, query: &str) -> String {
    if segments.is_empty() {
        return String::new();
    }

    let max_duration = segments
        .iter()
        .map(|s| s.duration.as_secs_f64().ceil() as u64)
        .max()
        .unwrap_or(1)
        .max(1);

    let mut playlist = String::with_capacity(512);

    playlist.push_str("#EXTM3U\n");
    playlist.push_str("#EXT-X-VERSION:7\n");
    playlist.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", max_duration));
    playlist.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    playlist.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    playlist.push_str(&format!(
        "#EXT-X-MAP:URI=\"{}\"\n",
        asset_uri(asset_prefix, "init.m4s", query)
    ));

    for segment in segments {
        if segment.discontinuity {
            playlist.push_str("#EXT-X-DISCONTINUITY\n");
        }
        let duration = segment.duration.as_secs_f64();
        playlist.push_str(&format!("#EXTINF:{:.3},\n", duration));
        playlist.push_str(&format!(
            "{}\n",
            asset_uri(asset_prefix, &format!("{}.m4s", segment.sequence), query)
        ));
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
            discontinuity: false,
            discontinuity_sequence: None,
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
        assert!(playlist.contains("#EXT-X-MAP:URI=\"init.m4s\""));
        assert!(playlist.contains("#EXTINF:2.000,"));
        assert!(playlist.contains("#EXTINF:2.500,"));
        assert!(playlist.contains("0.m4s"));
        assert!(playlist.contains("2.m4s"));
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
        // An empty window still yields a valid (header-only) playlist so
        // strict parsers do not reject the response.
        let playlist = write_live_playlist(&[], Duration::from_secs(2));
        assert!(playlist.starts_with("#EXTM3U"));
        assert!(!playlist.is_empty());
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
        assert!(playlist.contains("100.m4s"));
        assert!(playlist.contains("102.m4s"));
    }
}
