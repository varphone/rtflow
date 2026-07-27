//! DASH HTTP handlers for MPD, init segment, and media segments.
use super::mpd::{self, MpdConfig};
use crate::hls::segment_ring::SegmentRing;
use crate::media::MediaDescription;
use crate::{
    Result,
    http::{HttpRequest, ResponseWriter},
};
use tokio::io::AsyncWrite;

/// Serve the DASH MPD manifest.
pub async fn serve_mpd<W: AsyncWrite + Unpin>(
    _req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
    description: &MediaDescription,
    config: &MpdConfig,
    asset_prefix: &str,
) -> Result<ResponseWriter<W>> {
    let segments = ring.snapshot();

    if segments.is_empty() {
        writer
            .write_body(503, "text/plain", b"No segments available yet")
            .await?;
        return Ok(writer);
    }

    let body = mpd::generate_mpd(&segments, description, config, asset_prefix);

    let headers = [
        ("Content-Type", "application/dash+xml"),
        ("Cache-Control", "no-cache, no-store, must-revalidate"),
        ("Access-Control-Allow-Origin", "*"),
        ("Connection", "close"),
    ];
    writer.write_headers(200, &headers).await?;
    writer.write_raw(body.as_bytes()).await?;
    writer.flush().await?;
    Ok(writer)
}

/// Serve the DASH init segment (ftyp + moov).
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
        ("Cache-Control", "public, max-age=86400"),
        ("Access-Control-Allow-Origin", "*"),
        ("Connection", "close"),
    ];
    writer.write_headers(200, &headers).await?;
    writer.write_raw(&init).await?;
    writer.flush().await?;
    Ok(writer)
}

/// Serve a DASH media segment (moof + mdat).
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
