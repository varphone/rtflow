//! Multi-protocol gateway orchestration.
//!
//! The [`Gateway`] binds multiple protocol servers (RTSP, HTTP, RTMP, SRT, etc.)
//! to a shared [`MountRegistry`], enabling cross-protocol stream relay.
pub mod protocol;

#[cfg(feature = "http")]
use crate::http::{HttpConfig, HttpServer};
use crate::mount::{MountConfig, MountOptions, MountRegistry};
#[cfg(feature = "rtmp")]
use crate::rtmp::RtmpServer;
#[cfg(feature = "server")]
use crate::server::{RtspServer, ServerConfig};
#[cfg(feature = "srt")]
use crate::srt::SrtServer;
use crate::{Result, mount::MountHandle};
use std::sync::Arc;
#[cfg(any(feature = "rtmp", feature = "srt"))]
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
        let server = RtspServer::bind_with(config, self.mounts.clone()).await?;
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
    ///
    /// This delegates to the single `Mount` construction path
    /// ([`MountRegistry::register_with`]) so gateway mounts get identical
    /// wiring (stats, lifecycle, queue policy) to protocol-server mounts.
    pub fn register_local_mount(
        &self,
        path: Arc<str>,
        description: crate::media::MediaDescription,
        event_sink: Arc<dyn crate::observe::EventSink>,
        consumer_queue_capacity: usize,
        max_queued_bytes: usize,
    ) -> Result<MountHandle> {
        self.mounts.register_with(
            path,
            MountConfig::Local { description },
            MountOptions {
                consumer_queue_capacity: Some(consumer_queue_capacity),
                max_queued_bytes_per_session: Some(max_queued_bytes),
                event_sink: Some(event_sink),
                // Gateway local streams serve from the bootstrap keyframe
                // snapshot; the RTSP-proxy startup cache is not used.
                #[cfg(feature = "server")]
                startup_cache_config: Some(crate::mount::continuity::StartupCacheConfig {
                    enabled: false,
                    ..Default::default()
                }),
                timeline: None,
                cancel: None,
            },
        )
    }

    /// Shutdown all running protocol servers.
    #[cfg_attr(not(any(feature = "rtmp", feature = "srt")), allow(unused_mut))]
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
        self.mounts.cancel_all();
        Ok(())
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
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
            task.abort();
        }
        #[cfg(feature = "srt")]
        if let Some(task) = self.srt_task.take() {
            task.abort();
        }
        self.mounts.cancel_all();
    }
}
