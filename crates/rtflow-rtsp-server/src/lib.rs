mod proxy_runtime;
mod session;
mod tasks;
mod timed_writer;
mod vod_runtime;
use session::serve_connection;
#[cfg(feature = "multicast")]
use tasks::{MediaCrypto, SenderInput, spawn_udp_sender_from};

// Re-export mount types for backward compatibility (deprecated in 0.3).
#[cfg(feature = "multicast")]
pub(crate) use rtflow_mount::Mount;
#[deprecated(
    since = "0.3.0",
    note = "use `rtflow::mount::{LocalPublisher, MountConfig, MountHandle, MountSnapshot, PushResult, StreamStats}`"
)]
pub use rtflow_mount::{
    LocalPublisher, MountConfig, MountHandle, MountSnapshot, PushResult, StreamStats,
};

use rtflow_core::{
    error::{Error, Operation, Result},
    identity::{MountId, SessionId},
    observe::{
        EventSink, FrameworkEvent, MountEvent, NoopEventSink, RejectReason, SecurityEvent,
        ServerEvent, SessionEvent, emit,
    },
};
use rtflow_mount::{MountOptions, MountRegistry, continuity::StartupCacheConfig};
use rtflow_rtsp::auth::{AllowAll, Authenticator};
use rtflow_rtsp_client::{ClientOptions, RtspClient};
use rtflow_runtime::{ShutdownReport, TaskSpec, TaskSupervisor, io::ControlConnection};
#[cfg(feature = "tls")]
use rtflow_security::TlsServerConfig;
use rtflow_security::{ConnectionRateLimiter, SecurityConfig};
#[cfg(feature = "multicast")]
use std::sync::atomic::AtomicU16;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    ops::RangeInclusive,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinHandle};
use tokio_util::sync::CancellationToken;

mod config;

pub use config::{ConfigError, ResourceLimits, ServerConfig, ServerConfigBuilder};
pub use rtflow_rtsp::range::TimelineMode;

struct State {
    mounts: Arc<MountRegistry>,
    config: ServerConfig,
    session_slots: Arc<Semaphore>,
    next_session: AtomicU64,
    cancel: CancellationToken,
    supervisor: TaskSupervisor,
    peer_sessions: Mutex<HashMap<IpAddr, usize>>,
    connection_rate_limiter: ConnectionRateLimiter,
    timeline_epoch: std::time::Instant,
}

struct PeerSessionLease {
    state: Weak<State>,
    peer: IpAddr,
}

impl Drop for PeerSessionLease {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let mut sessions = state
            .peer_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = sessions.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                sessions.remove(&self.peer);
            }
        }
    }
}

impl State {
    fn try_acquire_peer(self: &Arc<Self>, peer: IpAddr) -> Option<PeerSessionLease> {
        let mut sessions = self
            .peer_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = sessions.entry(peer).or_default();
        if *count >= self.config.resource_limits.max_sessions_per_ip {
            return None;
        }
        *count += 1;
        Some(PeerSessionLease {
            state: Arc::downgrade(self),
            peer,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionPhase {
    Init,
    Ready,
    Playing,
    Recording,
    Paused,
}

pub struct RtspServer {
    local_addr: SocketAddr,
    state: Arc<State>,
    accept_task: Option<JoinHandle<ShutdownReport>>,
}

impl RtspServer {
    pub async fn bind(config: ServerConfig) -> Result<Self> {
        let max_mounts = config.resource_limits.max_mounts;
        #[allow(unused_mut)] // only mutated by the metrics feature
        let mut mounts = MountRegistry::new(max_mounts);
        // R12: mirrors per-mount frame counters into the global metrics
        // registry so RTMP/SRT/gateway-registered mounts appear in /metrics
        // too, not just RtspServer mounts.
        #[cfg(feature = "metrics")]
        if let Some(registry_ref) = rtflow_core::metrics::registry_from_sink(&config.event_sink) {
            mounts.attach_global(registry_ref);
        }
        Self::bind_with(config, Arc::new(mounts)).await
    }

    /// Bind with an externally owned mount registry. Multi-protocol gateways
    /// pass their shared registry here so RTSP sees RTMP/SRT-mounted streams;
    /// the caller is responsible for any metrics attachment on that registry.
    pub async fn bind_with(config: ServerConfig, mounts: Arc<MountRegistry>) -> Result<Self> {
        config.validate()?;
        let listener = TcpListener::bind(config.listen).await?;
        let local_addr = listener.local_addr()?;
        let supervisor = TaskSupervisor::new(config.event_sink.clone());
        let connection_rate_limiter =
            ConnectionRateLimiter::new(config.security.connection_rate_limit)?;
        let state = Arc::new(State {
            mounts,
            session_slots: Arc::new(Semaphore::new(config.max_sessions)),
            next_session: AtomicU64::new(1),
            cancel: supervisor.cancellation_token(),
            supervisor,
            peer_sessions: Mutex::new(HashMap::new()),
            connection_rate_limiter,
            timeline_epoch: std::time::Instant::now(),
            config,
        });
        // Gateway-style registries may already hold proxy mounts before the
        // RTSP server binds; start their subscriber watchers now.
        for id in state.mounts.paths() {
            if let Some(handle) = state.mounts.get_handle(id.as_str())
                && matches!(handle.mount().config, MountConfig::Proxy { .. })
            {
                proxy_runtime::watch_proxy(
                    handle.mount().clone(),
                    state.cancel.clone(),
                    state.supervisor.clone(),
                    state.config.no_subscriber_grace,
                );
            }
        }
        emit(
            &state.config.event_sink,
            FrameworkEvent::Server(ServerEvent::Started { local_addr }),
        );
        let task_state = state.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_state.cancel.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, peer)) => {
                            if !task_state.config.security.ip_acl.permits(peer.ip()) {
                                emit(
                                    &task_state.config.event_sink,
                                    FrameworkEvent::Session(SessionEvent::Rejected {
                                        peer,
                                        reason: RejectReason::AccessDenied,
                                    }),
                                );
                                drop(stream);
                                continue;
                            }
                            if !task_state
                                .connection_rate_limiter
                                .admit(peer.ip(), std::time::Instant::now())
                            {
                                emit(
                                    &task_state.config.event_sink,
                                    FrameworkEvent::Security(SecurityEvent::RateLimited { peer }),
                                );
                                emit(
                                    &task_state.config.event_sink,
                                    FrameworkEvent::Session(SessionEvent::Rejected {
                                        peer,
                                        reason: RejectReason::RateLimited,
                                    }),
                                );
                                drop(stream);
                                continue;
                            }
                            let Ok(slot) = task_state.session_slots.clone().try_acquire_owned() else {
                                emit(
                                    &task_state.config.event_sink,
                                    FrameworkEvent::Session(SessionEvent::Rejected {
                                        peer,
                                        reason: RejectReason::GlobalLimit,
                                    }),
                                );
                                drop(stream);
                                continue;
                            };
                            let Some(peer_lease) = task_state.try_acquire_peer(peer.ip()) else {
                                emit(
                                    &task_state.config.event_sink,
                                    FrameworkEvent::Session(SessionEvent::Rejected {
                                        peer,
                                        reason: RejectReason::PeerLimit,
                                    }),
                                );
                                drop((stream, slot));
                                continue;
                            };
                            let id = SessionId::new(
                                task_state.next_session.fetch_add(1, Ordering::Relaxed),
                            );
                            let state = task_state.clone();
                            let sink = state.config.event_sink.clone();
                            #[cfg(feature = "tls")]
                            let tls = state.config.tls.clone();
                            task_state.supervisor.spawn(
                                TaskSpec::new(format!("rtsp-session-{id}"), Operation::ReadControl),
                                async move {
                                let _slot = slot;
                                let _peer_lease = peer_lease;
                                let started = std::time::Instant::now();
                                let local_addr = stream.local_addr()?;
                                stream.set_nodelay(true)?;
                                #[cfg(feature = "tls")]
                                let connection = if let Some(tls) = tls {
                                    let acceptor = tokio_rustls::TlsAcceptor::from(tls.config);
                                    match tokio::time::timeout(
                                        tls.handshake_timeout,
                                        acceptor.accept(stream),
                                    )
                                    .await
                                    {
                                        Ok(Ok(stream)) => ControlConnection::new(stream, local_addr, peer),
                                        result => {
                                            let message: Arc<str> = match result {
                                                Ok(Err(error)) => Arc::from(error.to_string()),
                                                Err(_) => Arc::from("TLS handshake timeout"),
                                                Ok(Ok(_)) => unreachable!(),
                                            };
                                            emit(
                                                &sink,
                                                FrameworkEvent::Security(
                                                    SecurityEvent::TlsHandshakeFailed {
                                                        peer,
                                                        error: message.clone(),
                                                    },
                                                ),
                                            );
                                            return Err(Error::Tls(message.to_string()));
                                        }
                                    }
                                } else {
                                    ControlConnection::new(stream, local_addr, peer)
                                };
                                #[cfg(not(feature = "tls"))]
                                let connection = ControlConnection::new(stream, local_addr, peer);
                                // Accepted is emitted only after the TLS handshake
                                // succeeded; a handshake failure would otherwise
                                // leak connections_active/sessions_active gauges.
                                emit(
                                    &sink,
                                    FrameworkEvent::Session(SessionEvent::Accepted { id, peer }),
                                );
                                let result = serve_connection(connection, peer, state).await;
                                emit(
                                    &sink,
                                    FrameworkEvent::Session(SessionEvent::Closed {
                                        id,
                                        peer,
                                        elapsed: started.elapsed(),
                                        error: result.as_ref().err().map(Error::code),
                                    }),
                                );
                                result
                            });
                        }
                        Err(error) => {
                            emit(
                                &task_state.config.event_sink,
                                FrameworkEvent::Server(ServerEvent::AcceptFailed {
                                    error: Arc::from(error.to_string()),
                                }),
                            );
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                }
            }
            task_state
                .supervisor
                .shutdown_after(task_state.config.shutdown_grace)
                .await
        });
        Ok(Self {
            local_addr,
            state,
            accept_task: Some(accept_task),
        })
    }
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The shared mount registry (for HTTP-FLV / WS-FLV / HLS servers that
    /// serve the same mounts).
    pub fn registry(&self) -> Arc<MountRegistry> {
        self.state.mounts.clone()
    }
    pub async fn register_mount(
        &self,
        path: impl Into<Arc<str>>,
        config: MountConfig,
    ) -> Result<MountHandle> {
        self.register_mount_with(path, config, MountOptions::default())
            .await
    }

    /// Register a mount with per-mount options (currently the live NPT
    /// timeline override, startup-cache and queue settings).
    pub async fn register_mount_with(
        &self,
        path: impl Into<Arc<str>>,
        mut config: MountConfig,
        options: MountOptions,
    ) -> Result<MountHandle> {
        let path = path.into();
        if !path.starts_with('/') {
            return Err(Error::InvalidConfig(
                "mount path must start with '/'".into(),
            ));
        }
        if self.state.mounts.len() >= self.state.config.resource_limits.max_mounts {
            return Err(Error::Limit("mounts"));
        }
        if let MountConfig::Proxy { upstream } = &mut config
            && upstream.cached_description.is_none()
        {
            let mut last_error = None;
            for endpoint in upstream.endpoints() {
                let mut builder = ClientOptions::builder()
                    .transport(endpoint.transport)
                    .request_timeout(endpoint.request_timeout)
                    .jitter(endpoint.jitter);
                if let Some(credentials) = endpoint.credentials {
                    builder = builder.credentials(credentials);
                }
                #[cfg(feature = "tls")]
                if let Some(tls) = endpoint.tls {
                    builder = builder.tls(tls);
                }
                let options = match builder.build() {
                    Ok(options) => options,
                    Err(_) => continue,
                };
                match RtspClient::describe(endpoint.url, options).await {
                    Ok(description) => {
                        upstream.cached_description = Some(description);
                        break;
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            if upstream.cached_description.is_none() {
                return Err(last_error.unwrap_or(Error::Limit("proxy upstreams")));
            }
        }
        let mut effective = options;
        if effective.consumer_queue_capacity.is_none() {
            effective.consumer_queue_capacity = Some(self.state.config.consumer_queue_capacity);
        }
        if effective.max_queued_bytes_per_session.is_none() {
            effective.max_queued_bytes_per_session = Some(
                self.state
                    .config
                    .resource_limits
                    .max_queued_bytes_per_session,
            );
        }
        if effective.event_sink.is_none() {
            effective.event_sink = Some(self.state.config.event_sink.clone());
        }
        if effective.startup_cache_config.is_none() {
            effective.startup_cache_config = Some(match &config {
                MountConfig::Proxy { upstream } => upstream.startup_cache,
                _ => self.state.config.startup_cache,
            });
        }
        if effective.timeline.is_none() {
            effective.timeline = Some(self.state.config.timeline);
        }
        if effective.cancel.is_none() {
            effective.cancel = Some(self.state.cancel.child_token());
        }
        let effective = effective;
        // One Mount construction path for every protocol server. This keeps
        // queue/cache/event/metrics wiring identical between RTSP, RTMP, SRT
        // and the gateway instead of maintaining a second copy here.
        let handle = self
            .state
            .mounts
            .register_with(path.clone(), config, effective)?;
        if matches!(handle.mount().config, MountConfig::Proxy { .. }) {
            crate::proxy_runtime::watch_proxy(
                handle.mount().clone(),
                self.state.cancel.clone(),
                self.state.supervisor.clone(),
                self.state.config.no_subscriber_grace,
            );
        }
        #[cfg(feature = "multicast")]
        let mount = handle.mount().clone();
        #[cfg(feature = "multicast")]
        let description = handle.description();
        emit(
            &self.state.config.event_sink,
            FrameworkEvent::Mount(MountEvent::Registered {
                id: MountId::from(path.clone()),
            }),
        );
        #[cfg(feature = "multicast")]
        if let Some(connection) = &description.connection
            && connection.address.is_multicast()
        {
            spawn_multicast_senders(
                mount.clone(),
                &description,
                connection.address,
                connection.port.unwrap_or(5004),
                connection.ttl.unwrap_or(1),
                self.state.config.udp_recv_buffer,
                self.state.config.rtcp_report_interval,
                self.state.config.rtx,
            )
            .await;
        }
        Ok(handle)
    }
    pub async fn unregister_mount(&self, path: &str) -> Result<()> {
        let mount = self
            .state
            .mounts
            .remove(path)
            .ok_or_else(|| Error::NotFound(path.into()))?;
        mount.cancel.cancel();
        emit(
            &self.state.config.event_sink,
            FrameworkEvent::Mount(MountEvent::Unregistered {
                id: MountId::from(path),
            }),
        );
        Ok(())
    }
    pub async fn shutdown(mut self) -> Result<()> {
        self.shutdown_with_report().await.map(drop)
    }

    pub async fn shutdown_with_report(&mut self) -> Result<ShutdownReport> {
        emit(
            &self.state.config.event_sink,
            FrameworkEvent::Server(ServerEvent::Stopping),
        );
        self.state.cancel.cancel();
        if let Some(task) = self.accept_task.take() {
            let report = task
                .await
                .map_err(|error| Error::Rtsp(format!("server supervisor failed: {error}")))?;
            self.state.mounts.cancel_all();
            emit(
                &self.state.config.event_sink,
                FrameworkEvent::Server(ServerEvent::Stopped),
            );
            return Ok(report);
        }
        Ok(ShutdownReport::default())
    }
}
impl Drop for RtspServer {
    fn drop(&mut self) {
        self.state.cancel.cancel();
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
        // The accept task used to own the bounded forced shutdown
        // (supervisor.shutdown_after); aborting it would silently bypass
        // shutdown_grace. Run the forced shutdown in the background so
        // sessions still get a bounded grace period and are then aborted.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let state = self.state.clone();
            handle.spawn(async move {
                let _ = state
                    .supervisor
                    .shutdown_after(state.config.shutdown_grace)
                    .await;
            });
        }
    }
}

#[cfg(feature = "multicast")]
#[allow(clippy::too_many_arguments)]
async fn spawn_multicast_senders(
    mount: Arc<Mount>,
    description: &rtflow_media::MediaDescription,
    group: IpAddr,
    port: u16,
    ttl: u8,
    recv_buffer: usize,
    report_interval: std::time::Duration,
    rtx_enabled: bool,
) {
    let rtx_configs = if rtx_enabled {
        rtflow_media::rtx_configs(description)
    } else {
        std::collections::HashMap::new()
    };
    for track in &description.tracks {
        let (receiver, startup) = mount.subscribe_with_startup(track.id);
        let bind_ip = match group {
            IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        };
        let pair = match rtflow_rtsp::transport::UdpPair::bind_with_recv_buffer(
            bind_ip,
            None,
            recv_buffer,
        )
        .await
        {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(%error, "multicast sender bind failed");
                continue;
            }
        };
        if let Err(error) = pair.set_multicast_ttl(ttl) {
            tracing::warn!(%error, "multicast TTL setup failed");
        }
        spawn_udp_sender_from(
            mount.clone(),
            SenderInput {
                receiver,
                startup,
                sequence: Arc::new(AtomicU16::new(0)),
                crypto: MediaCrypto::from_track(track).unwrap_or_default(),
            },
            pair,
            SocketAddr::new(group, port),
            SocketAddr::new(group, port.saturating_add(1)),
            track.clone(),
            None,
            report_interval,
            rtx_configs.get(&track.id).copied(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtflow_media::{Codec, MediaDescription, MediaKind, TrackDescription};
    use rtflow_rtsp_client::{ClientOptions, RtspClient};
    #[tokio::test]
    async fn describe_registered_mount() {
        let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let description = MediaDescription {
            session_name: Arc::from("test"),
            tracks: vec![TrackDescription {
                id: 0,
                kind: MediaKind::Video,
                codec: Codec::H264,
                clock_rate: 90_000,
                channels: None,
                payload_type: 96,
                fmtp: None,
                control: Arc::from("trackID=0"),
                width: None,
                height: None,
                frame_rate: None,
                crypto: None,
                direction: None,
                ptime: None,
                rtcp: None,
                bandwidth_as: None,
                rtx: None,
            }],
            connection: None,
        };
        server
            .register_mount(Arc::from("/stream"), MountConfig::Local { description })
            .await
            .unwrap();
        let url = url::Url::parse(&format!("rtsp://{}/stream", server.local_addr())).unwrap();
        let session = RtspClient::connect(url, ClientOptions::default())
            .await
            .unwrap();
        assert_eq!(session.description.tracks[0].codec, Codec::H264);
        server.shutdown().await.unwrap();
    }
}
