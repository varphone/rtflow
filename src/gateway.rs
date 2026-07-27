//! Multi-protocol gateway orchestration.
//!
//! The [`Gateway`] binds multiple protocol servers (RTSP, HTTP, RTMP, etc.)
//! to a shared [`MountRegistry`], enabling cross-protocol stream relay.
use crate::mount::MountRegistry;
use crate::{Result, mount::MountHandle};
#[cfg(feature = "http")]
use crate::http::{HttpConfig, HttpServer};
#[cfg(feature = "server")]
use crate::server::{RtspServer, ServerConfig};
use std::sync::Arc;

/// Unified multi-protocol streaming gateway.
///
/// All protocol servers share the same mount registry so that a stream
/// published via one protocol (e.g. RTMP) can be played via another
/// (e.g. RTSP, HLS, HTTP-FLV).
pub struct Gateway {
    mounts: Arc<MountRegistry>,
    #[cfg(feature = "server")]
    rtsp: Option<RtspServer>,
    #[cfg(feature = "http")]
    http: Option<HttpServer>,
}

impl Gateway {
    /// Create a gateway with the given maximum mount capacity.
    pub fn new(max_mounts: usize) -> Self {
        Self {
            mounts: Arc::new(MountRegistry::new(max_mounts)),
            #[cfg(feature = "server")]
            rtsp: None,
            #[cfg(feature = "http")]
            http: None,
        }
    }

    /// Access the shared mount registry.
    pub fn mounts(&self) -> &Arc<MountRegistry> {
        &self.mounts
    }

    /// Start an RTSP server bound to the given config, sharing this gateway's
    /// mount registry.
    #[cfg(feature = "server")]
    pub async fn start_rtsp(&mut self, config: ServerConfig) -> Result<()> {
        let server = RtspServer::bind(config).await?;
        self.rtsp = Some(server);
        Ok(())
    }

    /// Start an HTTP server for HLS/FLV/DASH endpoints.
    #[cfg(feature = "http")]
    pub async fn start_http(&mut self, config: HttpConfig) -> Result<()> {
        let server = HttpServer::bind(config).await?;
        self.http = Some(server);
        Ok(())
    }

    /// Get a reference to the running RTSP server, if any.
    #[cfg(feature = "server")]
    pub fn rtsp(&self) -> Option<&RtspServer> {
        self.rtsp.as_ref()
    }

    /// Get a reference to the running HTTP server, if any.
    #[cfg(feature = "http")]
    pub fn http(&self) -> Option<&HttpServer> {
        self.http.as_ref()
    }

    /// Register a mount on the shared registry (protocol-agnostic).
    ///
    /// For RTSP-specific mount registration (with proxy DESCRIBE resolution),
    /// use `RtspServer::register_mount` directly.
    pub fn register_local_mount(
        &self,
        path: Arc<str>,
        description: crate::media::MediaDescription,
        event_sink: Arc<dyn crate::observe::EventSink>,
        consumer_queue_capacity: usize,
        max_queued_bytes: usize,
    ) -> Result<MountHandle> {
        use crate::identity::MountId;
        use crate::mount::{Mount, MountConfig};
        use std::collections::HashMap;
        use std::sync::Mutex;

        let (tx, _) = tokio::sync::broadcast::channel(consumer_queue_capacity);
        let mount = Arc::new(Mount {
            id: MountId::from(path.clone()),
            config: MountConfig::Local {
                description: description.clone(),
            },
            frames: tx,
            stats: crate::mount::StreamCounters::default(),
            #[cfg(feature = "server")]
            startup_cache: Mutex::new(crate::proxy::KeyframeCache::new(
                &description,
                crate::proxy::StartupCacheConfig {
                    enabled: false,
                    ..Default::default()
                },
            )?),
            bootstrap: Mutex::new(HashMap::new()),
            event_sink,
            cancel: tokio_util::sync::CancellationToken::new(),
            #[cfg(feature = "server")]
            proxy_running: std::sync::atomic::AtomicBool::new(false),
            publishers: std::sync::atomic::AtomicUsize::new(0),
            publisher_claimed: std::sync::atomic::AtomicBool::new(false),
            max_queued_bytes_per_session: max_queued_bytes,
            announce_fmtp: Mutex::new(HashMap::new()),
        });
        self.mounts.insert(path.clone(), mount.clone())?;
        Ok(MountHandle {
            path,
            description,
            inner: mount,
        })
    }

    /// Shutdown all running protocol servers.
    pub async fn shutdown(mut self) -> Result<()> {
        #[cfg(feature = "server")]
        if let Some(rtsp) = self.rtsp.take() {
            rtsp.shutdown().await?;
        }
        #[cfg(feature = "http")]
        if let Some(http) = self.http.take() {
            http.shutdown();
        }
        Ok(())
    }
}
