use super::*;

// ---------------------------------------------------------------------------
// MountRegistry
// ---------------------------------------------------------------------------

/// Shared mount registry accessible by all protocol servers.
pub struct MountRegistry {
    mounts: RwLock<HashMap<MountId, Arc<Mount>>>,
    max_mounts: usize,
    /// Global metrics registry mirrored by every mount created through
    /// this registry (R12: frame counters were previously only wired for
    /// mounts registered via RtspServer, leaving RTMP/SRT/gateway mounts
    /// invisible in /metrics).
    global: Option<Arc<rtflow_core::metrics::MetricsRegistry>>,
}

/// Default per-consumer buffering limit (bytes) applied to every mount
/// unless overridden via [`MountOptions`] or the server/gateway config.
pub const DEFAULT_MAX_QUEUED_BYTES_PER_SESSION: usize = 4 * 1024 * 1024;
/// Hard upper bound for the per-mount broadcast ring. A configuration typo
/// must not allocate an unbounded queue.
pub const MAX_CONSUMER_QUEUE_CAPACITY: usize = 4096;

/// Per-mount tuning knobs for [`MountRegistry::register_with`].
///
/// All fields are optional; `None` falls back to the defaults that
/// [`MountRegistry::register`] historically used, so existing callers keep
/// identical behavior.
#[derive(Default, Clone)]
#[non_exhaustive]
pub struct MountOptions {
    /// Capacity (frames) of the per-mount broadcast ring. Slow consumers
    /// lag and recover; the broadcaster never blocks the encoder.
    pub consumer_queue_capacity: Option<usize>,
    /// Frame-size budget used by senders and startup snapshots. This is not
    /// a full broadcast-ring byte budget yet; size the ring together with
    /// the expected frame size.
    pub max_queued_bytes_per_session: Option<usize>,
    /// Sink for mount lifecycle/stream events; `None` → no-op sink.
    pub event_sink: Option<Arc<dyn EventSink>>,
    /// Startup (keyframe) cache policy for every mount type; `None` uses
    /// the server default.
    pub startup_cache_config: Option<crate::continuity::StartupCacheConfig>,
    /// Per-mount override of the server's live NPT timeline mode.
    pub timeline: Option<rtflow_rtsp::range::TimelineMode>,
    /// Parent cancellation token. Server-owned mounts pass a child token so
    /// `RtspServer::shutdown` also stops file demuxers/proxies; standalone
    /// registries may leave this `None`.
    pub cancel: Option<CancellationToken>,
}

impl std::fmt::Debug for MountOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("MountOptions");
        debug
            .field("consumer_queue_capacity", &self.consumer_queue_capacity)
            .field(
                "max_queued_bytes_per_session",
                &self.max_queued_bytes_per_session,
            )
            .field("event_sink", &self.event_sink.as_ref().map(|_| "<sink>"))
            .field("timeline", &self.timeline)
            .field("cancel", &self.cancel.is_some());
        debug.field("startup_cache_config", &self.startup_cache_config);
        debug.finish()
    }
}

pub struct MountOptionsBuilder {
    options: MountOptions,
}

impl MountOptions {
    pub fn builder() -> MountOptionsBuilder {
        MountOptionsBuilder {
            options: MountOptions::default(),
        }
    }
}

impl MountOptionsBuilder {
    pub fn consumer_queue_capacity(mut self, value: usize) -> Self {
        self.options.consumer_queue_capacity = Some(value);
        self
    }

    pub fn max_queued_bytes_per_session(mut self, value: usize) -> Self {
        self.options.max_queued_bytes_per_session = Some(value);
        self
    }

    pub fn event_sink(mut self, value: Arc<dyn EventSink>) -> Self {
        self.options.event_sink = Some(value);
        self
    }

    pub fn startup_cache_config(mut self, value: crate::continuity::StartupCacheConfig) -> Self {
        self.options.startup_cache_config = Some(value);
        self
    }

    pub fn timeline(mut self, value: rtflow_rtsp::range::TimelineMode) -> Self {
        self.options.timeline = Some(value);
        self
    }

    pub fn cancel(mut self, value: CancellationToken) -> Self {
        self.options.cancel = Some(value);
        self
    }

    pub fn build(self) -> MountOptions {
        self.options
    }
}

impl MountRegistry {
    pub fn new(max_mounts: usize) -> Self {
        Self {
            mounts: RwLock::new(HashMap::new()),
            max_mounts,
            global: None,
        }
    }

    /// Mirror per-mount frame counters into a global metrics registry.
    /// Must be called before mounts are registered.
    pub fn attach_global(&mut self, registry: Arc<rtflow_core::metrics::MetricsRegistry>) {
        self.global = Some(registry);
    }

    /// Insert a pre-built mount. Returns error on conflict or limit.
    #[doc(hidden)]
    pub fn insert(&self, path: Arc<str>, mount: Arc<Mount>) -> Result<()> {
        let id = MountId::from(path);
        let mut mounts = self
            .mounts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if mounts.contains_key(&id) {
            return Err(Error::Conflict(id.to_string()));
        }
        if mounts.len() >= self.max_mounts {
            return Err(Error::Limit("mounts"));
        }
        mounts.insert(id, mount);
        Ok(())
    }

    #[doc(hidden)]
    pub fn remove(&self, path: &str) -> Option<Arc<Mount>> {
        self.mounts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&MountId::new(path))
    }

    /// Shared mount state for `path` (control-plane access).
    #[doc(hidden)]
    pub fn get(&self, path: &str) -> Option<Arc<Mount>> {
        self.mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&MountId::new(path))
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

    /// Cancel every mount's data plane and drop the registry entries.
    /// Protocol servers call this on shutdown; the mount handles held by
    /// callers become detached but their broadcast receivers close when
    /// those handles are dropped.
    pub fn cancel_all(&self) {
        let mut mounts = self
            .mounts
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for mount in mounts.values() {
            mount.cancel.cancel();
        }
        mounts.clear();
    }

    /// List all registered mounts in stable path order.
    pub fn paths(&self) -> Vec<MountId> {
        let mut paths = self
            .mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        paths
    }

    /// Find the mount whose path is the longest prefix of `requested`.
    #[allow(dead_code)] // used by the RTSP server
    /// Longest registered path that prefixes `requested`.
    #[doc(hidden)]
    pub fn find_longest_prefix(&self, requested: &str) -> Option<(MountId, Arc<Mount>)> {
        let mounts = self
            .mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Exact match is the overwhelmingly common case: O(1) hash lookup
        // instead of a linear prefix scan over every registered mount.
        if let Some((path, mount)) = mounts.get_key_value(&MountId::new(requested)) {
            return Some((path.clone(), mount.clone()));
        }
        mounts
            .iter()
            .filter(|(path, _)| {
                requested
                    .strip_prefix(path.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
            })
            .max_by_key(|(path, _)| path.as_str().len())
            .map(|(path, mount)| (path.clone(), mount.clone()))
    }

    /// Register a new mount from a config with default options. Returns a
    /// handle on success.
    pub fn register(&self, path: Arc<str>, config: MountConfig) -> Result<MountHandle> {
        self.register_with(path, config, MountOptions::default())
    }

    /// Register a new mount from a config plus per-mount tuning options
    /// (queue capacity, event sink, startup cache policy). Returns a
    /// handle on success.
    ///
    /// This is the single construction path for `Mount`: protocol servers
    /// (RTSP via `register_mount`, RTMP, SRT) and the multi-protocol
    /// gateway all funnel through it, so every mount gets identical
    /// wiring for stats, startup cache and lifecycle.
    pub fn register_with(
        &self,
        path: Arc<str>,
        config: MountConfig,
        options: MountOptions,
    ) -> Result<MountHandle> {
        if !path.starts_with('/') {
            return Err(Error::InvalidConfig(
                "mount path must start with '/'".into(),
            ));
        }
        let description = description_for(&config)?;
        let queue_capacity = options
            .consumer_queue_capacity
            .unwrap_or(256)
            .clamp(1, MAX_CONSUMER_QUEUE_CAPACITY);
        let max_bytes = options
            .max_queued_bytes_per_session
            .unwrap_or(DEFAULT_MAX_QUEUED_BYTES_PER_SESSION);
        let frames = FrameBus::new(queue_capacity, max_bytes);
        let (keyframe_requests, _) = broadcast::channel(16);
        let (nack_requests, _) = broadcast::channel(64);
        let startup_cache = KeyframeCache::new(
            &description,
            options.startup_cache_config.unwrap_or_default(),
        )?;
        let mount = Arc::new(Mount {
            id: MountId::from(path.clone()),
            config,
            frames,
            consumer_queue_capacity: queue_capacity,
            keyframe_requests,
            nack_requests,
            timeline: options.timeline,
            stats: {
                #[cfg_attr(not(feature = "metrics"), allow(unused_mut))]
                let mut counters = StreamCounters::default();
                if let Some(global) = &self.global {
                    counters.attach_global(global.clone());
                } else if let Some(sink) = &options.event_sink {
                    // Gateway-style registries carry metrics through the
                    // event sink instead of a registry-global handle.
                    if let Some(registry) = rtflow_core::metrics::registry_from_sink(sink) {
                        counters.attach_global(registry);
                    }
                }
                counters
            },
            description: std::sync::RwLock::new(description.clone()),
            startup_cache: Mutex::new(startup_cache),
            startup_cache_config: options.startup_cache_config.unwrap_or_default(),
            bootstrap: Mutex::new(std::array::from_fn(|_| None)),
            codec_by_track: std::sync::RwLock::new(codec_lookup_table(&description)),
            event_sink: options
                .event_sink
                .unwrap_or_else(|| Arc::new(rtflow_core::observe::NoopEventSink)),
            cancel: options.cancel.unwrap_or_default(),
            proxy_running: AtomicBool::new(false),
            publishers: AtomicUsize::new(0),
            publisher_claimed: AtomicBool::new(false),
            active_sessions: AtomicUsize::new(0),
            max_queued_bytes_per_session: max_bytes,
            announce_fmtp: Mutex::new(HashMap::new()),
            rtmp_seq_headers: Mutex::new(HashMap::new()),
        });
        self.insert(path.clone(), mount.clone())?;
        Ok(MountHandle {
            path: MountId::from(path),
            description,
            inner: mount,
        })
    }

    /// Get a mount handle by path.
    pub fn get_handle(&self, path: &str) -> Option<MountHandle> {
        self.get(path).map(|mount| {
            let description = mount.description_snapshot();
            MountHandle {
                path: mount.id.clone(),
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
        tracing::info!(path, "mount unregistered");
        Some(MountHandle {
            path: removed.id.clone(),
            description: description_for(&removed.config).unwrap_or_else(|_| MediaDescription {
                session_name: path.into(),
                tracks: Vec::new(),
                connection: None,
            }),
            inner: removed,
        })
    }

    /// Snapshot all registered mounts (identity + stats).
    pub fn snapshots(&self) -> Vec<(MountId, MountSnapshot)> {
        let mut snapshots = self
            .mounts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(path, mount)| {
                let handle = MountHandle {
                    path: path.clone(),
                    description: MediaDescription {
                        session_name: path.as_str().into(),
                        tracks: Vec::new(),
                        connection: None,
                    },
                    inner: mount.clone(),
                };
                (path.clone(), handle.snapshot())
            })
            .collect::<Vec<_>>();
        snapshots.sort_by(|(left, _), (right, _)| left.as_str().cmp(right.as_str()));
        snapshots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str) -> rtflow_media::MediaDescription {
        rtflow_media::MediaDescription::new(
            path,
            vec![rtflow_media::TrackDescription::h264_video(
                0,
                &[],
                &[],
                640,
                360,
                30,
            )],
        )
    }

    #[test]
    fn longest_prefix_respects_path_boundaries() {
        let registry = MountRegistry::new(4);
        registry
            .register(
                Arc::from("/cam"),
                MountConfig::Local {
                    description: local("cam"),
                },
            )
            .unwrap();
        registry
            .register(
                Arc::from("/cam/main"),
                MountConfig::Local {
                    description: local("main"),
                },
            )
            .unwrap();
        let (path, _) = registry
            .find_longest_prefix("/cam/main/track0")
            .expect("prefix");
        assert_eq!(path.as_str(), "/cam/main");
        assert!(registry.find_longest_prefix("/camera").is_none());
    }

    #[test]
    fn paths_are_sorted_and_cancel_all_clears() {
        let registry = MountRegistry::new(4);
        for path in ["/b", "/a"] {
            registry
                .register(
                    Arc::from(path),
                    MountConfig::Local {
                        description: local(path),
                    },
                )
                .unwrap();
        }
        assert_eq!(
            registry
                .paths()
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>(),
            vec!["/a", "/b"]
        );
        let handle = registry.get_handle("/a").unwrap();
        registry.cancel_all();
        assert!(handle.cancellation_token().is_cancelled());
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn dropping_mount_closes_frame_subscribers() {
        let registry = MountRegistry::new(1);
        let handle = registry
            .register(
                Arc::from("/close"),
                MountConfig::Local {
                    description: local("close"),
                },
            )
            .unwrap();
        let mut receiver = handle.subscribe();
        drop(handle);
        drop(registry);
        assert!(matches!(
            receiver.recv().await,
            Err(crate::FrameRecvError::Closed)
        ));
    }

    #[test]
    fn queue_capacity_is_clamped() {
        let registry = MountRegistry::new(1);
        let handle = registry
            .register_with(
                Arc::from("/capacity"),
                MountConfig::Local {
                    description: local("capacity"),
                },
                MountOptions::builder()
                    .consumer_queue_capacity(usize::MAX)
                    .build(),
            )
            .unwrap();
        assert_eq!(
            handle.snapshot().consumer_queue_capacity,
            MAX_CONSUMER_QUEUE_CAPACITY
        );
    }

    #[test]
    fn oversized_frame_is_dropped_before_broadcast() {
        let registry = MountRegistry::new(1);
        let handle = registry
            .register_with(
                Arc::from("/oversized"),
                MountConfig::Local {
                    description: local("oversized"),
                },
                MountOptions::builder()
                    .max_queued_bytes_per_session(4)
                    .build(),
            )
            .unwrap();
        let publisher = handle.publisher().unwrap();
        let frame = rtflow_media::AccessUnit {
            nal_offsets: None,
            track_id: 0,
            codec: rtflow_media::Codec::H264,
            pts: std::time::Duration::ZERO,
            dts: None,
            rtp_timestamp: 0,
            is_keyframe: true,
            data: bytes::Bytes::from_static(b"12345"),
        };
        assert_eq!(publisher.try_push(frame).unwrap(), PushResult::Dropped);
        assert_eq!(handle.snapshot().stats.dropped_frames, 1);
    }

    #[test]
    fn mount_options_builder_sets_values() {
        let options = MountOptions::builder()
            .consumer_queue_capacity(32)
            .max_queued_bytes_per_session(1024)
            .timeline(rtflow_rtsp::range::TimelineMode::Absolute {
                offset: std::time::Duration::from_secs(10),
            })
            .build();
        assert_eq!(options.consumer_queue_capacity, Some(32));
        assert_eq!(options.max_queued_bytes_per_session, Some(1024));
        assert!(matches!(
            options.timeline,
            Some(rtflow_rtsp::range::TimelineMode::Absolute { .. })
        ));
    }
}
