//! DASH HTTP handlers for MPD, init segment, and media segments.
use super::mpd::{self, MpdConfig};
use rtflow_core::error::Result;
use rtflow_hls::segment_ring::SegmentRing;
use rtflow_http::{HttpRequest, ResponseWriter};
use rtflow_media::MediaDescription;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::SystemTime;
use tokio::io::AsyncWrite;

/// Shared state for the DASH MPD handler, tracking the fixed availability
/// start time (wall-clock instant of the first segment).
pub struct DashState {
    /// Seconds since UNIX epoch when the first segment was produced.
    /// Stored as AtomicI64 so it can be set once from the segmenter task.
    availability_epoch_secs: AtomicI64,
    /// The raw segment timestamp (microseconds) of the very first segment produced.
    /// Used as the base for computing absolute SegmentTimeline `t` values.
    base_timestamp_micros: AtomicI64,
    /// The actual H.264 codec string derived from the SPS (e.g., "avc1.640028").
    codec_string: std::sync::OnceLock<String>,
    /// Video dimensions (width, height) parsed from the init segment's avcC/hvcC.
    video_dimensions: std::sync::OnceLock<(u16, u16)>,
}

impl Default for DashState {
    fn default() -> Self {
        Self::new()
    }
}

impl DashState {
    pub fn new() -> Self {
        Self {
            availability_epoch_secs: AtomicI64::new(0),
            base_timestamp_micros: AtomicI64::new(-1),
            codec_string: std::sync::OnceLock::new(),
            video_dimensions: std::sync::OnceLock::new(),
        }
    }

    /// Record the availability start instant (called once on first segment).
    pub fn set_availability_start(&self, time: SystemTime) {
        let secs = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.availability_epoch_secs.store(secs, Ordering::Release);
    }

    /// Get the fixed availability start time.
    pub fn availability_start(&self) -> SystemTime {
        let secs = self.availability_epoch_secs.load(Ordering::Acquire);
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
    }

    /// Record the base timestamp (first segment's PTS) for timeline calculation.
    pub fn set_base_timestamp(&self, ts: std::time::Duration) {
        let micros = ts.as_micros() as i64;
        self.base_timestamp_micros.store(micros, Ordering::Release);
    }

    /// Get the base timestamp. Returns None if not yet set.
    pub fn base_timestamp(&self) -> Option<std::time::Duration> {
        let micros = self.base_timestamp_micros.load(Ordering::Acquire);
        if micros < 0 {
            None
        } else {
            Some(std::time::Duration::from_micros(micros as u64))
        }
    }

    /// Set the actual codec string derived from the init segment's avcC.
    pub fn set_codec_string(&self, s: String) {
        let _ = self.codec_string.set(s);
    }

    /// Get the actual codec string, or None if not yet determined.
    pub fn codec_string(&self) -> Option<&str> {
        self.codec_string.get().map(|s| s.as_str())
    }

    /// Set the video dimensions parsed from the init segment.
    pub fn set_video_dimensions(&self, width: u16, height: u16) {
        let _ = self.video_dimensions.set((width, height));
    }

    /// Get the video dimensions, or None if not yet determined.
    pub fn video_dimensions(&self) -> Option<(u16, u16)> {
        self.video_dimensions.get().copied()
    }

    /// Reset timeline state when a publisher reconnects (new timestamp epoch).
    /// Allows `set_availability_start` and `set_base_timestamp` to be called again.
    pub fn reset_timeline(&self) {
        self.availability_epoch_secs.store(0, Ordering::Release);
        self.base_timestamp_micros.store(-1, Ordering::Release);
    }
}

/// Serialize a parsed query map back into `k=v&k2=v2` form (used to
/// propagate auth tokens from the manifest request into segment URLs).
fn serialize_query(query: &std::collections::HashMap<String, String>) -> String {
    let mut parts: Vec<String> = query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    parts.sort();
    parts.join("&")
}

/// Serve the DASH MPD manifest.
pub async fn serve_mpd<W: AsyncWrite + Unpin>(
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
    description: &MediaDescription,
    config: &MpdConfig,
    asset_prefix: &str,
    state: &DashState,
) -> Result<ResponseWriter<W>> {
    let segments = ring.snapshot();

    if segments.is_empty() {
        writer
            .write_body(
                503,
                "application/dash+xml",
                b"<MPD/>\n<!-- No segments available yet -->",
            )
            .await?;
        return Ok(writer);
    }

    // Propagate the request query (e.g. ?token=...) into embedded segment
    // URLs so token-protected deployments do not break on segment fetches.
    let query = serialize_query(&req.query);
    let body = mpd::generate_mpd_q(
        &segments,
        description,
        config,
        asset_prefix,
        state.availability_start(),
        state.base_timestamp().unwrap_or_default(),
        state.codec_string(),
        state.video_dimensions(),
        &query,
    );

    let content_length = body.len().to_string();
    let headers = [
        ("Content-Type", "application/dash+xml"),
        ("Content-Length", content_length.as_str()),
        ("Cache-Control", "no-cache, no-store, must-revalidate"),
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "GET, OPTIONS"),
        ("Connection", "close"),
    ];
    writer.write_headers(200, &headers).await?;
    writer.write_raw(body.as_bytes()).await?;
    writer.flush().await?;
    Ok(writer)
}

/// Serve the DASH init segment (ftyp + moov).
pub async fn serve_init_segment<W: AsyncWrite + Unpin>(
    req: HttpRequest,
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

    // ETag based on content hash (init segment is immutable)
    let etag = format!("\"init-{}\"", simple_hash(&init));

    // Check If-None-Match
    if let Some(inm) = req.header("if-none-match")
        && inm == etag
    {
        writer
            .write_headers(304, &[("ETag", etag.as_str())])
            .await?;
        writer.flush().await?;
        return Ok(writer);
    }

    let content_length = init.len().to_string();
    let headers = [
        ("Content-Type", "video/mp4"),
        ("Content-Length", content_length.as_str()),
        ("Cache-Control", "public, max-age=86400, immutable"),
        ("ETag", etag.as_str()),
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
    req: HttpRequest,
    mut writer: ResponseWriter<W>,
    ring: &SegmentRing,
    sequence: u64,
) -> Result<ResponseWriter<W>> {
    let segment = match ring.get(sequence) {
        Some(s) => s,
        None => {
            // DASH-IF recommended error response
            writer
                .write_body(404, "text/plain", b"Segment not found in sliding window")
                .await?;
            return Ok(writer);
        }
    };

    // ETag based on sequence number (segments are immutable)
    let etag = format!("\"seg-{sequence}\"");

    // Check If-None-Match
    if let Some(inm) = req.header("if-none-match")
        && inm == etag
    {
        writer
            .write_headers(304, &[("ETag", etag.as_str())])
            .await?;
        writer.flush().await?;
        return Ok(writer);
    }

    let content_length = segment.data.len().to_string();
    let headers = [
        ("Content-Type", "video/iso.segment"),
        ("Content-Length", content_length.as_str()),
        ("Cache-Control", "public, max-age=3600, immutable"),
        ("ETag", etag.as_str()),
        ("Access-Control-Allow-Origin", "*"),
        ("Connection", "close"),
    ];
    writer.write_headers(200, &headers).await?;
    writer.write_raw(&segment.data).await?;
    writer.flush().await?;
    Ok(writer)
}

/// Simple hash for ETag generation (FNV-1a).
fn simple_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
