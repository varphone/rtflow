//! Multi-protocol gateway orchestration.
//!
//! The [`Gateway`] binds multiple protocol servers (RTSP, HTTP, RTMP, SRT, etc.)
//! to a shared [`MountRegistry`], enabling cross-protocol stream relay.
#[cfg(feature = "http")]
use crate::http::{HttpConfig, HttpServer};
use crate::mount::MountRegistry;
#[cfg(feature = "rtmp")]
use crate::rtmp::RtmpServer;
#[cfg(feature = "server")]
use crate::server::{RtspServer, ServerConfig};
#[cfg(feature = "srt")]
use crate::srt::SrtServer;
use crate::{Result, mount::MountHandle};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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
    #[cfg(feature = "rtmp")]
    rtmp_cancel: Option<CancellationToken>,
    #[cfg(feature = "rtmp")]
    rtmp_task: Option<tokio::task::JoinHandle<crate::Result<()>>>,
    #[cfg(feature = "srt")]
    srt_cancel: Option<CancellationToken>,
    #[cfg(feature = "srt")]
    srt_task: Option<tokio::task::JoinHandle<crate::Result<()>>>,
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
            #[cfg(feature = "rtmp")]
            rtmp_cancel: None,
            #[cfg(feature = "rtmp")]
            rtmp_task: None,
            #[cfg(feature = "srt")]
            srt_cancel: None,
            #[cfg(feature = "srt")]
            srt_task: None,
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

    /// Start an RTMP server sharing this gateway's mount registry.
    #[cfg(feature = "rtmp")]
    pub async fn start_rtmp(
        &mut self,
        addr: impl tokio::net::ToSocketAddrs,
    ) -> Result<std::net::SocketAddr> {
        let server = RtmpServer::bind(addr, self.mounts.clone()).await?;
        let local = server
            .local_addr()
            .map_err(|e| crate::Error::Rtmp(e.to_string()))?;
        self.rtmp_cancel = Some(server.cancellation_token());
        // Keep the task handle so shutdown can await it and so a
        // panicked/errored accept loop is observable instead of silent.
        self.rtmp_task = Some(tokio::spawn(server.run()));
        Ok(local)
    }

    /// Start an RTMP server with full configuration.
    #[cfg(feature = "rtmp")]
    pub async fn start_rtmp_with_config(
        &mut self,
        config: crate::rtmp::RtmpConfig,
    ) -> Result<std::net::SocketAddr> {
        let server = RtmpServer::with_config(config, self.mounts.clone()).await?;
        let local = server
            .local_addr()
            .map_err(|e| crate::Error::Rtmp(e.to_string()))?;
        self.rtmp_cancel = Some(server.cancellation_token());
        // Keep the task handle so shutdown can await it and so a
        // panicked/errored accept loop is observable instead of silent.
        self.rtmp_task = Some(tokio::spawn(server.run()));
        Ok(local)
    }

    /// Start an SRT server sharing this gateway's mount registry.
    #[cfg(feature = "srt")]
    pub async fn start_srt(
        &mut self,
        addr: impl tokio::net::ToSocketAddrs,
    ) -> Result<std::net::SocketAddr> {
        let server = SrtServer::bind(addr, self.mounts.clone()).await?;
        let local = server
            .local_addr()
            .map_err(|e| crate::Error::Srt(e.to_string()))?;
        self.srt_cancel = Some(server.cancellation_token());
        // Keep the task handle so shutdown can await it and so a
        // panicked/errored accept loop is observable instead of silent.
        self.srt_task = Some(tokio::spawn(server.run()));
        Ok(local)
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
            bootstrap: Mutex::new(std::array::from_fn(|_| None)),
            codec_by_track: crate::mount::codec_lookup_table(&description),
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
        #[cfg(feature = "rtmp")]
        if let Some(cancel) = self.rtmp_cancel.take() {
            cancel.cancel();
        }
        #[cfg(feature = "srt")]
        if let Some(cancel) = self.srt_cancel.take() {
            cancel.cancel();
        }
        #[cfg(feature = "rtmp")]
        if let Some(task) = self.rtmp_task.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
        #[cfg(feature = "srt")]
        if let Some(task) = self.srt_task.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
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
