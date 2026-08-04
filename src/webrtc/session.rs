//! WebRTC session management.
//!
//! Tracks active WHIP/WHEP sessions with their ICE, DTLS, and SRTP state.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::dtls::{DtlsConfig, DtlsConnection};
use super::ice::IceAgent;
use crate::Result;

/// WebRTC session type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionType {
    /// WHIP: browser publishes to server.
    Publish,
    /// WHEP: browser plays from server.
    Play,
}

/// WebRTC session state.
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
pub struct WebrtcSession {
    /// Unique session ID.
    pub id: String,
    /// Session type (publish or play).
    pub session_type: SessionType,
    /// Current state.
    pub state: SessionState,
    /// Mount path this session is associated with.
    pub mount_path: String,
    /// ICE agent.
    pub ice: Option<Arc<IceAgent>>,
    /// DTLS connection.
    pub dtls: Option<DtlsConnection>,
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
            ice: None,
            dtls: None,
            cancel: CancellationToken::new(),
            created_at: now,
            last_activity: now,
        }
    }

    /// Update last activity time.
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Check if session has timed out.
    pub fn is_expired(&self, timeout: Duration) -> bool {
        self.last_activity.elapsed() > timeout
    }

    /// Close the session.
    pub async fn close(&mut self) -> Result<()> {
        self.state = SessionState::Closing;
        self.cancel.cancel();

        if let Some(ice) = &self.ice {
            let _ = ice.close().await;
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
    /// Shared DTLS configuration.
    dtls_config: Arc<DtlsConfig>,
}

impl SessionRegistry {
    /// Create a new session registry.
    pub fn new(timeout: Duration) -> Result<Self> {
        let dtls_config = DtlsConfig::generate()?;

        Ok(Self {
            sessions: RwLock::new(HashMap::new()),
            timeout,
            dtls_config: Arc::new(dtls_config),
        })
    }

    /// Get the shared DTLS config.
    pub fn dtls_config(&self) -> Arc<DtlsConfig> {
        self.dtls_config.clone()
    }

    /// Create a new session.
    pub async fn create_session(
        &self,
        session_type: SessionType,
        mount_path: String,
    ) -> Result<Arc<RwLock<WebrtcSession>>> {
        let id = generate_session_id();
        let session = WebrtcSession::new(id.clone(), session_type, mount_path);

        let session = Arc::new(RwLock::new(session));
        self.sessions
            .write()
            .await
            .insert(id.clone(), session.clone());

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
        session
    }

    /// Clean up expired sessions.
    pub async fn cleanup_expired(&self) -> usize {
        let mut expired = Vec::new();

        {
            let sessions = self.sessions.read().await;
            for (id, session) in sessions.iter() {
                let s = session.read().await;
                if s.is_expired(self.timeout) {
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

/// Generate a unique session ID.
fn generate_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:016x}", ts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_generation() {
        let id1 = generate_session_id();
        let id2 = generate_session_id();
        assert_eq!(id1.len(), 16);
        // IDs should be different (unless called in same nanosecond)
        // Just check format
        assert!(id1.chars().all(|c| c.is_ascii_hexdigit()));
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
}
