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
    Codec, Error, Result,
    identity::MountId,
    media::{AccessUnit, MediaDescription},
    observe::EventSink,
};
use bytes::Bytes;
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
    /// Unique RTSP play sessions (clients) on this mount.
    pub subscriber_count: usize,
    /// Current broadcast receivers (RTSP track subscriptions plus
    /// HTTP-FLV / WS-FLV / HLS subscribers). Publishers use the sum of
    /// both counters as their power gate so browser playback keeps the
    /// encoder running.
    pub broadcast_receivers: usize,
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
    /// Optional global registry to mirror frame counters into
    /// (`rtflow_frames_accepted_total` / `rtflow_frames_dropped_total`).
    #[cfg(feature = "metrics")]
    global: Option<Arc<crate::metrics::MetricsRegistry>>,
}

impl StreamCounters {
    /// Mirror frame counters into a global metrics registry (once per mount).
    #[cfg(feature = "metrics")]
    pub fn attach_global(&mut self, registry: Arc<crate::metrics::MetricsRegistry>) {
        self.global = Some(registry);
    }

    pub fn add_accepted(&self, value: u64) {
        self.accepted_frames.fetch_add(value, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        if let Some(global) = &self.global {
            global.frames_accepted.fetch_add(value, Ordering::Relaxed);
        }
    }
    pub fn add_dropped(&self, value: u64) {
        self.dropped_frames.fetch_add(value, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        if let Some(global) = &self.global {
            global.frames_dropped.fetch_add(value, Ordering::Relaxed);
        }
    }
    pub fn add_rtp(&self, packets: u64, octets: u64) {
        self.rtp_packets.fetch_add(packets, Ordering::Relaxed);
        self.rtp_octets.fetch_add(octets, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        if let Some(global) = &self.global {
            global
                .rtp_packets_sent
                .fetch_add(packets, Ordering::Relaxed);
            global.rtp_octets_sent.fetch_add(octets, Ordering::Relaxed);
        }
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
    /// Per-track bootstrap frame: fixed slots indexed by track_id (avoids
    /// hashing + allocating on every pushed frame).
    pub bootstrap: Mutex<[Option<AccessUnit>; 256]>,
    /// Expected codec per track id, for O(1) push validation without
    /// cloning the whole MediaDescription on every frame.
    pub codec_by_track: [Option<Codec>; 256],
    pub event_sink: Arc<dyn EventSink>,
    pub cancel: CancellationToken,
    #[cfg(feature = "server")]
    pub proxy_running: AtomicBool,
    pub publishers: AtomicUsize,
    pub publisher_claimed: AtomicBool,
    /// Unique active play sessions (clients) on this mount. Tracked by the
    /// RTSP session layer (one per connection, regardless of how many
    /// tracks it subscribed); `broadcast::Receiver` count counts track
    /// subscriptions instead and would show 2 for a single video+audio
    /// client.
    pub active_sessions: AtomicUsize,
    pub max_queued_bytes_per_session: usize,
    /// SDP fmtp parameters received via ANNOUNCE, keyed by track_id.
    pub announce_fmtp: Mutex<HashMap<u8, Arc<str>>>,
    /// Latest RTMP AVC/HEVC/AAC sequence headers published to this mount,
    /// keyed by track_id — so RTMP play sessions can initialize decoders
    /// before the first GOP arrives.
    pub rtmp_seq_headers: Mutex<HashMap<u8, Bytes>>,
}

/// Build the fixed codec lookup table from a media description.
pub(crate) fn codec_lookup_table(description: &MediaDescription) -> [Option<Codec>; 256] {
    let mut table = [None; 256];
    for track in &description.tracks {
        table[track.id as usize] = Some(track.codec);
    }
    table
}

/// Keys of frames covered by a startup snapshot.
///
/// Consumers that first drain the `startup` frames returned by
/// [`Mount::subscribe_with_startup`] and then read the broadcast receiver
/// will receive the same frames twice: the broadcast ring replays frames
/// that were sent before subscription. Skip any receiver frame whose
/// `(track_id, rtp_timestamp)` matches one of these keys.
pub fn startup_dedup_keys(startup: &[AccessUnit]) -> std::collections::HashSet<(u8, u32)> {
    startup
        .iter()
        .map(|unit| (unit.track_id, unit.rtp_timestamp))
        .collect()
}

/// How far back a new subscriber may start relative to the newest cached
/// frame. Bounding the startup window keeps the on-screen delay close to
/// the real-time edge instead of replaying the whole cache.
///
/// Low-latency trade-off: a fresh player first replays this window (paced
/// at media rate), so the on-screen delay equals roughly the window length
/// until it catches the live edge. 500ms keeps startup delay in the
/// sub-second range (VLC + zerolatency encoder total ~300-800ms); the
/// window still starts at the nearest keyframe so decoding is immediate.
pub const STARTUP_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

impl Mount {
    /// Cancellation token for this mount's data plane (cancelled on
    /// unregister or server shutdown).
    pub(crate) fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Publish a frame while maintaining the startup keyframe cache and the
    /// per-track bootstrap frame for new subscribers.
    pub fn publish_cached(
        &self,
        frame: AccessUnit,
    ) -> std::result::Result<usize, broadcast::error::SendError<AccessUnit>> {
        self.auto_inject_fmtp(&frame);
        // A cancelled mount (unregistered, or its server shutting down) must
        // not keep accepting data: publishers would silently write into a
        // detached mount while new registrations of the same path split the
        // stream.
        if self.cancel.is_cancelled() {
            return Err(broadcast::error::SendError(frame));
        }
        #[cfg(feature = "server")]
        {
            // Cache updates are best-effort: a concurrent subscriber holding
            // the lock must not block the publisher hot path, and dropping
            // one frame from the cache is harmless.
            if let Ok(mut cache) = self.startup_cache.try_lock() {
                cache.push(&frame);
            }
        }
        self.update_bootstrap(&frame);
        let result = self.frames.send(frame);
        if tracing::enabled!(tracing::Level::DEBUG) && self.frames.receiver_count() == 0 {
            tracing::debug!("mount: frames.send dropped (no receivers)");
        }
        result
    }

    /// Keep the latest (key)frame per track for RTP-Info/SDP timing even
    /// while no subscriber is attached: PLAY answers need a rtptime base
    /// the moment a client connects, before the first GOP lands in the
    /// startup cache.
    /// Auto-inject SDP fmtp from the first keyframe carrying parameter
    /// sets (hardware encoders like HiMPP often emit SPS/PPS separated
    /// from the IDR slices). DESCRIBE merges this into the SDP when the
    /// description has no explicit fmtp. Cheap: runs on keyframes only,
    /// once per track, and also while no subscriber is attached (the
    /// very first keyframe typically arrives before the first PLAY).
    #[cfg(feature = "server")]
    pub fn auto_inject_fmtp(&self, frame: &AccessUnit) {
        if !frame.is_keyframe {
            return;
        }
        let Some(codec) = self
            .codec_by_track
            .get(frame.track_id as usize)
            .copied()
            .flatten()
        else {
            return;
        };
        if codec != crate::media::Codec::H264 && codec != crate::media::Codec::H265 {
            return;
        }
        {
            let lock = self
                .announce_fmtp
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if lock.contains_key(&frame.track_id) {
                return;
            }
        }
        use crate::media::bitstream::{
            h264_fmtp, h264_parameter_sets, h265_fmtp, h265_parameter_sets,
        };
        let fmtp = match codec {
            crate::media::Codec::H264 => {
                let sets = h264_parameter_sets(&frame.data);
                match (sets.sps.as_deref(), sets.pps.as_deref()) {
                    (Some(sps), Some(pps)) => Some(h264_fmtp(sps, pps)),
                    _ => None,
                }
            }
            crate::media::Codec::H265 => {
                let sets = h265_parameter_sets(&frame.data);
                match (
                    sets.vps.as_deref(),
                    sets.sps.as_deref(),
                    sets.pps.as_deref(),
                ) {
                    (Some(vps), Some(sps), Some(pps)) => Some(h265_fmtp(vps, sps, pps)),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(fmtp) = fmtp {
            tracing::debug!("auto fmtp for track {}: {fmtp}", frame.track_id);
            self.announce_fmtp
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(frame.track_id, Arc::from(fmtp));
        }
    }

    pub fn update_bootstrap(&self, frame: &AccessUnit) {
        if frame.data.len() <= self.max_queued_bytes_per_session
            && (!matches!(
                frame.codec,
                crate::media::Codec::H264 | crate::media::Codec::H265
            ) || frame.is_keyframe)
            && let Ok(mut bootstrap) = self.bootstrap.try_lock()
        {
            bootstrap[frame.track_id as usize] = Some(frame.clone());
        }
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
            let mut bytes = startup.iter().fold(0usize, |total, frame| {
                total.saturating_add(frame.data.len())
            });
            if bytes > self.max_queued_bytes_per_session {
                // Roll off the oldest frames up to the limit instead of
                // clearing the whole snapshot (keep a decodable start).
                while bytes > self.max_queued_bytes_per_session && startup.len() > 1 {
                    bytes -= startup.remove(0).data.len();
                }
                while startup.first().is_some_and(|f| !f.is_keyframe) && startup.len() > 1 {
                    startup.remove(0);
                }
            }
            // Bound how far back a new subscriber starts: a snapshot that
            // reaches seconds into the past makes the on-screen delay equal
            // to the whole cached window. Keep at most STARTUP_WINDOW of
            // media, cutting at the nearest keyframe.
            if let Some(last) = startup.last() {
                let cutoff = last.pts.saturating_sub(STARTUP_WINDOW);
                match startup
                    .iter()
                    .position(|frame| frame.pts >= cutoff && frame.is_keyframe)
                {
                    Some(cut) => {
                        // Window contains a keyframe: start playback there
                        // (decodes immediately, replay ≤ STARTUP_WINDOW).
                        startup.drain(..cut);
                    }
                    None => {
                        // No keyframe inside the window (GOP > STARTUP_WINDOW):
                        // the current GOP start was missed. Like mainstream
                        // surveillance cameras, drop the snapshot and wait
                        // for the next IDR (the sender's awaiting_key gate
                        // handles that) instead of replaying from an old GOP
                        // start — the viewer would otherwise stay a full GOP
                        // behind the live edge (and the old start may already
                        // have been evicted, causing a black screen).
                        startup.clear();
                    }
                }
            }
            // Deliberately no bootstrap fallback here: an empty snapshot
            // means "wait for the next IDR" (sender's awaiting_key gate),
            // matching surveillance-camera startup. The bootstrap frame is
            // still used for RTP-Info rtptime so the PLAY answer always
            // carries a timing base.
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
            let mut bytes = startup.iter().fold(0usize, |total, frame| {
                total.saturating_add(frame.data.len())
            });
            if bytes > self.max_queued_bytes_per_session {
                // Roll off the oldest frames instead of clearing the snapshot.
                while bytes > self.max_queued_bytes_per_session && startup.len() > 1 {
                    bytes = bytes.saturating_sub(startup.remove(0).data.len());
                }
                while startup.first().is_some_and(|f| !f.is_keyframe) && startup.len() > 1 {
                    startup.remove(0);
                }
            }
            // Bound the startup window (see STARTUP_WINDOW).
            if let Some(last) = startup.last() {
                let cutoff = last.pts.saturating_sub(STARTUP_WINDOW);
                if let Some(cut) = startup
                    .iter()
                    .position(|frame| frame.pts >= cutoff && frame.is_keyframe)
                {
                    startup.drain(..cut);
                }
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
            MountConfig::Local { .. } => {
                self.inner.publishers.fetch_add(1, Ordering::AcqRel);
                Ok(LocalPublisher {
                    inner: self.inner.clone(),
                    active: true,
                })
            }
            MountConfig::Publisher { .. } => {
                // A claimed Publisher mount is exclusively owned by the
                // RTSP RECORD session: refuse extra publishers instead of
                // silently creating a second, interleaved stream.
                if self.inner.publisher_claimed.load(Ordering::Acquire) {
                    return Err(Error::Conflict(
                        "publisher is already claimed by an RTSP session".into(),
                    ));
                }
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

    /// Get a shared reference to the live announce_fmtp source.
    ///
    /// Unlike `announce_fmtp()` which returns a snapshot, this allows
    /// long-lived tasks to poll for updates as publishers ANNOUNCE.
    pub fn announce_fmtp_source(&self) -> Arc<Mount> {
        self.inner.clone()
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
        // Unique sessions, not track subscriptions: one client with a
        // video+audio pair counts once.
        let subscribers = self.inner.active_sessions.load(Ordering::Relaxed);
        // HTTP-FLV / WS-FLV / HLS subscribers hold a broadcast receiver
        // but never register an RTSP session; a publisher's power gate
        // (`subscriber_count + broadcast_receivers == 0` → stop pushing)
        // would otherwise stall browser playback after the startup
        // snapshot. RTSP track subscriptions are included here too.
        let broadcast_receivers = self.inner.frames.receiver_count();
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
            broadcast_receivers,
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
    /// Number of unique client sessions currently playing this mount.
    pub fn subscriber_count(&self) -> usize {
        self.inner.active_sessions.load(Ordering::Relaxed)
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
            // No subscribers: still keep the startup keyframe cache fresh
            // (plus the per-track bootstrap for the RTP-Info rtptime base).
            // Otherwise the snapshot a new PLAY sees is frozen at the last
            // disconnect — replaying it jumps straight from stale frames
            // to the live edge, which stalls VLC for seconds.
            self.inner.auto_inject_fmtp(&frame);
            #[cfg(feature = "server")]
            if let Ok(mut cache) = self.inner.startup_cache.try_lock() {
                cache.push(&frame);
            }
            self.inner.update_bootstrap(&frame);
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
        // O(1) codec validation via the fixed lookup table; no per-frame
        // MediaDescription clone + linear track search.
        let Some(expected) = self.inner.codec_by_track[frame.track_id as usize] else {
            return Err(Error::Media(format!(
                "mount has no track {}",
                frame.track_id
            )));
        };
        if expected != frame.codec {
            return Err(Error::Media(format!(
                "track {} expects {:?}, got {:?}",
                frame.track_id, expected, frame.codec
            )));
        }
        if self.inner.frames.receiver_count() == 0 {
            self.inner.stats.add_dropped(1);
            // No subscribers: still keep the startup keyframe cache fresh
            // (plus the per-track bootstrap for the RTP-Info rtptime base).
            // Otherwise the snapshot a new PLAY sees is frozen at the last
            // disconnect — replaying it jumps straight from stale frames
            // to the live edge, which stalls VLC for seconds.
            self.inner.auto_inject_fmtp(&frame);
            #[cfg(feature = "server")]
            if let Ok(mut cache) = self.inner.startup_cache.try_lock() {
                cache.push(&frame);
            }
            self.inner.update_bootstrap(&frame);
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
        let mounts = self
            .mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Exact match is the overwhelmingly common case: O(1) hash lookup
        // instead of a linear prefix scan over every registered mount.
        if let Some((path, mount)) = mounts.get_key_value(requested) {
            return Some((path.clone(), mount.clone()));
        }
        mounts
            .iter()
            .filter(|(path, _)| {
                requested
                    .strip_prefix(path.as_ref())
                    .is_some_and(|suffix| suffix.starts_with('/'))
            })
            .max_by_key(|(path, _)| path.len())
            .map(|(path, mount)| (path.clone(), mount.clone()))
    }

    /// Register a new mount from a config. Returns a handle on success.
    pub fn register(&self, path: Arc<str>, config: MountConfig) -> Result<MountHandle> {
        let description = description_for(&config)?;
        let (tx, _) = broadcast::channel(256);
        #[cfg(feature = "server")]
        let startup_cache =
            KeyframeCache::new(&description, crate::proxy::StartupCacheConfig::default())
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
            bootstrap: Mutex::new(std::array::from_fn(|_| None)),
            codec_by_track: codec_lookup_table(&description),
            event_sink: Arc::new(crate::observe::NoopEventSink),
            cancel: CancellationToken::new(),
            #[cfg(feature = "server")]
            proxy_running: AtomicBool::new(false),
            publishers: AtomicUsize::new(0),
            publisher_claimed: AtomicBool::new(false),
            active_sessions: AtomicUsize::new(0),
            max_queued_bytes_per_session: 4 * 1024 * 1024,
            announce_fmtp: Mutex::new(HashMap::new()),
            rtmp_seq_headers: Mutex::new(HashMap::new()),
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

    /// Remove a mount from the registry and cancel its data plane.
    ///
    /// Protocol servers (RTMP/SRT) must call this when the publishing
    /// session that created the mount ends, otherwise every distinct
    /// stream path permanently consumes one of the `max_mounts` slots.
    pub fn unregister(&self, path: &str) -> Option<MountHandle> {
        let removed = self.remove(path)?;
        removed.cancel.cancel();
        Some(MountHandle {
            path: removed.id.as_str().into(),
            description: description_for(&removed.config).unwrap_or_else(|_| MediaDescription {
                session_name: path.into(),
                tracks: Vec::new(),
                #[cfg(feature = "multicast")]
                connection: None,
            }),
            inner: removed,
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
