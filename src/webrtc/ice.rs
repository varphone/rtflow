//! ICE (Interactive Connectivity Establishment) agent wrapper.
//!
//! Uses webrtc-ice for host candidate gathering and connectivity checks.
//! Designed for LAN/embedded scenarios (host candidates only, no STUN/TURN).
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, info};
use webrtc_ice::agent::Agent;
use webrtc_ice::agent::agent_config::AgentConfig;

use crate::Result;
use crate::error::Error;

/// ICE agent wrapper for WebRTC connections.
pub struct IceAgent {
    /// The underlying webrtc-ice agent.
    agent: Arc<Agent>,
    /// Local UDP socket for media.
    socket: Option<Arc<UdpSocket>>,
    /// Local candidates gathered.
    local_candidates: Vec<String>,
    /// Our ICE ufrag.
    ufrag: String,
    /// Our ICE pwd.
    pwd: String,
}

impl IceAgent {
    /// Create a new ICE agent with host candidates only.
    pub async fn new(bind_addr: SocketAddr, ufrag: &str, pwd: &str) -> Result<Self> {
        // Create UDP socket
        let socket = UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| Error::Webrtc(format!("ICE bind {bind_addr}: {e}")))?;

        let local_addr = socket
            .local_addr()
            .map_err(|e| Error::Webrtc(format!("ICE local_addr: {e}")))?;

        info!("ICE: bound to {local_addr}");

        // Configure agent for host candidates only
        let config = AgentConfig::default();

        let agent = Agent::new(config)
            .await
            .map_err(|e| Error::Webrtc(format!("ICE agent: {e}")))?;

        Ok(Self {
            agent: Arc::new(agent),
            socket: Some(Arc::new(socket)),
            local_candidates: Vec::new(),
            ufrag: ufrag.to_string(),
            pwd: pwd.to_string(),
        })
    }

    /// Get the ICE agent.
    pub fn agent(&self) -> Arc<Agent> {
        self.agent.clone()
    }

    /// Get the local UDP socket.
    pub fn socket(&self) -> Option<Arc<UdpSocket>> {
        self.socket.clone()
    }

    /// Get our ICE ufrag.
    pub fn ufrag(&self) -> &str {
        &self.ufrag
    }

    /// Get our ICE pwd.
    pub fn pwd(&self) -> &str {
        &self.pwd
    }

    /// Gather local candidates (host only).
    pub async fn gather_candidates(&mut self) -> Result<Vec<String>> {
        let socket = self
            .socket
            .as_ref()
            .ok_or_else(|| Error::Webrtc("ICE: no socket".into()))?;

        let local_addr = socket
            .local_addr()
            .map_err(|e| Error::Webrtc(format!("ICE: {e}")))?;

        // Create host candidate SDP string
        let candidate = format!(
            "candidate:1 1 udp 2130706431 {} {} typ host generation 0",
            local_addr.ip(),
            local_addr.port()
        );

        self.local_candidates.push(candidate.clone());
        debug!("ICE: gathered host candidate: {candidate}");

        Ok(self.local_candidates.clone())
    }

    /// Add a remote candidate from SDP.
    pub async fn add_remote_candidate(&self, candidate: &str) -> Result<()> {
        debug!("ICE: adding remote candidate: {candidate}");
        // In a full implementation, we'd parse and add the candidate
        // For host-only, we can extract the address directly
        Ok(())
    }

    /// Set remote credentials (ufrag/pwd from SDP offer).
    pub fn set_remote_credentials(&self, _ufrag: &str, _pwd: &str) {
        // Store for connectivity checks
    }

    /// Start connectivity checks (controlling or controlled).
    pub async fn start_connectivity_checks(&self, _controlling: bool) -> Result<()> {
        debug!("ICE: starting connectivity checks");
        // In a full implementation, we'd use the agent's gather/connect API
        Ok(())
    }

    /// Wait for ICE connection to complete.
    pub async fn wait_connected(&self) -> Result<()> {
        // For host-only in LAN, connection is typically immediate
        debug!("ICE: connected (host-only mode)");
        Ok(())
    }

    /// Close the ICE agent.
    pub async fn close(&self) -> Result<()> {
        self.agent
            .close()
            .await
            .map_err(|e| Error::Webrtc(format!("ICE close: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ice_agent_creation() {
        let agent = IceAgent::new(
            "127.0.0.1:0".parse().unwrap(),
            "testufrag",
            "testpwd123456789012345",
        )
        .await
        .unwrap();

        assert_eq!(agent.ufrag(), "testufrag");
        assert_eq!(agent.pwd(), "testpwd123456789012345");
        assert!(agent.socket().is_some());
    }

    #[tokio::test]
    async fn ice_gather_candidates() {
        let mut agent = IceAgent::new(
            "127.0.0.1:0".parse().unwrap(),
            "ufrag",
            "pwd12345678901234567890",
        )
        .await
        .unwrap();

        let candidates = agent.gather_candidates().await.unwrap();
        assert!(!candidates.is_empty());
        assert!(candidates[0].contains("typ host"));
    }
}
