use super::*;

pub(crate) struct Mount {
    pub id: MountId,
    pub config: MountConfig,
    pub frames: broadcast::Sender<AccessUnit>,
    /// PLI/FIR/NACK feedback forwarded to local sources (track id).
    pub keyframe_requests: broadcast::Sender<u8>,
    /// Optional per-mount live NPT timeline override.
    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub timeline: Option<crate::rtsp::range::TimelineMode>,
    pub stats: StreamCounters,
    #[cfg(feature = "server")]
    pub startup_cache: Mutex<KeyframeCache>,
    /// Per-track bootstrap frame: fixed slots indexed by track_id (avoids
    /// hashing + allocating on every pushed frame).
    pub bootstrap: Mutex<[Option<AccessUnit>; 256]>,
    /// Expected codec per track id, for O(1) push validation without
    /// cloning the whole MediaDescription on every frame.
    pub codec_by_track: [Option<Codec>; 256],
    /// Read by feature-gated adapters (metrics exporter, RTMP/WebRTC).
    #[allow(dead_code)]
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
    /// Read by the RTMP play path (feature-gated).
    #[allow(dead_code)]
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
/// `Mount::subscribe_with_startup` and then read the broadcast receiver
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
    /// Ask a local source to emit a keyframe on `track_id`.
    pub(crate) fn request_keyframe(&self, track_id: u8) {
        let _ = self.keyframe_requests.send(track_id);
    }

    /// Cancellation token for this mount's data plane (cancelled on
    /// unregister or server shutdown).
    #[allow(dead_code)] // used by the RTSP server session layer
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
        if !matches!(
            codec,
            crate::media::Codec::H264 | crate::media::Codec::H265 | crate::media::Codec::Opus
        ) {
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
            // RFC 7587: same parameters the WebRTC path announces.
            crate::media::Codec::Opus => Some("minptime=10;useinbandfec=1".to_string()),
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

    #[allow(dead_code)] // used by RTSP sessions when the server feature is on
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
            #[allow(unreachable_patterns)] // only reachable with proxy/VOD mounts
            _ => Err(Error::Conflict("mount does not accept publishers".into())),
        }
    }

    /// Request a keyframe from a local source (RTCP PLI/FIR/NACK action).
    pub fn request_keyframe(&self, track_id: u8) {
        self.inner.request_keyframe(track_id);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AccessUnit> {
        self.inner.frames.subscribe()
    }

    /// Subscribe to live frames together with a cached decodable stream start.
    #[allow(dead_code)] // used by RTSP sessions and HLS/HTTP-FLV adapters
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
    #[cfg(feature = "dash")]
    pub(crate) fn announce_fmtp_source(&self) -> Arc<Mount> {
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
