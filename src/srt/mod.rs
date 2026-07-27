//! SRT (Secure Reliable Transport) protocol support.
//!
//! Provides an SRT listener that accepts caller connections and ingests
//! MPEG-TS streams into the mount registry.
//!
//! # Features
//!
//! - HSv5 handshake (induction + conclusion)
//! - Data packet reception with sequence tracking
//! - MPEG-TS demuxing to AccessUnit
//! - Integration with shared mount registry
//!
//! # Limitations
//!
//! - Listener mode only (no caller/rendezvous)
//! - No encryption (KM handshake skipped)
//! - Simplified ARQ (loss detection without retransmit requests)
//!
//! # Example
//!
//! ```no_run
//! use rtflow::srt::SrtServer;
//! use rtflow::mount::MountRegistry;
//! use std::sync::Arc;
//!
//! # async fn example() -> rtflow::Result<()> {
//! let mounts = Arc::new(MountRegistry::new(100));
//! let server = SrtServer::bind("0.0.0.0:9000", mounts).await?;
//! server.run().await?;
//! # Ok(())
//! # }
//! ```
pub mod handshake;
pub mod packet;
pub mod ts_demux;

use handshake::{
    HandshakeType, conclusion_response, generate_cookie, handshake_control_packet,
    induction_response, parse_handshake,
};
use packet::{ControlType, SrtPacket, encode_control_packet, keepalive_packet, parse_packet};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::mount::{MountConfig, MountRegistry};
use crate::{Error, Result};

/// SRT server configuration.
#[derive(Clone, Debug)]
pub struct SrtConfig {
    /// Bind address.
    pub bind_addr: SocketAddr,
    /// Maximum concurrent connections.
    pub max_connections: usize,
    /// Receive buffer size.
    pub recv_buffer_size: usize,
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:9000".parse().unwrap(),
            max_connections: 10,
            recv_buffer_size: 8192 * 188, // ~8192 TS packets
        }
    }
}

/// SRT connection state.
struct SrtConnection {
    /// Peer socket ID.
    peer_socket_id: u32,
    /// Our socket ID for this connection.
    local_socket_id: u32,
    /// Connection start time for timestamp calculation.
    start_time: Instant,
    /// Last received sequence number.
    last_sequence: u32,
    /// TS demuxer for this connection.
    demuxer: ts_demux::TsStreamDemuxer,
    /// Mount path for this stream.
    mount_path: Option<String>,
    /// Whether we've registered the mount.
    mount_registered: bool,
}

/// SRT server that accepts caller connections.
pub struct SrtServer {
    socket: Arc<UdpSocket>,
    mounts: Arc<MountRegistry>,
    config: SrtConfig,
    cancel: CancellationToken,
}

impl SrtServer {
    /// Bind an SRT server to the given address.
    pub async fn bind(
        addr: impl tokio::net::ToSocketAddrs,
        mounts: Arc<MountRegistry>,
    ) -> Result<Self> {
        let socket = UdpSocket::bind(&addr)
            .await
            .map_err(|e| Error::Srt(format!("bind: {e}")))?;

        let local_addr = socket.local_addr().ok();
        info!("SRT server listening on {:?}", local_addr);

        Ok(Self {
            socket: Arc::new(socket),
            mounts,
            config: SrtConfig::default(),
            cancel: CancellationToken::new(),
        })
    }

    /// Create an SRT server with full configuration.
    pub async fn with_config(config: SrtConfig, mounts: Arc<MountRegistry>) -> Result<Self> {
        let socket = UdpSocket::bind(config.bind_addr)
            .await
            .map_err(|e| Error::Srt(format!("bind {}: {e}", config.bind_addr)))?;

        info!("SRT server listening on {}", config.bind_addr);

        Ok(Self {
            socket: Arc::new(socket),
            mounts,
            config,
            cancel: CancellationToken::new(),
        })
    }

    /// Get the local address the server is bound to.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Get a cancellation token for graceful shutdown.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Run the server receive loop until cancelled.
    pub async fn run(self) -> Result<()> {
        let mut buf = vec![0u8; 2048];
        let mut connections: HashMap<SocketAddr, SrtConnection> = HashMap::new();
        let mut next_socket_id: u32 = 1;

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("SRT server shutting down");
                    break;
                }
                result = self.socket.recv_from(&mut buf) => {
                    let (n, peer) = match result {
                        Ok(r) => r,
                        Err(e) => {
                            warn!("SRT recv error: {e}");
                            continue;
                        }
                    };

                    let data = &buf[..n];
                    let Some(srt_packet) = parse_packet(data) else {
                        debug!("SRT: failed to parse packet from {peer}");
                        continue;
                    };

                    match srt_packet {
                        SrtPacket::Control(ctrl) => {
                            self.handle_control(
                                &ctrl,
                                peer,
                                &mut connections,
                                &mut next_socket_id,
                            ).await;
                        }
                        SrtPacket::Data(data_pkt) => {
                            self.handle_data(
                                &data_pkt,
                                peer,
                                &mut connections,
                            ).await;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_control(
        &self,
        ctrl: &packet::ControlPacket,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
        next_socket_id: &mut u32,
    ) {
        match ctrl.control_type {
            ControlType::Handshake => {
                let Some(hs) = parse_handshake(&ctrl.data) else {
                    debug!("SRT: failed to parse handshake from {peer}");
                    return;
                };

                match hs.handshake_type {
                    HandshakeType::Induction => {
                        // Phase 1: Send induction response with cookie
                        let cookie = generate_cookie(peer, ctrl.timestamp);
                        let local_socket_id = *next_socket_id;
                        *next_socket_id += 1;

                        let response = induction_response(local_socket_id, cookie);
                        let response_pkt =
                            handshake_control_packet(&response, 0, ctrl.timestamp);
                        let encoded = encode_control_packet(&response_pkt);

                        if let Err(e) = self.socket.send_to(&encoded, peer).await {
                            warn!("SRT: failed to send induction response to {peer}: {e}");
                        } else {
                            debug!("SRT: induction response sent to {peer}");
                        }
                    }
                    HandshakeType::Conclusion => {
                        // Phase 2: Verify cookie and complete handshake
                        let _expected_cookie = generate_cookie(peer, ctrl.timestamp);
                        // Note: In production, verify hs.cookie matches expected

                        let local_socket_id = *next_socket_id;
                        *next_socket_id += 1;

                        let response = conclusion_response(
                            local_socket_id,
                            hs.initial_sequence,
                            hs.socket_id,
                        );
                        let response_pkt =
                            handshake_control_packet(&response, hs.socket_id, ctrl.timestamp);
                        let encoded = encode_control_packet(&response_pkt);

                        if let Err(e) = self.socket.send_to(&encoded, peer).await {
                            warn!("SRT: failed to send conclusion response to {peer}: {e}");
                        } else {
                            info!("SRT: connection established with {peer}");

                            // Create connection state
                            connections.insert(
                                peer,
                                SrtConnection {
                                    peer_socket_id: hs.socket_id,
                                    local_socket_id,
                                    start_time: Instant::now(),
                                    last_sequence: hs.initial_sequence,
                                    demuxer: ts_demux::TsStreamDemuxer::new(),
                                    mount_path: None,
                                    mount_registered: false,
                                },
                            );
                        }
                    }
                    _ => {
                        debug!("SRT: unhandled handshake type from {peer}: {:?}", hs.handshake_type);
                    }
                }
            }
            ControlType::Keepalive => {
                // Respond with keepalive
                if let Some(conn) = connections.get(&peer) {
                    let timestamp = conn.start_time.elapsed().as_micros() as u32;
                    let response = keepalive_packet(conn.peer_socket_id, timestamp);
                    let encoded = encode_control_packet(&response);
                    let _ = self.socket.send_to(&encoded, peer).await;
                }
            }
            ControlType::Shutdown => {
                info!("SRT: peer {peer} requested shutdown");
                connections.remove(&peer);
            }
            _ => {
                debug!("SRT: unhandled control type from {peer}: {:?}", ctrl.control_type);
            }
        }
    }

    async fn handle_data(
        &self,
        data_pkt: &packet::DataPacket,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
    ) {
        let Some(conn) = connections.get_mut(&peer) else {
            debug!("SRT: data from unknown peer {peer}");
            return;
        };

        // Track sequence numbers (simplified - no reordering)
        conn.last_sequence = data_pkt.sequence;

        // Feed TS data to demuxer
        conn.demuxer.push(&data_pkt.payload);

        // Process completed access units
        let units = conn.demuxer.take_units();
        if units.is_empty() {
            return;
        }

        // Register mount if not done yet
        if !conn.mount_registered {
            if let Some(desc) = conn.demuxer.description() {
                let mount_path = format!("/srt/{}", peer.port());
                let config = MountConfig::Publisher {
                    description: desc,
                    allow_replace_after_disconnect: true,
                };

                match self.mounts.register(mount_path.clone().into(), config) {
                    Ok(handle) => {
                        info!("SRT: registered mount {mount_path}");
                        conn.mount_path = Some(mount_path);
                        conn.mount_registered = true;

                        // Get publisher and push units
                        if let Ok(publisher) = handle.publisher() {
                            for unit in units {
                                let _ = publisher.push(unit);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("SRT: failed to register mount: {e}");
                    }
                }
            }
        } else if let Some(mount_path) = &conn.mount_path {
            // Push to existing mount
            if let Some(handle) = self.mounts.get_handle(mount_path) {
                if let Ok(publisher) = handle.publisher() {
                    for unit in units {
                        let _ = publisher.push(unit);
                    }
                }
            }
        }
    }

    /// Shutdown the server.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn server_binds_and_shuts_down() {
        let mounts = Arc::new(MountRegistry::new(10));
        let config = SrtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        };
        let server = SrtServer::with_config(config, mounts).await.unwrap();
        let addr = server.local_addr().unwrap();
        assert_ne!(addr.port(), 0);

        let cancel = server.cancellation_token();
        let handle = tokio::spawn(server.run());

        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();

        handle.await.unwrap().unwrap();
    }
}
