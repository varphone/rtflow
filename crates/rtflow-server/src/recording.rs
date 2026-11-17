//! Recording supervisor: turns an online publisher mount into segmented
//! recordings, one directory per publish session.
use crate::{
    config::{Config, RecordingSection},
    hooks::{HookClient, HookKind, HookRequest},
    paths::PathStore,
};
use rtflow::{
    MountConfig, MountHandle, MountRegistry,
    storage::{RecordingCatalog, RecordingConfig, RecordingReport, SegmentRecorder},
};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Clone, Debug, Serialize)]
pub struct RecordingStatus {
    pub path: String,
    pub active: bool,
    pub started_at: u64,
    pub directory: String,
    pub segments: u64,
    pub bytes: u64,
    pub dropped: u64,
    pub last_error: Option<String>,
}

struct Session {
    started_at: u64,
    directory: PathBuf,
    cancel: tokio_util::sync::CancellationToken,
    feed: tokio::task::JoinHandle<()>,
    recorder: Option<SegmentRecorder>,
    segments: u64,
    bytes: u64,
    dropped: u64,
    last_error: Option<String>,
    manual: bool,
}

pub struct RecordingSupervisor {
    paths: Arc<PathStore>,
    mounts: Arc<MountRegistry>,
    hooks: Option<Arc<HookClient>>,
    policy: std::sync::RwLock<RecordingSection>,
    sessions: AsyncMutex<HashMap<String, Session>>,
    last_scan: Mutex<HashMap<String, u64>>,
    /// Paths where an operator explicitly stopped recording; auto_start
    /// must not immediately turn the recorder back on.
    suppressed: Mutex<HashSet<String>>,
}

impl RecordingSupervisor {
    pub fn new(
        config: Arc<Config>,
        paths: Arc<PathStore>,
        mounts: Arc<MountRegistry>,
        hooks: Option<Arc<HookClient>>,
    ) -> Arc<Self> {
        let policy = std::sync::RwLock::new(config.recording.clone());
        Arc::new(Self {
            policy,
            paths,
            mounts,
            hooks,
            sessions: AsyncMutex::new(HashMap::new()),
            last_scan: Mutex::new(HashMap::new()),
            suppressed: Mutex::new(HashSet::new()),
        })
    }

    /// Apply a reloaded `[recording]` section. Running sessions keep their
    /// original segment policy; new sessions use the new one.
    pub fn set_policy(&self, section: RecordingSection) {
        *self
            .policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = section;
    }

    fn policy(&self) -> RecordingSection {
        self.policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn status(&self) -> Vec<RecordingStatus> {
        // The async map is only locked briefly by the supervisor; for the
        // read-only status endpoint a blocking snapshot is acceptable.
        let sessions = match self.sessions.try_lock() {
            Ok(sessions) => sessions,
            Err(_) => return Vec::new(),
        };
        sessions
            .iter()
            .map(|(path, session)| RecordingStatus {
                path: path.clone(),
                active: session.recorder.is_some(),
                started_at: session.started_at,
                directory: session.directory.display().to_string(),
                segments: session
                    .recorder
                    .as_ref()
                    .map_or(0, |recorder| recorder.progress().segments),
                bytes: session
                    .recorder
                    .as_ref()
                    .map_or(0, |recorder| recorder.progress().bytes),
                dropped: session
                    .recorder
                    .as_ref()
                    .map_or(0, |recorder| recorder.progress().dropped_units),
                last_error: session.last_error.clone(),
            })
            .collect()
    }

    pub async fn start(&self, path: &str) -> Result<(), String> {
        validate_record_path(path)?;
        let handle = self
            .mounts
            .get_handle(&format!("/{path}"))
            .ok_or_else(|| format!("mount {path} is not online"))?;
        if !matches!(handle.mount().config, MountConfig::Publisher { .. }) {
            return Err(format!("mount {path} is not a publisher mount"));
        }
        let mut sessions = self.sessions.lock().await;
        self.suppressed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path);
        if sessions.contains_key(path) {
            return Ok(());
        }
        let session = self.start_session_locked(path, &handle, true).await?;
        sessions.insert(path.to_owned(), session);
        Ok(())
    }

    pub async fn stop(&self, path: &str) -> Result<(), String> {
        let session = {
            let mut sessions = self.sessions.lock().await;
            sessions.remove(path)
        };
        self.suppressed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.to_owned());
        let Some(session) = session else {
            return Ok(());
        };
        self.finish(session, path).await;
        Ok(())
    }

    pub async fn shutdown(&self) {
        let sessions: Vec<(String, Session)> = {
            let mut sessions = self.sessions.lock().await;
            sessions.drain().collect()
        };
        for (path, session) in sessions {
            self.finish(session, &path).await;
        }
    }

    /// Poll the registry and start/stop automatic recordings.
    pub async fn tick(&self) {
        let _ = self.paths.remove_expired();
        tracing::debug!(
            enabled = self.policy().enabled,
            auto_start = self.policy().auto_start,
            "recording tick"
        );
        let names: Vec<String> = self
            .mounts
            .paths()
            .iter()
            .map(|path| path.as_str().trim_start_matches('/').to_owned())
            .collect();
        for name in &names {
            let (_, rule) = self.paths.rule_for(name);
            self.paths.ensure_dynamic(name, &rule);
        }
        let mut stop: Vec<(String, Session)> = Vec::new();
        {
            let mut sessions = self.sessions.lock().await;
            for name in &names {
                let Some(handle) = self.mounts.get_handle(&format!("/{name}")) else {
                    continue;
                };
                let snapshot = handle.snapshot();
                let record = self.paths.resolve(name);
                let suppressed = self
                    .suppressed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(name);
                // A pull/proxy mount publishes through the shared frame bus
                // instead of a LocalPublisher, so `publisher_connected` stays
                // false; its upstream state is the equivalent signal.
                let proxy_streaming = matches!(
                    snapshot.proxy_state,
                    Some(rtflow::mount::upstream::ProxyState::Streaming)
                );
                let wanted = self.policy().enabled
                    && self.policy().auto_start
                    && !suppressed
                    && record.record
                    && (snapshot.publisher_connected || proxy_streaming);
                tracing::debug!(
                    path = %name,
                    record = record.record,
                    publisher = snapshot.publisher_connected,
                    proxy_streaming,
                    wanted,
                    "recording decision"
                );
                match (wanted, sessions.contains_key(name)) {
                    (true, false) => match self.start_session_locked(name, &handle, false).await {
                        Ok(session) => {
                            tracing::info!(
                                path = %name,
                                dir = %session.directory.display(),
                                "recording started"
                            );
                            sessions.insert(name.clone(), session);
                        }
                        Err(error) => {
                            tracing::warn!(path = %name, %error, "recording start failed")
                        }
                    },
                    (false, true) => {
                        if let Some(session) = sessions.remove(name) {
                            stop.push((name.clone(), session));
                        }
                    }
                    _ => {}
                }
            }
            // A mount that disappeared (publisher disconnect) must finalize
            // its recorder instead of leaving a .partial segment behind.
            let gone: Vec<String> = sessions
                .keys()
                .filter(|name| !names.contains(name))
                .cloned()
                .collect();
            for name in gone {
                self.suppressed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&name);
                if let Some(session) = sessions.remove(&name) {
                    stop.push((name, session));
                }
            }
        }
        for (name, session) in stop {
            self.finish(session, &name).await;
        }
    }

    async fn start_session_locked(
        &self,
        path: &str,
        handle: &MountHandle,
        manual: bool,
    ) -> Result<Session, String> {
        let description = handle.description();
        if description.tracks.is_empty() {
            return Err("mount has no tracks".into());
        }
        let has_video = description
            .tracks
            .iter()
            .any(|track| track.kind == rtflow::media::MediaKind::Video);
        let started_at = unix_now();
        let directory = recording_directory(&self.policy(), path, unix_millis());
        let recording_config = RecordingConfig {
            directory: directory.clone(),
            segment_duration: self.policy().segment_duration,
            max_disk_bytes: self.policy().max_disk_bytes,
            max_segments: 100_000,
            queue_capacity: 256,
            sync_on_finalize: true,
            ..RecordingConfig::default()
        };
        let recorder = SegmentRecorder::start(recording_config, description)
            .await
            .map_err(|error| error.to_string())?;
        let sink = recorder.sink();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (receiver, startup) = handle.subscribe_with_startup();
        let feed_cancel = cancel.clone();
        let feed_path = path.to_owned();
        let feed = tokio::spawn(async move {
            // A recording that starts mid-GOP begins with P-frames and no
            // SPS/PPS: its first VOD segment is undecodable and the fMP4 init
            // has an empty avcC. Wait for the next video keyframe instead.
            let mut started = !has_video;
            for unit in startup {
                if !started {
                    if !unit.is_keyframe
                        || !matches!(
                            unit.codec,
                            rtflow::media::Codec::H264 | rtflow::media::Codec::H265
                        )
                    {
                        continue;
                    }
                    started = true;
                }
                if sink.try_record((*unit).clone()) != rtflow::storage::RecordOutcome::Accepted {
                    tracing::debug!(path = %feed_path, "recording startup frame dropped");
                }
            }
            let mut receiver = receiver;
            loop {
                tokio::select! {
                    _ = feed_cancel.cancelled() => break,
                    result = receiver.recv() => match result {
                        Ok(unit) => {
                            if !started {
                                if !unit.is_keyframe || !matches!(unit.codec, rtflow::media::Codec::H264 | rtflow::media::Codec::H265) {
                                    continue;
                                }
                                started = true;
                            }
                            if sink.try_record((*unit).clone()) == rtflow::storage::RecordOutcome::QueueFull {
                                tracing::warn!(path = %feed_path, "recording queue full; dropped frame");
                            }
                        }
                        Err(rtflow::FrameRecvError::Lagged(count)) => {
                            tracing::warn!(path = %feed_path, count, "recording lagged");
                        }
                        Err(rtflow::FrameRecvError::Closed) => break,
                    },
                }
            }
        });
        Ok(Session {
            started_at,
            directory,
            cancel,
            feed,
            recorder: Some(recorder),
            segments: 0,
            bytes: 0,
            dropped: 0,
            last_error: None,
            manual,
        })
    }

    async fn finish(&self, mut session: Session, path: &str) {
        session.cancel.cancel();
        session.feed.abort();
        let _ = (&mut session.feed).await;
        if let Some(recorder) = session.recorder.take() {
            match recorder.shutdown().await {
                Ok(report) => {
                    self.record_report(path, &mut session, report);
                }
                Err(error) => {
                    session.last_error = Some(error.to_string());
                    tracing::warn!(path, %error, "recording shutdown failed");
                }
            }
        }
        if let Ok(catalog) = RecordingCatalog::scan(&session.directory, usize::MAX) {
            session.segments = catalog.segments.len() as u64;
            session.bytes = catalog.segments.iter().map(|segment| segment.bytes).sum();
        }
        if let Some(hooks) = &self.hooks {
            hooks.notify(HookRequest {
                event: HookKind::RecordDone,
                path: path.to_owned(),
                role: "record".into(),
                protocol: "file".into(),
                peer: None,
                key_id: None,
                stream_id: None,
                reason: session.last_error.clone(),
                timestamp: unix_now(),
            });
        }
        // Keep the finished status for the console for a short while.
        let mut last = self
            .last_scan
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        last.insert(path.to_owned(), unix_now());
        tracing::info!(
            path,
            segments = session.segments,
            bytes = session.bytes,
            manual = session.manual,
            "recording finalized"
        );
    }

    fn record_report(&self, path: &str, session: &mut Session, report: RecordingReport) {
        session.segments = report.finalized_segments;
        session.bytes = report.written_bytes;
        session.dropped = report.dropped_units;
        tracing::debug!(path, ?report, "recording report");
    }
}

fn validate_record_path(path: &str) -> Result<(), String> {
    crate::paths::validate_name(path).map_err(|error| error.to_string())
}

pub fn recording_directory(section: &RecordingSection, path: &str, started_at: u64) -> PathBuf {
    let mut directory = section.root.clone();
    for segment in path.trim_start_matches('/').split('/') {
        let safe: String = segment
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        directory.push(safe);
    }
    directory.push(started_at.to_string());
    directory
}

/// Recording directories under the configured root.
pub fn recording_directories(root: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    collect_directories(root, &mut directories, 0);
    directories
        .into_iter()
        .filter(|directory| {
            std::fs::read_dir(directory).is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "rtspseg")
                })
            })
        })
        .collect()
}

fn collect_directories(root: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_directories(&path, out, depth + 1);
            out.push(path);
        }
    }
}

/// Remove recording session directories older than `retention`.
pub fn prune_recordings(root: &Path, retention: Duration) -> usize {
    let cutoff = SystemTime::now()
        .checked_sub(retention)
        .unwrap_or(UNIX_EPOCH);
    let mut removed = 0;
    for directory in recording_directories(root) {
        let modified = directory.metadata().and_then(|meta| meta.modified()).ok();
        if modified.is_some_and(|modified| modified < cutoff)
            && std::fs::remove_dir_all(&directory).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
