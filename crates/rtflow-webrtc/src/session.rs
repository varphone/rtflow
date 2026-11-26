//! WebRTC session management.
//!
//! Tracks active WHIP/WHEP sessions with their peer connection, ICE state
//! and media tasks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use webrtc::peer_connection::PeerConnection;

use super::transport::PcState;
use rtflow_core::error::Result;

/// WebRTC session type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionType {
    /// WHIP: browser publishes to server.
    Publish,
    /// WHEP: browser plays from server.
    Play,
}

/// WebRTC session state, driven by the ICE/peer-connection state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    /// SDP exchange in progress.
    Connecting,
    /// ICE/DTLS establishing.
    Establishing,
    /// Media flowing.
    Active,
    /// Shutting down.
    Closing,
    /// Terminated.
    Closed,
}

/// A single WebRTC session.
/// 会话建立后多久还没连上就丢掉（见 [`WebrtcSession::is_expired_with`]）。
///
/// 浏览器正常建连在 1 秒内完成；10 秒还没连上的会话基本是客户端已经走了。
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(10);

pub struct WebrtcSession {
    /// Unique session ID.
    pub id: String,
    /// Session type (publish or play).
    pub session_type: SessionType,
    /// Current state.
    pub state: SessionState,
    /// Mount path this session is associated with.
    pub mount_path: String,
    /// The peer connection (transport + media plane).
    pub pc: Option<Arc<dyn PeerConnection>>,
    /// Transport state (ICE candidates, connection signals).
    pub pc_state: Option<Arc<PcState>>,
    /// Media-plane tasks (publish/play loops) to abort on close.
    media_tasks: Vec<JoinHandle<()>>,
    /// Cancellation token for cleanup.
    pub cancel: CancellationToken,
    /// Session creation time.
    pub created_at: Instant,
    /// Last activity time.
    pub last_activity: Instant,
}

impl WebrtcSession {
    /// Create a new session.
    pub fn new(id: String, session_type: SessionType, mount_path: String) -> Self {
        let now = Instant::now();
        Self {
            id,
            session_type,
            state: SessionState::Connecting,
            mount_path,
            pc: None,
            pc_state: None,
            media_tasks: Vec::new(),
            cancel: CancellationToken::new(),
            created_at: now,
            last_activity: now,
        }
    }

    /// Attach the peer connection and its transport state.
    pub fn attach(&mut self, pc: Arc<dyn PeerConnection>, pc_state: Arc<PcState>) {
        self.pc = Some(pc);
        self.pc_state = Some(pc_state);
        self.state = SessionState::Establishing;
    }

    /// Advance the session state from an ICE connection state.
    pub fn on_ice_state(&mut self, ice_state: &webrtc::peer_connection::RTCIceConnectionState) {
        use webrtc::peer_connection::RTCIceConnectionState as S;
        match ice_state {
            S::Checking | S::New => {
                if self.state == SessionState::Connecting {
                    self.state = SessionState::Establishing;
                }
            }
            S::Connected | S::Completed => {
                if self.state != SessionState::Active {
                    info!("WebRTC session {} active", self.id);
                    self.state = SessionState::Active;
                }
            }
            S::Failed | S::Disconnected | S::Closed => {
                self.state = SessionState::Closed;
            }
            _ => {}
        }
    }

    /// Spawn a media-plane task owned by this session.
    pub fn spawn_media_task(&mut self, task: JoinHandle<()>) {
        self.media_tasks.push(task);
    }

    /// Update last activity time.
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Check if session has timed out.
    ///
    /// Active sessions are not reaped by the idle sweep — media flowing
    /// keeps the session alive until the peer closes it (DELETE request
    /// or an ICE failure, which sets [`SessionState::Closed`]).
    pub fn is_expired(&self, timeout: Duration, max_lifetime: Duration) -> bool {
        self.is_expired_with(timeout, max_lifetime, CONNECT_DEADLINE)
    }

    /// 带「连接超时」的过期判定。
    ///
    /// `connect_deadline`：会话建好之后多久还没连上（ICE 既没到 Connected 也没 Failed）
    /// 就丢掉。**这个很关键**：客户端发完 offer 就关页面/断网时，ICE 会停在 New/Checking，
    /// 底层 agent 每 200ms 打一条 `pingAllCandidates called with no candidate pairs` WARN，
    /// 一直刷到 60 秒的 idle 超时 —— 一个僵尸会话就能刷出三百条。
    pub fn is_expired_with(
        &self,
        timeout: Duration,
        max_lifetime: Duration,
        connect_deadline: Duration,
    ) -> bool {
        if self.created_at.elapsed() > max_lifetime {
            return true;
        }
        if let Some(pc_state) = &self.pc_state
            && let Ok(ice) = pc_state.ice_state.try_read()
        {
            use webrtc::peer_connection::RTCIceConnectionState as S;
            if matches!(*ice, S::Failed | S::Closed) {
                return true;
            }
            let connected = matches!(*ice, S::Connected | S::Completed);
            if !connected && self.created_at.elapsed() > connect_deadline {
                return true;
            }
        }
        self.state != SessionState::Active && self.last_activity.elapsed() > timeout
    }

    /// Close the session: cancel tasks, close the peer connection.
    pub async fn close(&mut self) -> Result<()> {
        self.state = SessionState::Closing;
        self.cancel.cancel();
        for task in self.media_tasks.drain(..) {
            task.abort();
        }
        if let Some(pc) = &self.pc {
            let _ = pc.close().await;
        }
        self.state = SessionState::Closed;
        info!("WebRTC session {} closed", self.id);
        Ok(())
    }
}

/// Session registry for managing active WebRTC sessions.
pub struct SessionRegistry {
    /// Active sessions by ID.
    sessions: RwLock<HashMap<String, Arc<RwLock<WebrtcSession>>>>,
    /// Session timeout duration.
    timeout: Duration,
    max_sessions: usize,
    max_sessions_per_mount: usize,
    max_lifetime: Duration,
    #[cfg(feature = "metrics")]
    metrics: Option<Arc<rtflow_core::metrics::MetricsRegistry>>,
}

impl SessionRegistry {
    /// Create a new session registry.
    pub fn new(timeout: Duration) -> Result<Self> {
        Self::new_with_limits(timeout, 0, 0, Duration::from_secs(12 * 60 * 60))
    }

    /// Create a registry with explicit session limits.
    pub fn new_with_limits(
        timeout: Duration,
        max_sessions: usize,
        max_sessions_per_mount: usize,
        max_lifetime: Duration,
    ) -> Result<Self> {
        Ok(Self {
            sessions: RwLock::new(HashMap::new()),
            timeout,
            max_sessions,
            max_sessions_per_mount,
            max_lifetime,
            #[cfg(feature = "metrics")]
            metrics: None,
        })
    }

    /// Attach a metrics registry for session counters.
    #[cfg(feature = "metrics")]
    pub fn with_metrics(mut self, metrics: Arc<rtflow_core::metrics::MetricsRegistry>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Create a new session.
    pub async fn create_session(
        &self,
        session_type: SessionType,
        mount_path: String,
    ) -> Result<Arc<RwLock<WebrtcSession>>> {
        {
            let sessions = self.sessions.read().await;
            if self.max_sessions > 0 && sessions.len() >= self.max_sessions {
                return Err(rtflow_core::error::Error::Limit(
                    "WebRTC session limit reached",
                ));
            }
            if self.max_sessions_per_mount > 0 {
                let mut per_mount = 0usize;
                for session in sessions.values() {
                    if session.read().await.mount_path == mount_path {
                        per_mount += 1;
                    }
                }
                if per_mount >= self.max_sessions_per_mount {
                    return Err(rtflow_core::error::Error::Limit(
                        "WebRTC per-mount session limit reached",
                    ));
                }
            }
        }
        let id = generate_session_id();
        let session = WebrtcSession::new(id.clone(), session_type, mount_path);

        let session = Arc::new(RwLock::new(session));
        self.sessions
            .write()
            .await
            .insert(id.clone(), session.clone());

        #[cfg(feature = "metrics")]
        if let Some(metrics) = &self.metrics {
            use std::sync::atomic::Ordering;
            metrics
                .webrtc_sessions_total
                .fetch_add(1, Ordering::Relaxed);
            metrics
                .webrtc_sessions_active
                .fetch_add(1, Ordering::Relaxed);
        }
        info!("WebRTC session created: {id} ({session_type:?})");
        Ok(session)
    }

    /// Get a session by ID.
    pub async fn get_session(&self, id: &str) -> Option<Arc<RwLock<WebrtcSession>>> {
        self.sessions.read().await.get(id).cloned()
    }

    /// Remove a session.
    pub async fn remove_session(&self, id: &str) -> Option<Arc<RwLock<WebrtcSession>>> {
        let session = self.sessions.write().await.remove(id);
        if let Some(s) = &session {
            let mut s = s.write().await;
            let _ = s.close().await;
        }
        #[cfg(feature = "metrics")]
        if session.is_some()
            && let Some(metrics) = &self.metrics
        {
            use std::sync::atomic::Ordering;
            metrics
                .webrtc_sessions_active
                .fetch_sub(1, Ordering::Relaxed);
        }
        session
    }

    /// Clean up expired sessions.
    pub async fn cleanup_expired(&self) -> usize {
        let mut expired = Vec::new();

        {
            let sessions = self.sessions.read().await;
            for (id, session) in sessions.iter() {
                let s = session.read().await;
                if s.is_expired(self.timeout, self.max_lifetime) {
                    expired.push(id.clone());
                }
            }
        }

        for id in &expired {
            warn!("WebRTC session {id} expired, removing");
            self.remove_session(id).await;
        }

        expired.len()
    }

    /// Get active session count.
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

/// Generate an unguessable session ID (128-bit SplitMix64 over a
/// time-seeded counter). The ID is used as the WHIP/WHEP resource URL, so
/// predictability would let anyone DELETE another client's session.
fn generate_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut z = seed
        .wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
        .wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    let a = z ^ (z >> 31);
    z = z.wrapping_mul(0xBF58476D1CE4E5B9) ^ (z >> 27);
    let b = z.wrapping_mul(0x94D049BB133111EB) ^ (z >> 31);
    format!("{a:016x}{b:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_generation() {
        let id1 = generate_session_id();
        let id2 = generate_session_id();
        // 128-bit random hex, unique per call.
        assert_eq!(id1.len(), 32);
        assert!(id1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn session_registry_create_get() {
        let registry = SessionRegistry::new(Duration::from_secs(60)).unwrap();

        let session = registry
            .create_session(SessionType::Play, "/live/stream".to_string())
            .await
            .unwrap();

        let id = session.read().await.id.clone();
        assert_eq!(registry.session_count().await, 1);

        let retrieved = registry.get_session(&id).await;
        assert!(retrieved.is_some());
    }

    #[tokio::test]
    async fn session_registry_remove() {
        let registry = SessionRegistry::new(Duration::from_secs(60)).unwrap();

        let session = registry
            .create_session(SessionType::Publish, "/live/cam".to_string())
            .await
            .unwrap();

        let id = session.read().await.id.clone();
        registry.remove_session(&id).await;

        assert_eq!(registry.session_count().await, 0);
        assert!(registry.get_session(&id).await.is_none());
    }

    #[test]
    fn session_state_progression() {
        use webrtc::peer_connection::RTCIceConnectionState as S;
        let mut session = WebrtcSession::new("t".into(), SessionType::Play, "/live".into());
        assert_eq!(session.state, SessionState::Connecting);
        session.on_ice_state(&S::Checking);
        assert_eq!(session.state, SessionState::Establishing);
        session.on_ice_state(&S::Connected);
        assert_eq!(session.state, SessionState::Active);
        session.on_ice_state(&S::Failed);
        assert_eq!(session.state, SessionState::Closed);
    }
}
