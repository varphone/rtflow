mod proxy_runtime;
mod session;
mod tasks;
use session::serve_connection;

use crate::{
    Error, Result,
    auth::{AllowAll, Authenticator},
    client::{ClientOptions, RtspClient},
    media::{AccessUnit, MediaDescription},
    proxy::{ProxyState, UpstreamConfig},
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    ops::RangeInclusive,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{RwLock, Semaphore, broadcast},
    task::{JoinHandle, JoinSet},
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
            consumer_queue_capacity: 64,
            no_subscriber_grace: Duration::from_secs(5),
            shutdown_grace: Duration::from_secs(5),
            authenticator: Arc::new(AllowAll),
        }
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
}
struct Mount {
    config: MountConfig,
    frames: broadcast::Sender<AccessUnit>,
    stats: std::sync::Mutex<StreamStats>,
    cancel: CancellationToken,
    proxy_running: AtomicBool,
    publishers: AtomicUsize,
    publisher_claimed: AtomicBool,
}
struct State {
    mounts: RwLock<HashMap<Arc<str>, Arc<Mount>>>,
    config: ServerConfig,
    session_slots: Arc<Semaphore>,
    next_session: AtomicU64,
    cancel: CancellationToken,
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
    accept_task: Option<JoinHandle<()>>,
}

impl RtspServer {
    pub async fn bind(config: ServerConfig) -> Result<Self> {
        if config.consumer_queue_capacity == 0 {
            return Err(Error::Limit("consumer queue capacity"));
        }
        if config.max_sessions == 0 {
            return Err(Error::Limit("RTSP sessions"));
        }
        let listener = TcpListener::bind(config.listen).await?;
        let local_addr = listener.local_addr()?;
        let state = Arc::new(State {
            mounts: RwLock::new(HashMap::new()),
            session_slots: Arc::new(Semaphore::new(config.max_sessions)),
            next_session: AtomicU64::new(1),
            cancel: CancellationToken::new(),
            config,
        });
        let task_state = state.clone();
        let accept_task = tokio::spawn(async move {
            let mut sessions = JoinSet::new();
            loop {
                tokio::select! {
                    _ = task_state.cancel.cancelled() => break,
                    joined = sessions.join_next(), if !sessions.is_empty() => {
                        let _ = joined;
                    }
                    accepted = listener.accept() => match accepted {
                        Ok((stream, peer)) => {
                            let Ok(slot) = task_state.session_slots.clone().try_acquire_owned() else {
                                drop(stream);
                                continue;
                            };
                            let state = task_state.clone();
                            sessions.spawn(async move {
                                let _slot = slot;
                                let _ = serve_connection(stream, peer, state).await;
                            });
                        }
                        Err(_) => break,
                    }
                }
            }
            let deadline = tokio::time::Instant::now() + task_state.config.shutdown_grace;
            while !sessions.is_empty() {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => {
                        sessions.shutdown().await;
                        break;
                    }
                    joined = sessions.join_next() => {
                        let _ = joined;
                    }
                }
            }
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
        if let MountConfig::Proxy { upstream } = &mut config
            && upstream.cached_description.is_none()
        {
            let options = ClientOptions {
                transport: upstream.transport,
                credentials: upstream.credentials.clone(),
                request_timeout: upstream.request_timeout,
                jitter: upstream.jitter.clone(),
                ..Default::default()
            };
            upstream.cached_description =
                Some(RtspClient::describe(upstream.url.clone(), options).await?);
        }
        let description = description_for(&config)?;
        let (tx, _) = broadcast::channel(self.state.config.consumer_queue_capacity);
        let mount = Arc::new(Mount {
            config,
            frames: tx,
            stats: std::sync::Mutex::new(StreamStats::default()),
            cancel: self.state.cancel.child_token(),
            proxy_running: AtomicBool::new(false),
            publishers: AtomicUsize::new(0),
            publisher_claimed: AtomicBool::new(false),
        });
        let mut mounts = self.state.mounts.write().await;
        if mounts.contains_key(&path) {
            return Err(Error::Conflict(path.to_string()));
        }
        mounts.insert(path.clone(), mount.clone());
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
        Ok(())
    }
    pub async fn shutdown(mut self) -> Result<()> {
        self.state.cancel.cancel();
        if let Some(task) = self.accept_task.take() {
            task.await
                .map_err(|error| Error::Rtsp(format!("server supervisor failed: {error}")))?;
        }
        Ok(())
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
    pub fn snapshot(&self) -> MountSnapshot {
        let mut s = self.inner.stats.lock().unwrap().clone();
        s.subscribers = self.inner.frames.receiver_count();
        MountSnapshot {
            subscriber_count: s.subscribers,
            publisher_connected: self.inner.publishers.load(Ordering::Relaxed) > 0,
            proxy_state: match &self.inner.config {
                MountConfig::Proxy { upstream } => Some(upstream.state()),
                _ => None,
            },
            stats: s,
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
            self.inner.stats.lock().unwrap().dropped_frames += 1;
            return Ok(PushResult::DroppedNoSubscribers);
        }
        match self.inner.frames.send(frame) {
            Ok(_) => {
                self.inner.stats.lock().unwrap().accepted_frames += 1;
                Ok(PushResult::Accepted)
            }
            Err(_) => {
                self.inner.stats.lock().unwrap().dropped_frames += 1;
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
            }],
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
