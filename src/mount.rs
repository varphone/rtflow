//! Protocol-agnostic mount registry and stream publication primitives.
//!
//! A *mount* is a named stream endpoint that can be published to and subscribed
//! from. All protocol servers (RTSP, RTMP, SRT, WebRTC) share the same mount
//! registry, enabling cross-protocol interop.
#[cfg(feature = "server")]
use crate::proxy::{KeyframeCache, ProxyState, UpstreamConfig};
#[cfg(feature = "file-storage")]
use crate::storage::VodSource;
use crate::{
    Error, Result,
    identity::MountId,
    media::{AccessUnit, MediaDescription},
    observe::EventSink,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// MountConfig
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum MountConfig {
    Local {
        description: MediaDescription,
    },
    Publisher {
        description: MediaDescription,
        allow_replace_after_disconnect: bool,
    },
    #[cfg(feature = "server")]
    Proxy {
        upstream: Box<UpstreamConfig>,
    },
    #[cfg(feature = "file-storage")]
    Vod {
        source: Arc<dyn VodSource>,
    },
}

// ---------------------------------------------------------------------------
// PushResult / StreamStats / MountSnapshot
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushResult {
    Accepted,
    DroppedNoSubscribers,
    DroppedQueueFull,
}

#[derive(Clone, Debug, Default)]
pub struct StreamStats {
    pub accepted_frames: u64,
    pub dropped_frames: u64,
    pub subscribers: usize,
    pub rtp_packets: u64,
    pub rtp_octets: u64,
    pub rtcp_sender_reports: u64,
    pub rtcp_receiver_reports: u64,
}

#[derive(Clone, Debug)]
pub struct MountSnapshot {
    pub subscriber_count: usize,
    pub publisher_connected: bool,
    #[cfg(feature = "server")]
    pub proxy_state: Option<ProxyState>,
    pub stats: StreamStats,
    pub startup_cache_frames: usize,
    pub startup_cache_bytes: usize,
}

// ---------------------------------------------------------------------------
// StreamCounters (internal)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct StreamCounters {
    pub accepted_frames: AtomicU64,
    pub dropped_frames: AtomicU64,
    pub rtp_packets: AtomicU64,
    pub rtp_octets: AtomicU64,
    pub rtcp_sender_reports: AtomicU64,
    pub rtcp_receiver_reports: AtomicU64,
}

impl StreamCounters {
    pub fn add_accepted(&self, value: u64) {
        self.accepted_frames.fetch_add(value, Ordering::Relaxed);
    }
    pub fn add_dropped(&self, value: u64) {
        self.dropped_frames.fetch_add(value, Ordering::Relaxed);
    }
    pub fn add_rtp(&self, packets: u64, octets: u64) {
        self.rtp_packets.fetch_add(packets, Ordering::Relaxed);
        self.rtp_octets.fetch_add(octets, Ordering::Relaxed);
    }
    pub fn add_sender_reports(&self, value: u64) {
        self.rtcp_sender_reports.fetch_add(value, Ordering::Relaxed);
    }
    pub fn add_receiver_reports(&self, value: u64) {
        self.rtcp_receiver_reports
            .fetch_add(value, Ordering::Relaxed);
    }
    pub fn snapshot(&self, subscribers: usize) -> StreamStats {
        StreamStats {
            accepted_frames: self.accepted_frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            subscribers,
            rtp_packets: self.rtp_packets.load(Ordering::Relaxed),
            rtp_octets: self.rtp_octets.load(Ordering::Relaxed),
            rtcp_sender_reports: self.rtcp_sender_reports.load(Ordering::Relaxed),
            rtcp_receiver_reports: self.rtcp_receiver_reports.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// Mount (internal shared state)
// ---------------------------------------------------------------------------

pub(crate) struct Mount {
    pub id: MountId,
    pub config: MountConfig,
    pub frames: broadcast::Sender<AccessUnit>,
    pub stats: StreamCounters,
    #[cfg(feature = "server")]
    pub startup_cache: Mutex<KeyframeCache>,
    pub bootstrap: Mutex<HashMap<u8, AccessUnit>>,
    pub event_sink: Arc<dyn EventSink>,
    pub cancel: CancellationToken,
    #[cfg(feature = "server")]
    pub proxy_running: AtomicBool,
    pub publishers: AtomicUsize,
    pub publisher_claimed: AtomicBool,
    pub max_queued_bytes_per_session: usize,
    /// SDP fmtp parameters received via ANNOUNCE, keyed by track_id.
    pub announce_fmtp: Mutex<HashMap<u8, Arc<str>>>,
}

impl Mount {
    pub fn publish_cached(
        &self,
        frame: AccessUnit,
    ) -> std::result::Result<usize, broadcast::error::SendError<AccessUnit>> {
        #[cfg(feature = "server")]
        {
            let mut cache = self
                .startup_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.push(&frame);
            drop(cache);
        }
        if frame.data.len() <= self.max_queued_bytes_per_session
            && (!matches!(
                frame.codec,
                crate::media::Codec::H264 | crate::media::Codec::H265
            ) || frame.is_keyframe)
        {
            self.bootstrap
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(frame.track_id, frame.clone());
        }
        self.frames.send(frame)
    }

    pub fn subscribe_with_startup(
        &self,
        track_id: u8,
    ) -> (broadcast::Receiver<AccessUnit>, Vec<AccessUnit>) {
        #[cfg(feature = "server")]
        {
            let cache = self
                .startup_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let receiver = self.frames.subscribe();
            let mut startup = cache.snapshot(track_id);
            let bytes = startup.iter().fold(0usize, |total, frame| {
                total.saturating_add(frame.data.len())
            });
            if bytes > self.max_queued_bytes_per_session {
                startup.clear();
            }
            if startup.is_empty()
                && let Some(frame) = self
                    .bootstrap
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&track_id)
                    .cloned()
            {
                startup.push(frame);
            }
            (receiver, startup)
        }
        #[cfg(not(feature = "server"))]
        {
            let _ = track_id;
            let receiver = self.frames.subscribe();
            let startup = Vec::new();
            (receiver, startup)
        }
    }

    pub fn subscribe_with_all_startup(&self) -> (broadcast::Receiver<AccessUnit>, Vec<AccessUnit>) {
        let receiver = self.frames.subscribe();
        #[cfg(feature = "server")]
        {
            let cache = self
                .startup_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut startup = cache.snapshot_all();
            let bytes = startup.iter().fold(0usize, |total, frame| {
                total.saturating_add(frame.data.len())
            });
            if bytes > self.max_queued_bytes_per_session {
                startup.clear();
            }
            (receiver, startup)
        }
        #[cfg(not(feature = "server"))]
        {
            (receiver, Vec::new())
        }
    }
}

// ---------------------------------------------------------------------------
// MountHandle (public API)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MountHandle {
    pub path: Arc<str>,
    pub description: MediaDescription,
    pub(crate) inner: Arc<Mount>,
}

impl MountHandle {
    pub fn publisher(&self) -> Result<LocalPublisher> {
        match &self.inner.config {
            MountConfig::Local { .. } | MountConfig::Publisher { .. } => {
                self.inner.publishers.fetch_add(1, Ordering::AcqRel);
                Ok(LocalPublisher {
                    inner: self.inner.clone(),
                    active: true,
                })
            }
            _ => Err(Error::Conflict("mount does not accept publishers".into())),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AccessUnit> {
        self.inner.frames.subscribe()
    }

    /// Subscribe to live frames together with a cached decodable stream start.
    pub fn subscribe_with_startup(&self) -> (broadcast::Receiver<AccessUnit>, Vec<AccessUnit>) {
        self.inner.subscribe_with_all_startup()
    }

    /// Get the broadcast sender for creating per-request subscribers.
    pub fn frame_sender(&self) -> &broadcast::Sender<AccessUnit> {
        &self.inner.frames
    }

    /// Get ANNOUNCE SDP fmtp parameters (snapshot at call time).
    pub fn announce_fmtp(&self) -> HashMap<u8, Arc<str>> {
        self.inner
            .announce_fmtp
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Cancellation token tied to the mount lifetime.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }

    /// Mount path, suitable as the canonical HTTP asset prefix.
    pub fn path(&self) -> &str {
        self.inner.id.as_str()
    }

    pub fn snapshot(&self) -> MountSnapshot {
        let subscribers = self.inner.frames.receiver_count();
        let s = self.inner.stats.snapshot(subscribers);
        #[cfg(feature = "server")]
        let (startup_cache_frames, startup_cache_bytes) = self
            .inner
            .startup_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .usage();
        #[cfg(not(feature = "server"))]
        let (startup_cache_frames, startup_cache_bytes) = (0, 0);
        MountSnapshot {
            subscriber_count: s.subscribers,
            publisher_connected: self.inner.publishers.load(Ordering::Relaxed) > 0,
            #[cfg(feature = "server")]
            proxy_state: match &self.inner.config {
                MountConfig::Proxy { upstream } => Some(upstream.state()),
                _ => None,
            },
            stats: s,
            startup_cache_frames,
            startup_cache_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// LocalPublisher
// ---------------------------------------------------------------------------

pub struct LocalPublisher {
    pub(crate) inner: Arc<Mount>,
    active: bool,
}

impl LocalPublisher {
    /// Number of subscribers currently attached to this local source.
    pub fn subscriber_count(&self) -> usize {
        self.inner.frames.receiver_count()
    }

    /// True after the mount is unregistered or its server begins shutdown.
    pub fn is_closed(&self) -> bool {
        self.inner.cancel.is_cancelled()
    }

    /// Push a frame without track validation (for protocol ingest like RTMP
    /// where the description may not yet be populated).
    pub fn push(&self, frame: AccessUnit) -> Result<PushResult> {
        if self.inner.frames.receiver_count() == 0 {
            self.inner.stats.add_dropped(1);
            return Ok(PushResult::DroppedNoSubscribers);
        }
        match self.inner.publish_cached(frame) {
            Ok(_) => {
                self.inner.stats.add_accepted(1);
                Ok(PushResult::Accepted)
            }
            Err(_) => {
                self.inner.stats.add_dropped(1);
                Ok(PushResult::DroppedQueueFull)
            }
        }
    }

    pub fn try_push(&self, frame: AccessUnit) -> Result<PushResult> {
        let description = description_for(&self.inner.config)?;
        let Some(track) = description.track(frame.track_id) else {
            return Err(Error::Media(format!(
                "mount has no track {}",
                frame.track_id
            )));
        };
        if track.codec != frame.codec {
            return Err(Error::Media(format!(
                "track {} expects {:?}, got {:?}",
                frame.track_id, track.codec, frame.codec
            )));
        }
        if self.inner.frames.receiver_count() == 0 {
            self.inner.stats.add_dropped(1);
            return Ok(PushResult::DroppedNoSubscribers);
        }
        match self.inner.publish_cached(frame) {
            Ok(_) => {
                self.inner.stats.add_accepted(1);
                Ok(PushResult::Accepted)
            }
            Err(_) => {
                self.inner.stats.add_dropped(1);
                Ok(PushResult::DroppedQueueFull)
            }
        }
    }

    pub async fn close(mut self) -> Result<()> {
        self.release();
        Ok(())
    }

    fn release(&mut self) {
        if self.active {
            self.inner.publishers.fetch_sub(1, Ordering::AcqRel);
            self.active = false;
        }
    }
}

impl Drop for LocalPublisher {
    fn drop(&mut self) {
        self.release();
    }
}

// ---------------------------------------------------------------------------
// MountRegistry
// ---------------------------------------------------------------------------

/// Shared mount registry accessible by all protocol servers.
pub struct MountRegistry {
    mounts: RwLock<HashMap<Arc<str>, Arc<Mount>>>,
    max_mounts: usize,
}

impl MountRegistry {
    pub fn new(max_mounts: usize) -> Self {
        Self {
            mounts: RwLock::new(HashMap::new()),
            max_mounts,
        }
    }

    /// Insert a pre-built mount. Returns error on conflict or limit.
    pub(crate) fn insert(&self, path: Arc<str>, mount: Arc<Mount>) -> Result<()> {
        let mut mounts = self
            .mounts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if mounts.contains_key(&path) {
            return Err(Error::Conflict(path.to_string()));
        }
        if mounts.len() >= self.max_mounts {
            return Err(Error::Limit("mounts"));
        }
        mounts.insert(path, mount);
        Ok(())
    }

    pub(crate) fn remove(&self, path: &str) -> Option<Arc<Mount>> {
        self.mounts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path)
    }

    pub(crate) fn get(&self, path: &str) -> Option<Arc<Mount>> {
        self.mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned()
    }

    /// Number of currently registered mounts.
    pub fn len(&self) -> usize {
        self.mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// List all registered mount paths.
    pub fn paths(&self) -> Vec<Arc<str>> {
        self.mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Find the mount whose path is the longest prefix of `requested`.
    pub(crate) fn find_longest_prefix(&self, requested: &str) -> Option<(Arc<str>, Arc<Mount>)> {
        self.mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(path, _)| {
                requested == path.as_ref()
                    || requested
                        .strip_prefix(path.as_ref())
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
            .max_by_key(|(path, _)| path.len())
            .map(|(path, mount)| (path.clone(), mount.clone()))
    }

    /// Register a new mount from a config. Returns a handle on success.
    pub fn register(&self, path: Arc<str>, config: MountConfig) -> Result<MountHandle> {
        let description = description_for(&config)?;
        let (tx, _) = broadcast::channel(1024);
        #[cfg(feature = "server")]
        let startup_cache = KeyframeCache::new(
            &description,
            crate::proxy::StartupCacheConfig::default(),
        )
        .unwrap_or_else(|_| {
            KeyframeCache::new(
                &MediaDescription {
                    session_name: "".into(),
                    tracks: Vec::new(),
                    #[cfg(feature = "multicast")]
                    connection: None,
                },
                crate::proxy::StartupCacheConfig {
                    enabled: false,
                    max_bytes: 0,
                    max_frames_per_track: 0,
                },
            )
            .expect("disabled cache always succeeds")
        });
        let mount = Arc::new(Mount {
            id: MountId::from(path.clone()),
            config,
            frames: tx,
            stats: StreamCounters::default(),
            #[cfg(feature = "server")]
            startup_cache: Mutex::new(startup_cache),
            bootstrap: Mutex::new(HashMap::new()),
            event_sink: Arc::new(crate::observe::NoopEventSink),
            cancel: CancellationToken::new(),
            #[cfg(feature = "server")]
            proxy_running: AtomicBool::new(false),
            publishers: AtomicUsize::new(0),
            publisher_claimed: AtomicBool::new(false),
            max_queued_bytes_per_session: 4 * 1024 * 1024,
            announce_fmtp: Mutex::new(HashMap::new()),
        });
        self.insert(path.clone(), mount.clone())?;
        Ok(MountHandle {
            path,
            description,
            inner: mount,
        })
    }

    /// Get a mount handle by path.
    pub fn get_handle(&self, path: &str) -> Option<MountHandle> {
        self.get(path).map(|mount| {
            let description = description_for(&mount.config).unwrap_or_else(|_| MediaDescription {
                session_name: path.into(),
                tracks: Vec::new(),
                #[cfg(feature = "multicast")]
                connection: None,
            });
            MountHandle {
                path: mount.id.as_str().into(),
                description,
                inner: mount,
            }
        })
    }

    /// Snapshot all registered mounts (path + stats).
    pub fn snapshots(&self) -> Vec<(Arc<str>, MountSnapshot)> {
        self.mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(path, mount)| {
                let handle = MountHandle {
                    path: path.clone(),
                    description: MediaDescription {
                        session_name: path.clone(),
                        tracks: Vec::new(),
                        #[cfg(feature = "multicast")]
                        connection: None,
                    },
                    inner: mount.clone(),
                };
                (path.clone(), handle.snapshot())
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// description_for helper
// ---------------------------------------------------------------------------

pub(crate) fn description_for(config: &MountConfig) -> Result<MediaDescription> {
    match config {
        MountConfig::Local { description } | MountConfig::Publisher { description, .. } => {
            Ok(description.clone())
        }
        #[cfg(feature = "server")]
        MountConfig::Proxy { upstream } => upstream.cached_description.clone().ok_or_else(|| {
            Error::Rtsp("proxy mount requires cached SDP description before serving".into())
        }),
        #[cfg(feature = "file-storage")]
        MountConfig::Vod { source } => Ok(source.description().clone()),
    }
}
