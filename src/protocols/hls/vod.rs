//! Disk-backed HLS VOD generation from `VodSource` (recordings or files).
use super::{
    playlist,
    segmenter::{Segment, Segmenter, SegmenterConfig},
};
use crate::{
    http::{Handler, ResponseWriter},
    media::vod::VodSource,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
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

/// Caches generated fMP4 files on disk and serializes generation per id.
pub struct HlsVodManager {
    entries: HashMap<String, HlsVodEntry>,
    config: HlsVodConfig,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl HlsVodManager {
    pub fn new(entries: Vec<HlsVodEntry>, config: HlsVodConfig) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|entry| (entry.id.clone(), entry))
                .collect(),
            config,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// Remove a generated cache entry so the next request regenerates it
    /// from the (possibly changed) source.
    pub fn invalidate(&self, id: &str) -> crate::Result<bool> {
        if !self.entries.contains_key(id) {
            return Ok(false);
        }
        let directory = self.config.cache_dir.join(id);
        if directory.is_dir() {
            std::fs::remove_dir_all(directory)?;
        }
        Ok(true)
    }

    pub fn entries(&self) -> impl Iterator<Item = &HlsVodEntry> {
        self.entries.values()
    }

    pub fn entry(&self, id: &str) -> Option<&HlsVodEntry> {
        self.entries.get(id)
    }

    async fn lock_for(&self, id: &str) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn ensure(&self, id: &str) -> crate::Result<PathBuf> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| crate::Error::Rtsp("VOD id not found".into()))?;
        let valid = id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !valid {
            return Err(crate::Error::InvalidConfig("invalid VOD id".into()));
        }
        let directory = self.config.cache_dir.join(id);
        if directory.join("index.m3u8").is_file() {
            return Ok(directory);
        }
        let lock = self.lock_for(id).await;
        let _guard = lock.lock().await;
        if directory.join("index.m3u8").is_file() {
            return Ok(directory);
        }
        let temporary = self.config.cache_dir.join(format!(".tmp-{id}"));
        let source = entry.source.clone();
        let config = self.config.clone();
        let label = entry.title.clone();
        let target = directory.clone();
        tokio::task::spawn_blocking(move || {
            generate(source, &label, &config, &temporary)?;
            std::fs::rename(&temporary, &target)?;
            Ok::<(), crate::Error>(())
        })
        .await
        .map_err(|error| crate::Error::Media(format!("VOD worker failed: {error}")))??;
        Ok(directory)
    }

    async fn playlist(&self, id: &str) -> crate::Result<String> {
        let directory = self.ensure(id).await?;
        Ok(tokio::fs::read_to_string(directory.join("index.m3u8")).await?)
    }

    async fn file(&self, id: &str, name: &str) -> crate::Result<Option<Vec<u8>>> {
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
) -> crate::Result<()> {
    std::fs::create_dir_all(directory)?;
    let description = source.description().clone();
    let mut demuxer = source.open()?;
    let mut segmenter = Segmenter::new(
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
    if completed.is_empty() {
        return Err(crate::Error::Media(format!(
            "VOD {title} produced no segments"
        )));
    }
    let init = completed
        .first()
        .map(|segment| segment.init.clone())
        .unwrap_or_default();
    std::fs::write(directory.join("init.m4s"), &init)?;
    for segment in &completed {
        std::fs::write(
            directory.join(format!("{}.m4s", segment.sequence)),
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
    let body = playlist::write_vod_playlist(&playlist_segments);
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
    if file.ends_with(".m4s") {
        return Some((id, "segment"));
    }
    None
}

fn byte_range(header: Option<&str>, length: usize) -> Option<(usize, usize)> {
    let value = header?.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
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
                                    ("Cache-Control", "public, max-age=60"),
                                    ("Access-Control-Allow-Origin", "*"),
                                    ("Connection", "close"),
                                ],
                            )
                            .await?;
                        writer.write_raw(body.as_bytes()).await?;
                        writer.flush().await?;
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
                                ("Cache-Control", "public, max-age=3600"),
                                ("Access-Control-Allow-Origin", "*"),
                                ("Accept-Ranges", "bytes"),
                                ("Connection", "close"),
                            ];
                            if status == 206 {
                                headers.push(("Content-Range", content_range.as_str()));
                            }
                            writer.write_headers(status, &headers).await?;
                            writer.write_raw(&body[slice_start..slice_end]).await?;
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
