use super::playlist;
use super::segment_ring::SegmentRing;
use crate::{
    Result,
    http::{HttpRequest, ResponseWriter},
};
use std::time::Duration;
use tokio::io::AsyncWrite;

/// Serve an HLS M3U8 playlist.
pub async fn serve_playlist<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
    is_live: bool,
    target_duration: Duration,
    asset_prefix: &str,
) -> Result<ResponseWriter<W>> {
    let segments = ring.snapshot();

    let body = if is_live {
        playlist::write_live_playlist_at(&segments, target_duration, asset_prefix)
    } else {
        playlist::write_vod_playlist_at(&segments, asset_prefix)
    };

    writer
        .write_body(200, "application/vnd.apple.mpegurl", body.as_bytes())
        .await?;
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
            writer
                .write_body(404, "text/plain", b"Segment not found")
                .await?;
            return Ok(writer);
        }
    };

    let content_length = segment.data.len().to_string();
    let headers = [
        ("Content-Type", "video/iso.segment"),
        ("Content-Length", content_length.as_str()),
        ("Cache-Control", "public, max-age=3600"),
        ("Access-Control-Allow-Origin", "*"),
        ("Connection", "close"),
    ];
    writer.write_headers(200, &headers).await?;
    writer.write_raw(&segment.data).await?;
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
        ("Cache-Control", "public, max-age=3600"),
        ("Access-Control-Allow-Origin", "*"),
        ("Connection", "close"),
    ];
    writer.write_headers(200, &headers).await?;
    writer.write_raw(&init).await?;
    writer.flush().await?;
    Ok(writer)
}
