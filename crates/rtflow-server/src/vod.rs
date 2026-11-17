//! Recording directories to HLS VOD catalog.
use crate::{config::Config, recording::recording_directories};
use rtflow::{
    dash::vod::{DashVodEntry, DashVodManager},
    hls::vod::{HlsVodEntry, HlsVodManager},
    storage::{RecordingCatalog, RecordingVodSource},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

#[derive(Clone, Debug, serde::Serialize)]
pub struct VodStatus {
    pub id: String,
    pub title: String,
    pub duration_seconds: f64,
    pub playlist: String,
    pub dash: String,
}

pub struct VodCatalog {
    manager: Arc<HlsVodManager>,
    dash: Arc<DashVodManager>,
    root: PathBuf,
    max_sample_size: usize,
    known: RwLock<HashMap<String, (PathBuf, usize, Duration)>>,
}

impl VodCatalog {
    pub fn new(config: &Config, manager: Arc<HlsVodManager>, dash: Arc<DashVodManager>) -> Self {
        Self {
            manager,
            dash,
            root: config.recording.root.clone(),
            max_sample_size: 8 * 1024 * 1024,
            known: RwLock::new(HashMap::new()),
        }
    }

    pub fn manager(&self) -> &Arc<HlsVodManager> {
        &self.manager
    }

    /// Original mount path for a VOD id, so playback can be authorized
    /// against the path's read policy instead of a synthetic `vod` namespace.
    pub fn source_path(&self, id: &str) -> Option<String> {
        let directory = self
            .known
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .map(|(directory, _, _)| directory.clone())?;
        let relative = directory.strip_prefix(&self.root).ok()?;
        let parts: Vec<String> = relative
            .parent()?
            .components()
            .filter_map(|component| component.as_os_str().to_str().map(str::to_owned))
            .collect();
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    pub fn list(&self) -> Vec<VodStatus> {
        self.manager
            .entries()
            .into_iter()
            .map(|entry| VodStatus {
                id: entry.id.clone(),
                title: entry.title,
                duration_seconds: entry
                    .duration
                    .map(|duration| duration.as_secs_f64())
                    .unwrap_or(0.0),
                playlist: format!("/vod/{}/index.m3u8", entry.id),
                dash: format!("/dash-vod/{}/manifest.mpd", entry.id),
            })
            .collect()
    }

    /// Scan recordings and upsert/remove HLS VOD entries. Returns the
    /// number of entries now known.
    pub fn refresh(&self) -> usize {
        let mut found: HashMap<String, PathBuf> = HashMap::new();
        for directory in recording_directories(&self.root) {
            let Some(id) = vod_id(&self.root, &directory) else {
                continue;
            };
            if found.contains_key(&id) {
                continue;
            }
            let Ok(catalog) = RecordingCatalog::scan(&directory, self.max_sample_size) else {
                continue;
            };
            let source = RecordingVodSource::open(&directory, self.max_sample_size);
            let Ok(source) = source else {
                continue;
            };
            let duration = catalog.duration();
            let title = relative_title(&self.root, &directory);
            let source: Arc<dyn rtflow::storage::VodSource> = Arc::new(source);
            self.dash.upsert_entry(DashVodEntry {
                id: id.clone(),
                title: title.clone(),
                duration: Some(duration),
                source: source.clone(),
            });
            let entry = HlsVodEntry {
                id: id.clone(),
                title,
                duration: Some(duration),
                source,
            };
            // Keep the entry's duration and playlist current while the
            // recording is still being written: a rename-only comparison
            // leaves the first (short) snapshot in the catalog forever.
            let state = (directory.clone(), catalog.segments.len(), duration);
            let changed = self
                .known
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .is_none_or(|previous| previous != &state);
            if changed {
                self.manager.upsert_entry(entry);
                self.known
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id.clone(), state);
            }
            found.insert(id, directory);
        }
        let stale: Vec<String> = {
            let known = self
                .known
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            known
                .keys()
                .filter(|id| !found.contains_key(*id))
                .cloned()
                .collect()
        };
        for id in stale {
            self.manager.remove_entry(&id);
            self.dash.remove_entry(&id);
        }
        let mut known = self
            .known
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        known.retain(|id, _| found.contains_key(id));
        known.len()
    }

    pub fn remove(&self, id: &str) -> bool {
        self.dash.remove_entry(id);
        self.manager.remove_entry(id)
    }

    /// Prune expired recording sessions by directory mtime.
    pub fn prune(&self, retention: Duration) -> usize {
        crate::recording::prune_recordings(&self.root, retention)
    }
}

fn vod_id(root: &Path, directory: &Path) -> Option<String> {
    let relative = directory.strip_prefix(root).ok()?;
    let parts: Vec<String> = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(str::to_owned))
        .collect();
    let (timestamp, parent) = parts.split_last()?;
    if !timestamp.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let prefix = parent
        .join("_")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if prefix.is_empty() {
        return None;
    }
    Some(format!("{prefix}@{timestamp}"))
}

fn relative_title(root: &Path, directory: &Path) -> String {
    directory
        .strip_prefix(root)
        .ok()
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|| directory.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vod_id_is_url_safe() {
        let root = Path::new("/recordings");
        let id = vod_id(root, Path::new("/recordings/live/cam/1700000000")).unwrap();
        assert_eq!(id, "live_cam@1700000000");
        assert!(
            id.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '@'))
        );
    }
}
