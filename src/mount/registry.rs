use super::*;

// ---------------------------------------------------------------------------
// MountRegistry
// ---------------------------------------------------------------------------

/// Shared mount registry accessible by all protocol servers.
pub struct MountRegistry {
    mounts: RwLock<HashMap<Arc<str>, Arc<Mount>>>,
    max_mounts: usize,
    /// Global metrics registry mirrored by every mount created through
    /// this registry (R12: frame counters were previously only wired for
    /// mounts registered via RtspServer, leaving RTMP/SRT/gateway mounts
    /// invisible in /metrics).
    #[cfg(feature = "metrics")]
    global: Option<Arc<crate::core::metrics::MetricsRegistry>>,
}

/// Default per-consumer buffering limit (bytes) applied to every mount
/// unless overridden via [`MountOptions`] or the server/gateway config.
pub const DEFAULT_MAX_QUEUED_BYTES_PER_SESSION: usize = 4 * 1024 * 1024;

/// Per-mount tuning knobs for [`MountRegistry::register_with`].
///
/// All fields are optional; `None` falls back to the defaults that
/// [`MountRegistry::register`] historically used, so existing callers keep
/// identical behavior.
#[derive(Default, Clone)]
pub struct MountOptions {
    /// Capacity of the per-mount frame broadcast channel. Applies backpressure
    /// to publishers when every consumer is slower than the encoder.
    pub consumer_queue_capacity: Option<usize>,
    /// Per-consumer buffering limit in bytes; excess frames are dropped
    /// (with Lagged recovery) instead of growing memory without bound.
    pub max_queued_bytes_per_session: Option<usize>,
    /// Sink for mount lifecycle/stream events; `None` → no-op sink.
    pub event_sink: Option<Arc<dyn EventSink>>,
    /// Startup (keyframe) cache policy for proxy mounts; `None` → enabled
    /// with default limits.
    #[cfg(feature = "server")]
    pub startup_cache_config: Option<crate::mount::continuity::StartupCacheConfig>,
}

impl MountRegistry {
    pub fn new(max_mounts: usize) -> Self {
        Self {
            mounts: RwLock::new(HashMap::new()),
            max_mounts,
            #[cfg(feature = "metrics")]
            global: None,
        }
    }

    /// Mirror per-mount frame counters into a global metrics registry.
    /// Must be called before mounts are registered.
    #[cfg(feature = "metrics")]
    pub fn attach_global(&mut self, registry: Arc<crate::core::metrics::MetricsRegistry>) {
        self.global = Some(registry);
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
    #[allow(dead_code)] // used by the RTSP server
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
        let description = description_for(&config)?;
        let queue_capacity = options.consumer_queue_capacity.unwrap_or(256);
        let (tx, _) = broadcast::channel(queue_capacity);
        let (keyframe_requests, _) = broadcast::channel(16);
        #[cfg(feature = "server")]
        let startup_cache = match KeyframeCache::new(
            &description,
            options.startup_cache_config.unwrap_or_default(),
        ) {
            Ok(cache) => cache,
            Err(e) => {
                // Degenerate fallback: a disabled cache always validates.
                tracing::warn!(%e, "startup cache init failed; disabling");
                KeyframeCache::new(
                    &MediaDescription {
                        session_name: "".into(),
                        tracks: Vec::new(),
                        #[cfg(feature = "multicast")]
                        connection: None,
                    },
                    crate::mount::continuity::StartupCacheConfig {
                        enabled: false,
                        ..Default::default()
                    },
                )
                .expect("disabled startup cache always validates")
            }
        };
        let mount = Arc::new(Mount {
            id: MountId::from(path.clone()),
            config,
            frames: tx,
            keyframe_requests,
            stats: {
                #[cfg_attr(not(feature = "metrics"), allow(unused_mut))]
                let mut counters = StreamCounters::default();
                #[cfg(feature = "metrics")]
                if let Some(global) = &self.global {
                    counters.attach_global(global.clone());
                } else if let Some(sink) = &options.event_sink {
                    // Gateway-style registries carry metrics through the
                    // event sink instead of a registry-global handle.
                    if let Some(registry) = crate::core::metrics::registry_from_sink(sink) {
                        counters.attach_global(registry);
                    }
                }
                counters
            },
            #[cfg(feature = "server")]
            startup_cache: Mutex::new(startup_cache),
            bootstrap: Mutex::new(std::array::from_fn(|_| None)),
            codec_by_track: codec_lookup_table(&description),
            event_sink: options
                .event_sink
                .unwrap_or_else(|| Arc::new(crate::observe::NoopEventSink)),
            cancel: CancellationToken::new(),
            #[cfg(feature = "server")]
            proxy_running: AtomicBool::new(false),
            publishers: AtomicUsize::new(0),
            publisher_claimed: AtomicBool::new(false),
            active_sessions: AtomicUsize::new(0),
            max_queued_bytes_per_session: options
                .max_queued_bytes_per_session
                .unwrap_or(DEFAULT_MAX_QUEUED_BYTES_PER_SESSION),
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
