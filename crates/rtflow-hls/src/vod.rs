//! Disk-backed HLS VOD generation from `VodSource` (recordings or files).
use super::{
    playlist,
    segmenter::{Segment, Segmenter, SegmenterConfig},
};
use rtflow_http::{Handler, Method, ResponseWriter};
use rtflow_media::vod::VodSource;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::Mutex;

/// One VOD source exposed through HLS.
#[derive(Clone)]
pub struct HlsVodEntry {
    pub id: String,
    pub title: String,
    pub duration: Option<Duration>,
    pub source: Arc<dyn VodSource>,
}

/// VOD generation configuration.
#[derive(Clone, Debug)]
pub struct HlsVodConfig {
    pub segment_duration: Duration,
    pub cache_dir: PathBuf,
}

/// Bump when the generated fMP4 layout/codec handling changes so existing
/// on-disk VOD caches are regenerated instead of serving stale corrupt data.
const VOD_CACHE_VERSION: u32 = 4;

/// Caches generated fMP4 files on disk and serializes generation per id.
pub struct HlsVodManager {
    entries: RwLock<HashMap<String, HlsVodEntry>>,
    config: HlsVodConfig,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl HlsVodManager {
    pub fn new(entries: Vec<HlsVodEntry>, config: HlsVodConfig) -> Self {
        Self {
            entries: RwLock::new(
                entries
                    .into_iter()
                    .map(|entry| (entry.id.clone(), entry))
                    .collect(),
            ),
            config,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Remove a generated cache entry so the next request regenerates it
    /// from the (possibly changed) source.
    pub fn invalidate(&self, id: &str) -> rtflow_core::error::Result<bool> {
        if !self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(id)
        {
            return Ok(false);
        }
        let directory = self.config.cache_dir.join(id);
        if directory.is_dir() {
            std::fs::remove_dir_all(directory)?;
        }
        Ok(true)
    }

    /// Snapshot of the current catalog, ordered by id.
    pub fn entries(&self) -> Vec<HlsVodEntry> {
        let mut entries: Vec<HlsVodEntry> = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        entries
    }

    pub fn entry(&self, id: &str) -> Option<HlsVodEntry> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// Add or replace an entry. Used by the recording/VOD catalog when new
    /// recordings finish while the server is running.
    pub fn upsert_entry(&self, entry: HlsVodEntry) {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(entry.id.clone(), entry);
    }

    pub fn remove_entry(&self, id: &str) -> bool {
        let removed = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
            .is_some();
        if removed {
            let _ = std::fs::remove_dir_all(self.config.cache_dir.join(id));
        }
        removed
    }

    async fn lock_for(&self, id: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn ensure(&self, id: &str) -> rtflow_core::error::Result<PathBuf> {
        let entry = self
            .entry(id)
            .ok_or_else(|| rtflow_core::error::Error::Rtsp("VOD id not found".into()))?;
        let valid = id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'));
        if !valid || id == "." || id == ".." || id.starts_with('.') {
            return Err(rtflow_core::error::Error::InvalidConfig(
                "invalid VOD id".into(),
            ));
        }
        let directory = self.config.cache_dir.join(id);
        let fingerprint = entry.source.fingerprint();
        let cache_valid = |directory: &Path| {
            if !directory.join("index.m3u8").is_file() {
                return false;
            }
            match fingerprint.as_deref() {
                Some(expected) => std::fs::read_to_string(directory.join(".fingerprint"))
                    .is_ok_and(|stored| stored == format!("{VOD_CACHE_VERSION}:{expected}")),
                None => true,
            }
        };
        if cache_valid(&directory) {
            return Ok(directory);
        }
        let lock = self.lock_for(id).await;
        let _guard = lock.lock().await;
        if cache_valid(&directory) {
            return Ok(directory);
        }
        if directory.exists() {
            std::fs::remove_dir_all(&directory)?;
        }
        let temporary = self.config.cache_dir.join(format!(".tmp-{id}"));
        let source = entry.source.clone();
        let config = self.config.clone();
        let label = entry.title.clone();
        let target = directory.clone();
        let temporary_for_cleanup = temporary.clone();
        tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_dir_all(&temporary_for_cleanup);
            if let Err(error) = generate(source, &label, &config, &temporary_for_cleanup) {
                let _ = std::fs::remove_dir_all(&temporary_for_cleanup);
                return Err(error);
            }
            if let Some(fingerprint) = fingerprint {
                let _ = std::fs::write(
                    temporary_for_cleanup.join(".fingerprint"),
                    format!("{VOD_CACHE_VERSION}:{fingerprint}"),
                );
            }
            if let Err(error) = std::fs::rename(&temporary_for_cleanup, &target) {
                let _ = std::fs::remove_dir_all(&temporary_for_cleanup);
                return Err(error.into());
            }
            Ok::<(), rtflow_core::error::Error>(())
        })
        .await
        .map_err(|error| {
            rtflow_core::error::Error::Media(format!("VOD worker failed: {error}"))
        })??;
        Ok(directory)
    }

    async fn playlist(&self, id: &str) -> rtflow_core::error::Result<String> {
        let directory = self.ensure(id).await?;
        Ok(tokio::fs::read_to_string(directory.join("index.m3u8")).await?)
    }

    async fn file(&self, id: &str, name: &str) -> rtflow_core::error::Result<Option<Vec<u8>>> {
        let directory = self.ensure(id).await?;
        let path = directory.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(tokio::fs::read(path).await?))
    }
}

fn generate(
    source: Arc<dyn VodSource>,
    title: &str,
    config: &HlsVodConfig,
    directory: &Path,
) -> rtflow_core::error::Result<()> {
    std::fs::create_dir_all(directory)?;
    let description = source.description().clone();
    tracing::info!(
        title,
        tracks = description.tracks.len(),
        segment_ms = config.segment_duration.as_millis() as u64,
        "HLS VOD generation started"
    );
    let mut demuxer = source.open()?;
    // MPEG-TS is the most interoperable HLS container (Firefox MSE rejects
    // this project's fMP4 init/sample layout). Fall back to fMP4 when the
    // description has no codec configuration for the TS pipeline.
    let mut segmenter = Segmenter::new_mpegts(
        SegmenterConfig {
            segment_duration: config.segment_duration,
            lookback_count: 1,
        },
        &description,
    );
    let mut segments = segmenter.subscribe();
    let mut completed = Vec::new();
    while let Some(sample) = demuxer.next_sample()? {
        segmenter.push(sample.unit);
        while let Ok(segment) = segments.try_recv() {
            completed.push(segment);
        }
    }
    segmenter.flush();
    while let Ok(segment) = segments.try_recv() {
        completed.push(segment);
    }
    tracing::info!(
        title,
        segments = completed.len(),
        "HLS VOD generation finished"
    );
    if completed.is_empty() {
        return Err(rtflow_core::error::Error::Media(format!(
            "VOD {title} produced no segments"
        )));
    }
    let mpeg_ts = completed
        .first()
        .is_some_and(|segment| segment.format == crate::segmenter::SegmentFormat::MpegTs);
    if !mpeg_ts {
        let init = completed
            .first()
            .map(|segment| segment.init.clone())
            .unwrap_or_default();
        std::fs::write(directory.join("init.m4s"), &init)?;
    }
    let extension = if mpeg_ts { "ts" } else { "m4s" };
    for segment in &completed {
        std::fs::write(
            directory.join(format!("{}.{extension}", segment.sequence)),
            &segment.data,
        )?;
    }
    let playlist_segments: Vec<Segment> = completed
        .into_iter()
        .map(|mut segment| {
            segment.data = bytes::Bytes::new();
            segment
        })
        .collect();
    // Give every regeneration a new URI token: a browser that cached an old
    // (possibly corrupt) init/segment under the same name must fetch the new
    // files instead of reusing them.
    let token = format!(
        "v={VOD_CACHE_VERSION}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let body = playlist::write_vod_playlist_at_q(&playlist_segments, "", &token);
    std::fs::write(directory.join("index.m3u8"), body)?;
    Ok(())
}

fn parse(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/vod/")?;
    if let Some(id) = rest.strip_suffix("/index.m3u8") {
        return Some((id, "playlist"));
    }
    if let Some(id) = rest.strip_suffix("/init.m4s") {
        return Some((id, "init"));
    }
    let (id, file) = rest.rsplit_once('/')?;
    if file.ends_with(".m4s") || file.ends_with(".ts") {
        return Some((id, "segment"));
    }
    None
}

fn byte_range(header: Option<&str>, length: usize) -> Option<(usize, usize)> {
    let value = header?.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        // Suffix form: bytes=-N asks for the last N bytes.
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

/// Handler for `/vod/{id}/index.m3u8`, `/init.m4s`, `/{seq}.m4s`.
pub fn create_hls_vod_handler(manager: Arc<HlsVodManager>) -> Handler {
    Arc::new(move |req, mut writer| {
        let manager = manager.clone();
        Box::pin(async move {
            let Some((id, kind)) = parse(&req.path).map(|(id, kind)| (id.to_owned(), kind)) else {
                writer.write_body(404, "text/plain", b"Not Found").await?;
                return Ok(writer);
            };
            if manager.entry(&id).is_none() {
                writer
                    .write_body(404, "text/plain", b"VOD not found")
                    .await?;
                return Ok(writer);
            }
            let head = req.method == Method::Head;
            match kind {
                "playlist" => match manager.playlist(&id).await {
                    Ok(body) => {
                        let length = body.len().to_string();
                        writer
                            .write_headers(
                                200,
                                &[
                                    ("Content-Type", "application/vnd.apple.mpegurl"),
                                    ("Content-Length", length.as_str()),
                                    ("Cache-Control", "no-store"),
                                    ("Access-Control-Allow-Origin", "*"),
                                    ("Connection", "close"),
                                ],
                            )
                            .await?;
                        if !head {
                            writer.write_raw(body.as_bytes()).await?;
                            writer.flush().await?;
                        }
                        Ok(writer)
                    }
                    Err(error) => {
                        tracing::warn!(%error, %id, "HLS VOD generation failed");
                        writer
                            .write_body(500, "text/plain", b"VOD generation failed")
                            .await?;
                        Ok(writer)
                    }
                },
                "init" | "segment" => {
                    let name = if kind == "init" {
                        "init.m4s".to_owned()
                    } else {
                        req.path.rsplit('/').next().unwrap_or("").to_owned()
                    };
                    match manager.file(&id, &name).await {
                        Ok(Some(body)) => {
                            let content_type = if kind == "init" {
                                "video/mp4"
                            } else if name.ends_with(".ts") {
                                "video/mp2t"
                            } else {
                                "video/iso.segment"
                            };
                            let range = byte_range(req.header("range"), body.len());
                            let (status, slice_start, slice_end) = match range {
                                Some((start, end)) => (206, start, end + 1),
                                None => (200, 0, body.len()),
                            };
                            let length = (slice_end - slice_start).to_string();
                            let content_range = format!(
                                "bytes {slice_start}-{}/{body_len}",
                                slice_end - 1,
                                body_len = body.len()
                            );
                            let mut headers = vec![
                                ("Content-Type", content_type),
                                ("Content-Length", length.as_str()),
                                ("Cache-Control", "no-store"),
                                ("Access-Control-Allow-Origin", "*"),
                                ("Accept-Ranges", "bytes"),
                                ("Connection", "close"),
                            ];
                            if status == 206 {
                                headers.push(("Content-Range", content_range.as_str()));
                            }
                            writer.write_headers(status, &headers).await?;
                            if !head {
                                writer.write_raw(&body[slice_start..slice_end]).await?;
                            }
                            writer.flush().await?;
                            Ok(writer)
                        }
                        Ok(None) => {
                            writer.write_body(404, "text/plain", b"Not Found").await?;
                            Ok(writer)
                        }
                        Err(error) => {
                            tracing::warn!(%error, %id, "HLS VOD file failed");
                            writer.write_body(500, "text/plain", b"VOD failed").await?;
                            Ok(writer)
                        }
                    }
                }
                _ => {
                    writer.write_body(404, "text/plain", b"Not Found").await?;
                    Ok(writer)
                }
            }
        })
    })
}

#[allow(dead_code)]
fn _keep_handler_import(_: ResponseWriter<tokio::net::tcp::OwnedWriteHalf>) {}

#[cfg(test)]
mod tests {
    use super::byte_range;

    #[test]
    fn byte_range_supports_suffix_and_open_ended() {
        assert_eq!(byte_range(Some("bytes=-10"), 100), Some((90, 99)));
        assert_eq!(byte_range(Some("bytes=10-"), 100), Some((10, 99)));
        assert_eq!(byte_range(Some("bytes=10-19"), 100), Some((10, 19)));
        assert_eq!(byte_range(Some("bytes=-0"), 100), None);
        assert_eq!(byte_range(Some("bytes=200-"), 100), None);
    }
}
