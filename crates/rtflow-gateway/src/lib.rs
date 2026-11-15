//! Multi-protocol gateway orchestration.
//!
//! The [`Gateway`] binds multiple protocol servers (RTSP, HTTP, RTMP, SRT, etc.)
//! to a shared [`MountRegistry`], enabling cross-protocol stream relay.
pub mod protocol;

use rtflow_core::error::Result;
use rtflow_http::{HttpConfig, HttpServer};
use rtflow_mount::MountHandle;
use rtflow_mount::{MountConfig, MountOptions, MountRegistry};
use rtflow_rtmp::RtmpServer;
use rtflow_rtsp_server::{RtspServer, ServerConfig};
use rtflow_srt::SrtServer;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Unified multi-protocol streaming gateway.
///
/// All protocol servers share the same mount registry so that a stream
/// published via one protocol (e.g. RTMP) can be played via another
/// (e.g. RTSP, HLS, HTTP-FLV).
pub struct Gateway {
    mounts: Arc<MountRegistry>,
    rtsp: Option<RtspServer>,
    http: Option<HttpServer>,
    rtmp_cancel: Option<CancellationToken>,
    rtmp_task: Option<tokio::task::JoinHandle<rtflow_core::error::Result<()>>>,
    srt_cancel: Option<CancellationToken>,
    srt_task: Option<tokio::task::JoinHandle<rtflow_core::error::Result<()>>>,
}

impl Gateway {
    /// Create a gateway with the given maximum mount capacity.
    pub fn new(max_mounts: usize) -> Self {
        Self {
            mounts: Arc::new(MountRegistry::new(max_mounts)),
            rtsp: None,
            http: None,
            rtmp_cancel: None,
            rtmp_task: None,
            srt_cancel: None,
            srt_task: None,
        }
    }

    /// Access the shared mount registry.
    pub fn mounts(&self) -> &Arc<MountRegistry> {
        &self.mounts
    }

    /// Start an RTSP server bound to the given config, sharing this gateway's
    /// mount registry.
    pub async fn start_rtsp(&mut self, config: ServerConfig) -> Result<()> {
        let server = RtspServer::bind_with(config, self.mounts.clone()).await?;
        self.rtsp = Some(server);
        Ok(())
    }

    /// Start an HTTP server for HLS/FLV/DASH endpoints.
    pub async fn start_http(&mut self, config: HttpConfig) -> Result<()> {
        let server = HttpServer::bind(config).await?;
        self.http = Some(server);
        Ok(())
    }

    /// Local address of the HTTP server, when started.
    pub fn http_addr(&self) -> Option<std::net::SocketAddr> {
        self.http.as_ref().map(HttpServer::local_addr)
    }

    /// Start an RTMP server sharing this gateway's mount registry.
    pub async fn start_rtmp(
        &mut self,
        addr: impl tokio::net::ToSocketAddrs,
    ) -> Result<std::net::SocketAddr> {
        let server = RtmpServer::bind(addr, self.mounts.clone()).await?;
        let local = server
            .local_addr()
            .map_err(|e| rtflow_core::error::Error::Rtmp(e.to_string()))?;
        self.rtmp_cancel = Some(server.cancellation_token());
        // Keep the task handle so shutdown can await it and so a
        // panicked/errored accept loop is observable instead of silent.
        self.rtmp_task = Some(tokio::spawn(server.run()));
        Ok(local)
    }

    /// Start an RTMP server with full configuration.
    pub async fn start_rtmp_with_config(
        &mut self,
        config: rtflow_rtmp::RtmpConfig,
    ) -> Result<std::net::SocketAddr> {
        let server = RtmpServer::with_config(config, self.mounts.clone()).await?;
        let local = server
            .local_addr()
            .map_err(|e| rtflow_core::error::Error::Rtmp(e.to_string()))?;
        self.rtmp_cancel = Some(server.cancellation_token());
        // Keep the task handle so shutdown can await it and so a
        // panicked/errored accept loop is observable instead of silent.
        self.rtmp_task = Some(tokio::spawn(server.run()));
        Ok(local)
    }

    /// Start an SRT server sharing this gateway's mount registry.
    pub async fn start_srt(
        &mut self,
        addr: impl tokio::net::ToSocketAddrs,
    ) -> Result<std::net::SocketAddr> {
        let server = SrtServer::bind(addr, self.mounts.clone()).await?;
        let local = server
            .local_addr()
            .map_err(|e| rtflow_core::error::Error::Srt(e.to_string()))?;
        self.srt_cancel = Some(server.cancellation_token());
        // Keep the task handle so shutdown can await it and so a
        // panicked/errored accept loop is observable instead of silent.
        self.srt_task = Some(tokio::spawn(server.run()));
        Ok(local)
    }

    /// Get a reference to the running RTSP server, if any.
    pub fn rtsp(&self) -> Option<&RtspServer> {
        self.rtsp.as_ref()
    }

    /// Get a reference to the running HTTP server, if any.
    pub fn http(&self) -> Option<&HttpServer> {
        self.http.as_ref()
    }

    /// Register a mount on the shared registry (protocol-agnostic).
    ///
    /// For RTSP-specific mount registration (with proxy DESCRIBE resolution),
    /// use `RtspServer::register_mount` directly.
    ///
    /// This delegates to the single `Mount` construction path
    /// ([`MountRegistry::register_with`]) so gateway mounts get identical
    /// wiring (stats, lifecycle, queue policy) to protocol-server mounts.
    pub fn register_local_mount(
        &self,
        path: Arc<str>,
        description: rtflow_media::MediaDescription,
        event_sink: Arc<dyn rtflow_core::observe::EventSink>,
        consumer_queue_capacity: usize,
        max_queued_bytes: usize,
    ) -> Result<MountHandle> {
        self.mounts.register_with(
            path,
            MountConfig::Local { description },
            MountOptions::builder()
                .consumer_queue_capacity(consumer_queue_capacity)
                .max_queued_bytes_per_session(max_queued_bytes)
                .event_sink(event_sink)
                // Gateway local streams serve from the bootstrap keyframe
                // snapshot; the RTSP-proxy startup cache is not used.
                .startup_cache_config(rtflow_mount::continuity::StartupCacheConfig {
                    enabled: false,
                    ..Default::default()
                })
                .build(),
        )
    }

    /// Shutdown all running protocol servers.
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(cancel) = self.rtmp_cancel.take() {
            cancel.cancel();
        }
        if let Some(cancel) = self.srt_cancel.take() {
            cancel.cancel();
        }
        if let Some(task) = self.rtmp_task.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
        if let Some(task) = self.srt_task.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        }
        if let Some(rtsp) = self.rtsp.take() {
            rtsp.shutdown().await?;
        }
        if let Some(http) = self.http.take() {
            http.shutdown();
        }
        self.mounts.cancel_all();
        Ok(())
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if let Some(cancel) = self.rtmp_cancel.take() {
            cancel.cancel();
        }
        if let Some(cancel) = self.srt_cancel.take() {
            cancel.cancel();
        }
        if let Some(task) = self.rtmp_task.take() {
            task.abort();
        }
        if let Some(task) = self.srt_task.take() {
            task.abort();
        }
        if let Some(http) = self.http.take() {
            http.shutdown();
        }
        self.mounts.cancel_all();
    }
}
