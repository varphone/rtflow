use super::playlist;
use super::segment_ring::SegmentRing;
use crate::{
    Result,
    http::{HttpRequest, ResponseWriter},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWrite;

/// Serve an HLS M3U8 playlist.
pub async fn serve_playlist<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
    is_live: bool,
    target_duration: Duration,
) -> Result<ResponseWriter<W>> {
    let segments = ring.snapshot();

    let body = if is_live {
        playlist::write_live_playlist(&segments, target_duration)
    } else {
        playlist::write_vod_playlist(&segments)
    };

    writer.write_body(200, "application/vnd.apple.mpegurl", body.as_bytes()).await?;
    Ok(writer)
}

/// Serve an HLS media segment (.m4s).
pub async fn serve_segment<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
    sequence: u64,
) -> Result<ResponseWriter<W>> {
    let segment = match ring.get(sequence) {
        Some(s) => s,
        None => {
            writer.write_body(404, "text/plain", b"Segment not found").await?;
            return Ok(writer);
        }
    };

    let headers = vec![
        ("Cache-Control", "public, max-age=3600"),
        ("Access-Control-Allow-Origin", "*"),
    ];

    writer.write_headers(200, &headers).await?;
    // Write init segment + media segment
    let mut body = Vec::with_capacity(segment.init.len() + segment.data.len());
    body.extend_from_slice(&segment.init);
    body.extend_from_slice(&segment.data);
    writer.write_chunk(&body).await?;
    writer.flush().await?;
    Ok(writer)
}

/// Serve the HLS init segment.
pub async fn serve_init_segment<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
) -> Result<ResponseWriter<W>> {
    let init = match ring.latest_init() {
        Some(i) => i,
        None => {
            writer.write_body(404, "text/plain", b"No init segment available").await?;
            return Ok(writer);
        }
    };

    let headers = vec![
        ("Cache-Control", "public, max-age=3600"),
        ("Access-Control-Allow-Origin", "*"),
    ];
    writer.write_headers(200, &headers).await?;
    writer.write_chunk(&init).await?;
    writer.flush().await?;
    Ok(writer)
}
