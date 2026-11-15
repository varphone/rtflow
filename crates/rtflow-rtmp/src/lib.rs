//! RTMP (Real-Time Messaging Protocol) server support.
//!
//! Provides an RTMP server that accepts publish and play connections,
//! integrating with the shared mount registry for cross-protocol interop.
//!
//! # Features
//!
//! - Simple handshake (C0/C1/C2 + S0/S1/S2)
//! - Chunk stream protocol (fmt 0-3, configurable chunk size)
//! - AMF0 command encoding/decoding
//! - H.264/H.265 video + AAC audio via FLV tag format
//! - Publish: RTMP -> AccessUnit -> mount (playable via RTSP/HLS/FLV)
//! - Play: mount -> AccessUnit -> FLV tags -> RTMP (viewable via FFmpeg)
//!
//! # Example
//!
//! ```no_run
//! use rtflow_rtmp::RtmpServer;
//! use rtflow_mount::MountRegistry;
//! use std::sync::Arc;
//!
//! # async fn example() -> rtflow_core::error::Result<()> {
//! let mounts = Arc::new(MountRegistry::new(100));
//! let server = RtmpServer::bind("0.0.0.0:1935", mounts).await?;
//! server.run().await?;
//! # Ok(())
//! # }
//! ```
pub mod amf;
pub mod chunk;
pub mod command;
pub mod flv;
pub mod handshake;
pub mod session;

use session::{RtmpSessionConfig, serve_rtmp_session};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use rtflow_core::error::{Error, Result as RtflowResult};
use rtflow_mount::MountRegistry;

/// RTMP server configuration.
#[derive(Clone, Debug)]
pub struct RtmpConfig {
    /// Bind address.
    pub bind_addr: SocketAddr,
    /// Session configuration.
    pub session: RtmpSessionConfig,
    /// Maximum concurrent connections.
    pub max_connections: usize,
    /// Optional metrics registry for protocol-level counters.
    #[cfg(feature = "metrics")]
    pub metrics: Option<Arc<rtflow_core::metrics::MetricsRegistry>>,
}

impl Default for RtmpConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:1935".parse().unwrap(),
            session: RtmpSessionConfig::default(),
            max_connections: 100,
            #[cfg(feature = "metrics")]
            metrics: None,
        }
    }
}

/// RTMP server that accepts publish/play connections.
pub struct RtmpServer {
    listener: TcpListener,
    mounts: Arc<MountRegistry>,
    config: RtmpConfig,
    cancel: CancellationToken,
}

impl Drop for RtmpServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl RtmpServer {
    /// Bind an RTMP server to the given address.
    pub async fn bind(
        addr: impl tokio::net::ToSocketAddrs,
        mounts: Arc<MountRegistry>,
    ) -> RtflowResult<Self> {
        let listener = TcpListener::bind(&addr)
            .await
            .map_err(|e| Error::Rtmp(format!("bind: {e}")))?;

        let local_addr = listener.local_addr().ok();
        info!("RTMP server listening on {:?}", local_addr);

        Ok(Self {
            listener,
            mounts,
            config: RtmpConfig::default(),
            cancel: CancellationToken::new(),
        })
    }

    /// Create an RTMP server with full configuration.
    pub async fn with_config(config: RtmpConfig, mounts: Arc<MountRegistry>) -> RtflowResult<Self> {
        let listener = TcpListener::bind(config.bind_addr)
            .await
            .map_err(|e| Error::Rtmp(format!("bind {}: {e}", config.bind_addr)))?;

        info!("RTMP server listening on {}", config.bind_addr);

        Ok(Self {
            listener,
            mounts,
            config,
            cancel: CancellationToken::new(),
        })
    }

    /// Get the local address the server is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Get a cancellation token for graceful shutdown.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Run the server accept loop until cancelled.
    pub async fn run(self) -> RtflowResult<()> {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.config.max_connections));

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("RTMP server shutting down");
                    break;
                }
                result = self.listener.accept() => {
                    let (stream, peer) = match result {
                        Ok(conn) => conn,
                        Err(e) => {
                            warn!("RTMP accept error: {e}");
                            continue;
                        }
                    };
                    // RTMP carries many small control messages interleaved
                    // with media; Nagle would add up to 40ms of latency.
                    let _ = stream.set_nodelay(true);

                    let permit = match semaphore.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            debug!("RTMP: connection limit reached, rejecting {peer}");
                            continue;
                        }
                    };

                    let mounts = self.mounts.clone();
                    let config = self.config.session.clone();
                    let cancel = self.cancel.clone();
                    #[cfg(feature = "metrics")]
                    let metrics = self.config.metrics.clone();

                    tokio::spawn(async move {
                        let _permit = permit;
                        #[cfg(feature = "metrics")]
                        if let Some(ref m) = metrics {
                            use std::sync::atomic::Ordering;
                            m.rtmp_connections_total.fetch_add(1, Ordering::Relaxed);
                        }
                        #[cfg(feature = "metrics")]
                        let result = serve_rtmp_session(
                            stream,
                            mounts,
                            config,
                            cancel,
                            metrics.clone(),
                        )
                        .await;
                        #[cfg(not(feature = "metrics"))]
                        let result = serve_rtmp_session(stream, mounts, config, cancel).await;
                        if let Err(e) = result {
                            debug!("RTMP session error from {peer}: {e}");
                        }
                    });
                }
            }
        }

        Ok(())
    }

    /// Shutdown the server.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn server_binds_and_shuts_down() {
        let mounts = Arc::new(MountRegistry::new(10));
        let config = RtmpConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        };
        let server = RtmpServer::with_config(config, mounts).await.unwrap();
        let addr = server.local_addr().unwrap();
        assert_ne!(addr.port(), 0);

        let cancel = server.cancellation_token();
        let handle = tokio::spawn(server.run());

        // Give it a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        cancel.cancel();

        handle.await.unwrap().unwrap();
    }
}
