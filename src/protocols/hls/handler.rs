use super::playlist;
use super::segment_ring::SegmentRing;
use super::segmenter::Segment;
use crate::{
    Result,
    http::{HttpRequest, Method, ResponseWriter},
};
use std::time::Duration;
use tokio::io::AsyncWrite;

/// Serialize a parsed query map back into `k=v&k2=v2` form (used to
/// propagate auth tokens from the playlist request into embedded URIs).
fn serialize_query(query: &std::collections::HashMap<String, String>) -> String {
    let mut parts: Vec<String> = query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    parts.sort();
    parts.join("&")
}

/// Weak entity tag for a live playlist: a fingerprint of the current
/// segment window (sequence numbers, durations, discontinuity flags) plus
/// the embedded-URI query. Unchanged windows produce the same ETag, so
/// hls.js-style players can revalidate with `If-None-Match` and get a 304
/// instead of a full playlist download every poll interval.
fn playlist_etag(segments: &[Segment], query: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut update = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    update(b"rtflow-hls-playlist");
    for segment in segments {
        update(&segment.sequence.to_be_bytes());
        update(&segment.duration.as_nanos().to_be_bytes());
        update(&[segment.discontinuity as u8]);
        update(&segment.discontinuity_sequence.unwrap_or(0).to_be_bytes());
        update(&segment.timestamp.as_nanos().to_be_bytes());
    }
    update(query.as_bytes());
    format!("\"{:016x}\"", hash)
}

/// Serve an HLS M3U8 playlist.
pub async fn serve_playlist<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
    is_live: bool,
    target_duration: Duration,
    asset_prefix: &str,
    program_date_time: Option<std::time::SystemTime>,
) -> Result<ResponseWriter<W>> {
    let segments = ring.snapshot();

    // Propagate the request query (e.g. ?token=...) into every embedded
    // URI so token-protected deployments do not break on segment fetches.
    let query = serialize_query(&req.query);
    let etag = playlist_etag(&segments, &query);

    // Cache revalidation: the window (and therefore the playlist body)
    // is unchanged, so answer 304 without regenerating the body.
    if req
        .header("if-none-match")
        .is_some_and(|value| value.trim() == etag)
    {
        writer
            .write_headers(
                304,
                &[
                    ("ETag", etag.as_str()),
                    ("Cache-Control", "no-cache"),
                    ("Connection", "keep-alive"),
                ],
            )
            .await?;
        return Ok(writer);
    }

    let body = if is_live {
        playlist::write_live_playlist_at_q_pdt(
            &segments,
            target_duration,
            asset_prefix,
            &query,
            program_date_time,
        )
    } else {
        playlist::write_vod_playlist_at_q(&segments, asset_prefix, &query)
    };

    let content_length = body.len().to_string();
    writer
        .write_headers(
            200,
            &[
                ("Content-Type", "application/vnd.apple.mpegurl"),
                ("Content-Length", content_length.as_str()),
                ("ETag", etag.as_str()),
                ("Cache-Control", "no-cache"),
                ("Access-Control-Allow-Origin", "*"),
                ("Connection", "keep-alive"),
            ],
        )
        .await?;
    writer.write_raw(body.as_bytes()).await?;
    Ok(writer)
}

fn byte_range(header: Option<&str>, length: usize) -> Option<(usize, usize)> {
    let value = header?.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<usize>().ok()?.min(length);
        let start = length.checked_sub(suffix)?;
        return (suffix > 0).then_some((start, length - 1));
    }
    let start = start.parse::<usize>().ok()?;
    let end = if end.is_empty() {
        length.checked_sub(1)?
    } else {
        end.parse::<usize>().ok()?.min(length.checked_sub(1)?)
    };
    (start <= end && start < length).then_some((start, end))
}

/// Serve an HLS media segment (.m4s / .ts), optionally a byte range.
pub async fn serve_segment<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
    sequence: u64,
) -> Result<ResponseWriter<W>> {
    let segment = match ring.get(sequence) {
        Some(s) => s,
        None => {
            writer
                .write_body(404, "text/plain", b"Segment not found")
                .await?;
            return Ok(writer);
        }
    };

    let content_type = if segment.format == super::segmenter::SegmentFormat::MpegTs {
        "video/mp2t"
    } else {
        "video/iso.segment"
    };
    let (status, start, end) = match byte_range(req.header("range"), segment.data.len()) {
        Some((start, end)) => (206, start, end + 1),
        None => (200, 0, segment.data.len()),
    };
    let content_length = (end - start).to_string();
    let content_range = format!("bytes {start}-{}/{len}", end - 1, len = segment.data.len());
    let mut headers = vec![
        ("Content-Type", content_type),
        ("Content-Length", content_length.as_str()),
        ("Cache-Control", "no-store"),
        ("Access-Control-Allow-Origin", "*"),
        ("Accept-Ranges", "bytes"),
        ("Connection", "keep-alive"),
    ];
    if status == 206 {
        headers.push(("Content-Range", content_range.as_str()));
    }
    writer.write_headers(status, &headers).await?;
    if req.method != Method::Head {
        writer.write_raw(&segment.data[start..end]).await?;
        writer.flush().await?;
    }
    Ok(writer)
}

/// Serve the HLS init segment.
pub async fn serve_init_segment<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
) -> Result<ResponseWriter<W>> {
    let init = match ring.latest_init().filter(|init| !init.is_empty()) {
        Some(i) => i,
        None => {
            writer
                .write_body(404, "text/plain", b"No init segment available")
                .await?;
            return Ok(writer);
        }
    };

    let content_length = init.len().to_string();
    let headers = [
        ("Content-Type", "video/mp4"),
        ("Content-Length", content_length.as_str()),
        ("Cache-Control", "no-store"),
        ("Access-Control-Allow-Origin", "*"),
        ("Accept-Ranges", "bytes"),
        ("Connection", "keep-alive"),
    ];
    writer.write_headers(200, &headers).await?;
    if req.method != Method::Head {
        writer.write_raw(&init).await?;
    }
    writer.flush().await?;
    Ok(writer)
}

#[cfg(test)]
mod tests {
    use super::super::segmenter::Segment;
    use super::*;
    use crate::http::HttpRequest;
    use crate::http::Method;
    use crate::http::ResponseWriter;
    use bytes::Bytes;
    use std::collections::HashMap;

    fn segment(seq: u64) -> Segment {
        Segment {
            sequence: seq,
            duration: Duration::from_millis(2000),
            init: Bytes::new(),
            data: Bytes::new(),
            starts_with_keyframe: true,
            discontinuity: false,
            discontinuity_sequence: None,
            timestamp: Duration::from_millis(seq * 2000),
            format: crate::protocols::hls::segmenter::SegmentFormat::Fmp4,
        }
    }

    fn request(headers: Vec<(String, String)>) -> HttpRequest {
        HttpRequest {
            method: Method::Get,
            path: "/live/stream.m3u8".into(),
            query: HashMap::new(),
            headers,
            body: Bytes::new(),
            path_params: Vec::new(),
        }
    }

    #[test]
    fn etag_stable_for_unchanged_window() {
        let a = vec![segment(10), segment(11)];
        let b = vec![segment(10), segment(11)];
        assert_eq!(playlist_etag(&a, ""), playlist_etag(&b, ""));
        // The embedded-URI query participates in the fingerprint.
        assert_ne!(playlist_etag(&a, "token=x"), playlist_etag(&a, ""));
        // A slid window (new segment arrived) changes the ETag.
        let c = vec![segment(11), segment(12)];
        assert_ne!(playlist_etag(&a, ""), playlist_etag(&c, ""));
    }

    #[tokio::test]
    async fn playlist_serves_etag_and_304_on_revalidation() {
        let ring = SegmentRing::new(3);
        ring.push(segment(0));
        ring.push(segment(1));

        // First request: 200 with ETag.
        let writer = ResponseWriter::new(Vec::new());
        let writer = serve_playlist(
            request(Vec::new()),
            writer,
            &ring,
            true,
            Duration::from_secs(2),
            "/live/stream",
            None,
        )
        .await
        .unwrap();
        let out = writer.into_inner();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 200"), "first request: {text}");
        assert!(
            text.to_ascii_lowercase().contains("connection: keep-alive"),
            "playlist response must keep the connection alive: {text}"
        );
        let etag = text
            .lines()
            .find(|line| line.to_ascii_lowercase().starts_with("etag:"))
            .expect("ETag header present")
            .split_once(':')
            .unwrap()
            .1
            .trim()
            .to_string();

        // Revalidation with the same ETag: 304, no playlist body.
        let writer = ResponseWriter::new(Vec::new());
        let writer = serve_playlist(
            request(vec![("If-None-Match".into(), etag)]),
            writer,
            &ring,
            true,
            Duration::from_secs(2),
            "/live/stream",
            None,
        )
        .await
        .unwrap();
        let out = writer.into_inner();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 304"), "revalidation: {text}");
        assert!(!text.contains("#EXTM3U"), "304 must not carry a body");

        // A stale ETag still gets the full playlist.
        let writer = ResponseWriter::new(Vec::new());
        let writer = serve_playlist(
            request(vec![("If-None-Match".into(), "\"deadbeef\"".into())]),
            writer,
            &ring,
            true,
            Duration::from_secs(2),
            "/live/stream",
            None,
        )
        .await
        .unwrap();
        let out = writer.into_inner();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 200"), "stale etag: {text}");
        assert!(text.contains("#EXTM3U"));
    }
    #[tokio::test]
    async fn segment_serves_byte_range() {
        let ring = SegmentRing::new(3);
        let mut seg = segment(0);
        seg.data = Bytes::from_static(b"0123456789");
        ring.push(seg);
        let writer = ResponseWriter::new(Vec::new());
        let writer = serve_segment(
            request(vec![("Range".into(), "bytes=2-4".into())]),
            writer,
            &ring,
            0,
        )
        .await
        .unwrap();
        let out = writer.into_inner();
        let header_end = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert!(out.starts_with(b"HTTP/1.1 206 Partial Content\r\n"));
        assert!(
            String::from_utf8_lossy(&out[..header_end])
                .to_ascii_lowercase()
                .contains("content-range: bytes 2-4/10")
        );
        assert_eq!(&out[header_end..], b"234");
    }
}
