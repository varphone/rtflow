//! Disk-backed DASH VOD generation from `VodSource` (recordings or files).
//!
//! Each source is demuxed once and packaged into per-track fMP4:
//! `video-init.mp4`/`video-N.m4s` and `audio-init.mp4`/`audio-N.m4s`, with a
//! static MPD. The cache is keyed by the source fingerprint and regenerated
//! atomically.
use crate::mpd::{self, MpdConfig};
use crate::{extract_h264_codec_string, extract_h265_codec_string, extract_video_dimensions};
use rtflow_hls::segmenter::{Segment, Segmenter, SegmenterConfig};
use rtflow_http::Handler;
use rtflow_media::MediaKind;
use rtflow_media::vod::VodSource;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::Mutex;

const VOD_CACHE_VERSION: u32 = 6;

#[derive(Clone)]
pub struct DashVodEntry {
    pub id: String,
    pub title: String,
    pub duration: Option<Duration>,
    pub source: Arc<dyn VodSource>,
}

#[derive(Clone, Debug)]
pub struct DashVodConfig {
    pub segment_duration: Duration,
    pub cache_dir: PathBuf,
}

pub struct DashVodManager {
    entries: RwLock<HashMap<String, DashVodEntry>>,
    config: DashVodConfig,
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl DashVodManager {
    pub fn new(entries: Vec<DashVodEntry>, config: DashVodConfig) -> Self {
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

    pub fn entries(&self) -> Vec<DashVodEntry> {
        let mut entries: Vec<DashVodEntry> = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        entries
    }

    pub fn entry(&self, id: &str) -> Option<DashVodEntry> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    pub fn upsert_entry(&self, entry: DashVodEntry) {
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
            .ok_or_else(|| rtflow_core::error::Error::Rtsp("DASH VOD id not found".into()))?;
        let valid_id = id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@'));
        if !valid_id || id == "." || id == ".." || id.starts_with('.') {
            return Err(rtflow_core::error::Error::InvalidConfig(
                "invalid DASH VOD id".into(),
            ));
        }
        let directory = self.config.cache_dir.join(id);
        let fingerprint = entry.source.fingerprint();
        let cache_valid = |directory: &Path| {
            if !directory.join("manifest.mpd").is_file() {
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
        let target = directory.clone();
        let temporary_for_cleanup = temporary.clone();
        let config = self.config.clone();
        let source = entry.source.clone();
        let title = entry.title.clone();
        let duration = entry.duration;
        tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_dir_all(&temporary_for_cleanup);
            if let Err(error) = generate(source, &title, duration, &config, &temporary_for_cleanup)
            {
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
            rtflow_core::error::Error::Media(format!("DASH VOD worker failed: {error}"))
        })??;
        Ok(directory)
    }

    pub async fn manifest(&self, id: &str, query: &str) -> rtflow_core::error::Result<String> {
        let directory = self.ensure(id).await?;
        let manifest = tokio::fs::read_to_string(directory.join("manifest.mpd")).await?;
        Ok(append_query(&manifest, query))
    }

    pub async fn file(&self, id: &str, name: &str) -> rtflow_core::error::Result<Option<Vec<u8>>> {
        let directory = self.ensure(id).await?;
        let path = directory.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(tokio::fs::read(path).await?))
    }
}

fn append_query(manifest: &str, query: &str) -> String {
    if query.is_empty() {
        return manifest.to_owned();
    }
    manifest
        .replace("-init.mp4\"", &format!("-init.mp4?{query}\""))
        .replace(".m4s\"", &format!(".m4s?{query}\""))
}

fn generate(
    source: Arc<dyn VodSource>,
    title: &str,
    duration: Option<Duration>,
    config: &DashVodConfig,
    directory: &Path,
) -> rtflow_core::error::Result<()> {
    std::fs::create_dir_all(directory)?;
    let description = source.description().clone();
    tracing::info!(
        title,
        tracks = description.tracks.len(),
        segment_ms = config.segment_duration.as_millis() as u64,
        "DASH VOD generation started"
    );
    let video_tracks: Vec<rtflow_media::TrackDescription> = description
        .tracks
        .iter()
        .filter(|track| track.kind == MediaKind::Video)
        .cloned()
        .collect();
    let audio_tracks: Vec<rtflow_media::TrackDescription> = description
        .tracks
        .iter()
        .filter(|track| track.kind == MediaKind::Audio)
        .cloned()
        .collect();
    let video_descriptions: Vec<rtflow_media::MediaDescription> = video_tracks
        .iter()
        .map(|track| {
            let mut filtered = description.clone();
            filtered.tracks = vec![track.clone()];
            filtered
        })
        .collect();
    let audio_descriptions: Vec<rtflow_media::MediaDescription> = audio_tracks
        .iter()
        .map(|track| {
            let mut filtered = description.clone();
            filtered.tracks = vec![track.clone()];
            filtered
        })
        .collect();
    let video_index: HashMap<u8, usize> = video_tracks
        .iter()
        .enumerate()
        .map(|(index, track)| (track.id, index))
        .collect();

    let segmenter_config = SegmenterConfig {
        segment_duration: config.segment_duration,
        lookback_count: 2,
    };
    let audio_segment_duration = if video_tracks.is_empty() {
        config.segment_duration / 2
    } else {
        config.segment_duration.saturating_mul(2)
    };
    let mut video_segmenters: Vec<Segmenter> = video_descriptions
        .iter()
        .map(|description| Segmenter::new(segmenter_config.clone(), description))
        .collect();
    let mut video_receivers: Vec<_> = video_segmenters
        .iter()
        .map(|segmenter| segmenter.subscribe())
        .collect();
    let mut video_segments: Vec<Vec<Segment>> = vec![Vec::new(); video_segmenters.len()];
    let mut audio_segmenters: Vec<Segmenter> = audio_descriptions
        .iter()
        .map(|description| {
            Segmenter::new(
                SegmenterConfig {
                    segment_duration: audio_segment_duration,
                    lookback_count: 2,
                },
                description,
            )
        })
        .collect();
    let mut audio_receivers: Vec<_> = audio_segmenters
        .iter()
        .map(|segmenter| segmenter.subscribe())
        .collect();
    let audio_index: HashMap<u8, usize> = audio_tracks
        .iter()
        .enumerate()
        .map(|(index, track)| (track.id, index))
        .collect();
    let mut audio_segments: Vec<Vec<Segment>> = vec![Vec::new(); audio_segmenters.len()];
    let mut demuxer = source.open()?;
    while let Some(sample) = demuxer.next_sample()? {
        if let Some(index) = video_index.get(&sample.unit.track_id).copied() {
            video_segmenters[index].push(sample.unit);
        } else if let Some(index) = audio_index.get(&sample.unit.track_id).copied() {
            audio_segmenters[index].push(sample.unit);
        }
        for (index, receiver) in video_receivers.iter_mut().enumerate() {
            while let Ok(segment) = receiver.try_recv() {
                video_segments[index].push(segment);
                for segmenter in &mut audio_segmenters {
                    segmenter.cut_current();
                }
            }
        }
        for (index, receiver) in audio_receivers.iter_mut().enumerate() {
            while let Ok(segment) = receiver.try_recv() {
                audio_segments[index].push(segment);
            }
        }
    }
    for segmenter in &mut video_segmenters {
        segmenter.flush();
    }
    for segmenter in &mut audio_segmenters {
        segmenter.flush();
    }
    for (index, receiver) in video_receivers.iter_mut().enumerate() {
        while let Ok(segment) = receiver.try_recv() {
            video_segments[index].push(segment);
        }
    }
    for (index, receiver) in audio_receivers.iter_mut().enumerate() {
        while let Ok(segment) = receiver.try_recv() {
            audio_segments[index].push(segment);
        }
    }
    tracing::info!(
        title,
        video_segments = video_segments.iter().map(Vec::len).sum::<usize>(),
        audio_segments = audio_segments.iter().map(Vec::len).sum::<usize>(),
        "DASH VOD generation finished"
    );
    if video_segments.iter().all(Vec::is_empty) && audio_segments.iter().all(Vec::is_empty) {
        return Err(rtflow_core::error::Error::Media(format!(
            "DASH VOD {title} produced no segments"
        )));
    }
    for (track, segments) in video_tracks.iter().zip(&video_segments) {
        let Some(first) = segments.first() else {
            continue;
        };
        std::fs::write(
            directory.join(format!("video-{}-init.mp4", track.id)),
            &first.init,
        )?;
        for segment in segments {
            std::fs::write(
                directory.join(format!("video-{}-{}.m4s", track.id, segment.sequence)),
                &segment.data,
            )?;
        }
    }
    let audio_with_segments: Vec<(&rtflow_media::TrackDescription, &[Segment])> = audio_tracks
        .iter()
        .zip(&audio_segments)
        .filter(|(_, segments)| !segments.is_empty())
        .map(|(track, segments)| (track, segments.as_slice()))
        .collect();
    let multiple_audio = audio_with_segments.len() > 1;
    for (track, segments) in &audio_with_segments {
        let prefix = if multiple_audio {
            format!("audio-{}", track.id)
        } else {
            "audio".to_string()
        };
        if let Some(first) = segments.first() {
            std::fs::write(directory.join(format!("{prefix}-init.mp4")), &first.init)?;
        }
        for segment in *segments {
            std::fs::write(
                directory.join(format!("{prefix}-{}.m4s", segment.sequence)),
                &segment.data,
            )?;
        }
    }
    let duration = duration.unwrap_or_else(|| {
        video_segments
            .iter()
            .flat_map(|segments| segments.iter())
            .chain(audio_segments.iter().flat_map(|segments| segments.iter()))
            .map(|segment| segment.timestamp.saturating_add(segment.duration))
            .max()
            .unwrap_or_default()
    });
    let video_reps: Vec<mpd::MpdVideoTrack<'_>> = video_tracks
        .iter()
        .zip(&video_segments)
        .filter(|(_, segments)| !segments.is_empty())
        .map(|(track, segments)| {
            let codec = segments.first().and_then(|segment| {
                extract_h264_codec_string(&segment.init)
                    .or_else(|| extract_h265_codec_string(&segment.init))
            });
            let dimensions = segments
                .first()
                .and_then(|segment| extract_video_dimensions(&segment.init));
            mpd::MpdVideoTrack {
                id: track.id,
                track,
                segments,
                codec,
                dimensions,
            }
        })
        .collect();
    let audio_reps: Vec<mpd::MpdAudioTrack<'_>> = audio_tracks
        .iter()
        .zip(&audio_segments)
        .filter(|(_, segments)| !segments.is_empty())
        .map(|(track, segments)| mpd::MpdAudioTrack {
            id: track.id,
            track,
            segments,
        })
        .collect();
    let manifest = mpd::generate_static_mpd(
        &video_reps,
        &audio_reps,
        &description,
        &MpdConfig::default(),
        duration,
        "",
    );
    std::fs::write(directory.join("manifest.mpd"), manifest)?;
    Ok(())
}

fn serialize_query(query: &HashMap<String, String>) -> String {
    let mut parts: Vec<String> = query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    parts.sort();
    parts.join("&")
}

pub fn create_dash_vod_handler(manager: Arc<DashVodManager>) -> Handler {
    Arc::new(move |req, mut writer| {
        let manager = manager.clone();
        Box::pin(async move {
            let Some(rest) = req.path.strip_prefix("/dash-vod/") else {
                writer.write_body(404, "text/plain", b"not found").await?;
                return Ok(writer);
            };
            let Some((id, name)) = rest.split_once('/') else {
                writer.write_body(404, "text/plain", b"not found").await?;
                return Ok(writer);
            };
            if name.contains('/') || name.contains("..") {
                writer.write_body(404, "text/plain", b"not found").await?;
                return Ok(writer);
            }
            if name == "manifest.mpd" {
                let query = serialize_query(&req.query);
                match manager.manifest(id, &query).await {
                    Ok(manifest) => {
                        writer
                            .write_body(200, "application/dash+xml", manifest.as_bytes())
                            .await?;
                    }
                    Err(error) => {
                        tracing::warn!(%error, id, "DASH VOD manifest failed");
                        writer
                            .write_body(404, "text/plain", b"VOD not found")
                            .await?;
                    }
                }
                return Ok(writer);
            }
            match manager.file(id, name).await {
                Ok(Some(data)) => {
                    let content_type = if name.ends_with(".mp4") {
                        if name.starts_with("audio-") {
                            "audio/mp4"
                        } else {
                            "video/mp4"
                        }
                    } else if name.starts_with("audio-") {
                        "audio/iso.segment"
                    } else {
                        "video/iso.segment"
                    };
                    writer.write_body(200, content_type, &data).await?;
                }
                Ok(None) => {
                    writer.write_body(404, "text/plain", b"not found").await?;
                }
                Err(error) => {
                    tracing::warn!(%error, id, name, "DASH VOD file failed");
                    writer
                        .write_body(404, "text/plain", b"VOD not found")
                        .await?;
                }
            }
            Ok(writer)
        })
    })
}
