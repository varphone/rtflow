mod proxy_runtime;
mod session;
mod tasks;
#[cfg(feature = "file-storage")]
mod vod_runtime;
use session::serve_connection;

#[cfg(feature = "tls")]
use crate::security::TlsServerConfig;
#[cfg(feature = "file-storage")]
use crate::storage::VodSource;
use crate::{
    Error, Result,
    auth::{AllowAll, Authenticator},
    client::{ClientOptions, RtspClient},
    error::Operation,
    identity::{MountId, SessionId},
    media::{AccessUnit, MediaDescription},
    observe::{
        EventSink, FrameworkEvent, MountEvent, NoopEventSink, RejectReason, SecurityEvent,
        ServerEvent, SessionEvent, emit,
    },
    proxy::{KeyframeCache, ProxyState, StartupCacheConfig, UpstreamConfig},
    runtime::{ShutdownReport, TaskSpec, TaskSupervisor, io::ControlConnection},
    security::{ConnectionRateLimiter, SecurityConfig},
};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    ops::RangeInclusive,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{RwLock, Semaphore, broadcast},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub session_timeout: Duration,
    pub max_rtsp_header_bytes: usize,
    pub max_rtsp_body_bytes: usize,
    pub max_sessions: usize,
    pub udp_port_range: Option<RangeInclusive<u16>>,
    pub consumer_queue_capacity: usize,
    pub no_subscriber_grace: Duration,
    pub shutdown_grace: Duration,
    pub authenticator: Arc<dyn Authenticator>,
    pub resource_limits: ResourceLimits,
    pub event_sink: Arc<dyn EventSink>,
    pub security: SecurityConfig,
    #[cfg(feature = "tls")]
    pub tls: Option<TlsServerConfig>,
}
impl ServerConfig {
    pub fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            session_timeout: Duration::from_secs(60),
            max_rtsp_header_bytes: 16 * 1024,
            max_rtsp_body_bytes: 256 * 1024,
            max_sessions: 256,
            udp_port_range: None,
            consumer_queue_capacity: 4096,
            no_subscriber_grace: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(5),
            authenticator: Arc::new(AllowAll),
            resource_limits: ResourceLimits::default(),
            event_sink: Arc::new(NoopEventSink),
            security: SecurityConfig::default(),
            #[cfg(feature = "tls")]
            tls: None,
        }
    }

    pub fn builder(listen: SocketAddr) -> ServerConfigBuilder {
        ServerConfigBuilder {
            config: Self::new(listen),
        }
    }

    pub fn validate(&self) -> std::result::Result<(), ConfigError> {
        let positive = [
            (self.max_rtsp_header_bytes, "RTSP header bytes"),
            (self.max_rtsp_body_bytes, "RTSP body bytes"),
            (self.max_sessions, "RTSP sessions"),
            (self.consumer_queue_capacity, "consumer queue capacity"),
            (self.resource_limits.max_sessions_per_ip, "sessions per IP"),
            (self.resource_limits.max_mounts, "mounts"),
            (
                self.resource_limits.max_tracks_per_session,
                "tracks per session",
            ),
            (
                self.resource_limits.max_interleaved_frame_bytes,
                "interleaved frame bytes",
            ),
            (
                self.resource_limits.max_queued_bytes_per_session,
                "queued bytes per session",
            ),
        ];
        if let Some((_, name)) = positive.into_iter().find(|(value, _)| *value == 0) {
            return Err(ConfigError::Zero(name));
        }
        if self.session_timeout == Duration::ZERO {
            return Err(ConfigError::Zero("session timeout"));
        }
        if self.shutdown_grace == Duration::ZERO {
            return Err(ConfigError::Zero("shutdown grace"));
        }
        if self
            .udp_port_range
            .as_ref()
            .is_some_and(|range| range.start() >= range.end())
        {
            return Err(ConfigError::Invalid(
                "UDP port range must contain an RTP/RTCP pair",
            ));
        }
        if let Some(rate_limit) = self.security.connection_rate_limit {
            rate_limit
                .validate()
                .map_err(|_| ConfigError::Invalid("invalid connection rate limit"))?;
        }
        #[cfg(feature = "tls")]
        if self
            .tls
            .as_ref()
            .is_some_and(|tls| tls.handshake_timeout == Duration::ZERO)
        {
            return Err(ConfigError::Zero("TLS handshake timeout"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ResourceLimits {
    pub max_sessions_per_ip: usize,
    pub max_mounts: usize,
    pub max_tracks_per_session: usize,
    pub max_interleaved_frame_bytes: usize,
    pub max_queued_bytes_per_session: usize,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_sessions_per_ip: 32,
            max_mounts: 1_024,
            max_tracks_per_session: 16,
            max_interleaved_frame_bytes: 65_536,
            max_queued_bytes_per_session: 4 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigError {
    Zero(&'static str),
    Invalid(&'static str),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zero(name) => write!(formatter, "{name} must be greater than zero"),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<ConfigError> for Error {
    fn from(error: ConfigError) -> Self {
        match error {
            ConfigError::Zero(name) => Self::Limit(name),
            ConfigError::Invalid(message) => Self::Rtsp(message.into()),
        }
    }
}

pub struct ServerConfigBuilder {
    config: ServerConfig,
}

impl ServerConfigBuilder {
    pub fn session_timeout(mut self, value: Duration) -> Self {
        self.config.session_timeout = value;
        self
    }

    pub fn max_sessions(mut self, value: usize) -> Self {
        self.config.max_sessions = value;
        self
    }

    pub fn consumer_queue_capacity(mut self, value: usize) -> Self {
        self.config.consumer_queue_capacity = value;
        self
    }

    pub fn udp_port_range(mut self, value: RangeInclusive<u16>) -> Self {
        self.config.udp_port_range = Some(value);
        self
    }

    pub fn no_subscriber_grace(mut self, value: Duration) -> Self {
        self.config.no_subscriber_grace = value;
        self
    }

    pub fn shutdown_grace(mut self, value: Duration) -> Self {
        self.config.shutdown_grace = value;
        self
    }

    pub fn authenticator(mut self, value: Arc<dyn Authenticator>) -> Self {
        self.config.authenticator = value;
        self
    }

    pub fn resource_limits(mut self, value: ResourceLimits) -> Self {
        self.config.resource_limits = value;
        self
    }

    pub fn event_sink(mut self, value: Arc<dyn EventSink>) -> Self {
        self.config.event_sink = value;
        self
    }

    pub fn security(mut self, value: SecurityConfig) -> Self {
        self.config.security = value;
        self
    }

    #[cfg(feature = "tls")]
    pub fn tls(mut self, value: TlsServerConfig) -> Self {
        self.config.tls = Some(value);
        self
    }

    pub fn build(self) -> std::result::Result<ServerConfig, ConfigError> {
        self.config.validate()?;
        Ok(self.config)
    }
}

#[derive(Clone)]
pub enum MountConfig {
    Local {
        description: MediaDescription,
    },
    Publisher {
        description: MediaDescription,
        allow_replace_after_disconnect: bool,
    },
    Proxy {
        upstream: Box<UpstreamConfig>,
    },
    #[cfg(feature = "file-storage")]
    Vod {
        source: Arc<dyn VodSource>,
    },
}
#[derive(Clone)]
pub struct MountHandle {
    pub path: Arc<str>,
    pub description: MediaDescription,
    inner: Arc<Mount>,
}
pub struct LocalPublisher {
    inner: Arc<Mount>,
    active: bool,
}
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
    pub subscriber_count: usize,
    pub publisher_connected: bool,
    pub proxy_state: Option<ProxyState>,
    pub stats: StreamStats,
    pub startup_cache_frames: usize,
    pub startup_cache_bytes: usize,
}
struct Mount {
    id: MountId,
    config: MountConfig,
    frames: broadcast::Sender<AccessUnit>,
    stats: StreamCounters,
    startup_cache: Mutex<KeyframeCache>,
    bootstrap: Mutex<HashMap<u8, AccessUnit>>,
    event_sink: Arc<dyn EventSink>,
    cancel: CancellationToken,
    proxy_running: AtomicBool,
    publishers: AtomicUsize,
    publisher_claimed: AtomicBool,
    max_queued_bytes_per_session: usize,
    /// SDP fmtp parameters received via ANNOUNCE, keyed by track_id.
    announce_fmtp: Mutex<HashMap<u8, Arc<str>>>,
}
struct State {
    mounts: RwLock<HashMap<Arc<str>, Arc<Mount>>>,
    config: ServerConfig,
    session_slots: Arc<Semaphore>,
    next_session: AtomicU64,
    cancel: CancellationToken,
    supervisor: TaskSupervisor,
    peer_sessions: Mutex<HashMap<IpAddr, usize>>,
    connection_rate_limiter: ConnectionRateLimiter,
}

#[derive(Default)]
struct StreamCounters {
    accepted_frames: AtomicU64,
    dropped_frames: AtomicU64,
    rtp_packets: AtomicU64,
    rtp_octets: AtomicU64,
    rtcp_sender_reports: AtomicU64,
    rtcp_receiver_reports: AtomicU64,
}

impl StreamCounters {
    fn add_accepted(&self, value: u64) {
        self.accepted_frames.fetch_add(value, Ordering::Relaxed);
    }
    fn add_dropped(&self, value: u64) {
        self.dropped_frames.fetch_add(value, Ordering::Relaxed);
    }
    fn add_rtp(&self, packets: u64, octets: u64) {
        self.rtp_packets.fetch_add(packets, Ordering::Relaxed);
        self.rtp_octets.fetch_add(octets, Ordering::Relaxed);
    }
    fn add_sender_reports(&self, value: u64) {
        self.rtcp_sender_reports.fetch_add(value, Ordering::Relaxed);
    }
    fn add_receiver_reports(&self, value: u64) {
        self.rtcp_receiver_reports
            .fetch_add(value, Ordering::Relaxed);
    }
    fn snapshot(&self, subscribers: usize) -> StreamStats {
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

impl Mount {
    fn publish_cached(
        &self,
        frame: AccessUnit,
    ) -> std::result::Result<usize, broadcast::error::SendError<AccessUnit>> {
        let mut cache = self
            .startup_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.push(&frame);
        drop(cache);
        if frame.data.len() <= self.max_queued_bytes_per_session
            && (!matches!(
                frame.codec,
                crate::media::Codec::H264 | crate::media::Codec::H265
            ) || frame.is_keyframe)
        {
            self.bootstrap
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(frame.track_id, frame.clone());
        }
        self.frames.send(frame)
    }

    fn subscribe_with_startup(
        &self,
        track_id: u8,
    ) -> (broadcast::Receiver<AccessUnit>, Vec<AccessUnit>) {
        let cache = self
            .startup_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let receiver = self.frames.subscribe();
        let mut startup = cache.snapshot(track_id);
        let bytes = startup.iter().fold(0usize, |total, frame| {
            total.saturating_add(frame.data.len())
        });
        if bytes > self.max_queued_bytes_per_session {
            startup.clear();
        }
        if startup.is_empty()
            && let Some(frame) = self
                .bootstrap
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&track_id)
                .cloned()
        {
            startup.push(frame);
        }
        (receiver, startup)
    }

    fn subscribe_with_all_startup(&self) -> (broadcast::Receiver<AccessUnit>, Vec<AccessUnit>) {
        let receiver = self.frames.subscribe();
        let cache = self
            .startup_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut startup = cache.snapshot_all();
        let bytes = startup.iter().fold(0usize, |total, frame| {
            total.saturating_add(frame.data.len())
        });
        if bytes > self.max_queued_bytes_per_session {
            startup.clear();
        }
        (receiver, startup)
    }
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
        config.validate()?;
        let listener = TcpListener::bind(config.listen).await?;
        let local_addr = listener.local_addr()?;
        let supervisor = TaskSupervisor::new(config.event_sink.clone());
        let connection_rate_limiter =
            ConnectionRateLimiter::new(config.security.connection_rate_limit)?;
        let state = Arc::new(State {
            mounts: RwLock::new(HashMap::new()),
            session_slots: Arc::new(Semaphore::new(config.max_sessions)),
            next_session: AtomicU64::new(1),
            cancel: supervisor.cancellation_token(),
            supervisor,
            peer_sessions: Mutex::new(HashMap::new()),
            connection_rate_limiter,
            config,
        });
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
                            emit(
                                &task_state.config.event_sink,
                                FrameworkEvent::Session(SessionEvent::Accepted { id, peer }),
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
                                let result = serve_connection(connection, peer, id, state).await;
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
    pub async fn register_mount(
        &self,
        path: impl Into<Arc<str>>,
        mut config: MountConfig,
    ) -> Result<MountHandle> {
        let path = path.into();
        if !path.starts_with('/') {
            return Err(Error::Rtsp("mount path must start with '/'".into()));
        }
        if self.state.mounts.read().await.len() >= self.state.config.resource_limits.max_mounts {
            return Err(Error::Limit("mounts"));
        }
        if let MountConfig::Proxy { upstream } = &mut config
            && upstream.cached_description.is_none()
        {
            let mut last_error = None;
            for endpoint in upstream.endpoints() {
                let options = ClientOptions {
                    transport: endpoint.transport,
                    credentials: endpoint.credentials,
                    request_timeout: endpoint.request_timeout,
                    jitter: endpoint.jitter,
                    #[cfg(feature = "tls")]
                    tls: endpoint.tls,
                    ..Default::default()
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
        let description = description_for(&config)?;
        let startup_cache_config = match &config {
            MountConfig::Proxy { upstream } => upstream.startup_cache,
            _ => StartupCacheConfig {
                enabled: false,
                ..Default::default()
            },
        };
        let startup_cache = KeyframeCache::new(&description, startup_cache_config)?;
        let (tx, _) = broadcast::channel(self.state.config.consumer_queue_capacity);
        let mount = Arc::new(Mount {
            id: MountId::from(path.clone()),
            config,
            frames: tx,
            stats: StreamCounters::default(),
            startup_cache: Mutex::new(startup_cache),
            bootstrap: Mutex::new(HashMap::new()),
            event_sink: self.state.config.event_sink.clone(),
            cancel: self.state.cancel.child_token(),
            proxy_running: AtomicBool::new(false),
            publishers: AtomicUsize::new(0),
            publisher_claimed: AtomicBool::new(false),
            max_queued_bytes_per_session: self
                .state
                .config
                .resource_limits
                .max_queued_bytes_per_session,
            announce_fmtp: Mutex::new(HashMap::new()),
        });
        let mut mounts = self.state.mounts.write().await;
        if mounts.contains_key(&path) {
            return Err(Error::Conflict(path.to_string()));
        }
        if mounts.len() >= self.state.config.resource_limits.max_mounts {
            return Err(Error::Limit("mounts"));
        }
        mounts.insert(path.clone(), mount.clone());
        drop(mounts);
        emit(
            &self.state.config.event_sink,
            FrameworkEvent::Mount(MountEvent::Registered {
                id: MountId::from(path.clone()),
            }),
        );
        Ok(MountHandle {
            path,
            description,
            inner: mount,
        })
    }
    pub async fn unregister_mount(&self, path: &str) -> Result<()> {
        let mount = self
            .state
            .mounts
            .write()
            .await
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
    }
}
fn description_for(config: &MountConfig) -> Result<MediaDescription> {
    match config {
        MountConfig::Local { description } | MountConfig::Publisher { description, .. } => {
            Ok(description.clone())
        }
        MountConfig::Proxy { upstream } => upstream.cached_description.clone().ok_or_else(|| {
            Error::Rtsp("proxy mount requires cached SDP description before serving".into())
        }),
        #[cfg(feature = "file-storage")]
        MountConfig::Vod { source } => Ok(source.description().clone()),
    }
}
impl MountHandle {
    pub fn publisher(&self) -> Result<LocalPublisher> {
        if matches!(self.inner.config, MountConfig::Local { .. }) {
            self.inner.publishers.fetch_add(1, Ordering::AcqRel);
            Ok(LocalPublisher {
                inner: self.inner.clone(),
                active: true,
            })
        } else {
            Err(Error::Conflict("mount is not a local source".into()))
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
    /// Use `sender.subscribe()` to create a fresh receiver for each connection.
    pub fn frame_sender(&self) -> &broadcast::Sender<AccessUnit> {
        &self.inner.frames
    }
    /// Get ANNOUNCE SDP fmtp parameters (snapshot at call time).
    pub fn announce_fmtp(&self) -> HashMap<u8, Arc<str>> {
        self.inner.announce_fmtp.lock().unwrap().clone()
    }
    /// Cancellation token tied to the mount lifetime.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }
    /// RTSP mount path, suitable as the canonical HTTP asset prefix.
    pub fn path(&self) -> &str {
        self.inner.id.as_str()
    }
    pub fn snapshot(&self) -> MountSnapshot {
        let subscribers = self.inner.frames.receiver_count();
        let s = self.inner.stats.snapshot(subscribers);
        let (startup_cache_frames, startup_cache_bytes) = self
            .inner
            .startup_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .usage();
        MountSnapshot {
            subscriber_count: s.subscribers,
            publisher_connected: self.inner.publishers.load(Ordering::Relaxed) > 0,
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
impl LocalPublisher {
    /// Number of RTSP sessions currently subscribed to this local source.
    pub fn subscriber_count(&self) -> usize {
        self.inner.frames.receiver_count()
    }

    /// True after the mount is unregistered or its server begins shutdown.
    pub fn is_closed(&self) -> bool {
        self.inner.cancel.is_cancelled()
    }

    pub fn try_push(&self, frame: AccessUnit) -> Result<PushResult> {
        let description = description_for(&self.inner.config)?;
        let Some(track) = description.track(frame.track_id) else {
            return Err(Error::Media(format!(
                "mount has no track {}",
                frame.track_id
            )));
        };
        if track.codec != frame.codec {
            return Err(Error::Media(format!(
                "track {} expects {:?}, got {:?}",
                frame.track_id, track.codec, frame.codec
            )));
        }
        if self.inner.frames.receiver_count() == 0 {
            self.inner.stats.add_dropped(1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::{ClientOptions, RtspClient},
        media::{Codec, MediaKind, TrackDescription},
    };
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
                crypto: None,
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
