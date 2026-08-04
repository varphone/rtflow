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
pub mod tsbpd;

use handshake::{
    HandshakeType, conclusion_response, generate_cookie, handshake_control_packet,
    induction_response, parse_handshake,
};
use packet::{
    ControlType, SrtPacket, ack_packet, encode_control_packet, keepalive_packet, nak_packet,
    parse_packet,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use tsbpd::TsbpdBuffer;

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
    /// TSBPD delay in milliseconds.
    pub tsbpd_delay_ms: u32,
    /// ACK interval in milliseconds.
    pub ack_interval_ms: u64,
    /// Keepalive timeout (disconnect after this without keepalive).
    pub keepalive_timeout: Duration,
    /// Optional metrics registry for protocol-level counters.
    #[cfg(feature = "metrics")]
    pub metrics: Option<Arc<crate::metrics::MetricsRegistry>>,
    /// Allowed stream ids for publish (SRT `streamid` parameter). Empty
    /// means open mode (any stream id may publish); otherwise a caller
    /// whose stream id is not listed is rejected at handshake time.
    pub allowed_stream_ids: Vec<String>,
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:9000".parse().unwrap(),
            max_connections: 10,
            recv_buffer_size: 8192 * 188, // ~8192 TS packets
            tsbpd_delay_ms: tsbpd::DEFAULT_TSBPD_DELAY_MS,
            ack_interval_ms: 10,
            keepalive_timeout: Duration::from_secs(30),
            #[cfg(feature = "metrics")]
            metrics: None,
            allowed_stream_ids: Vec::new(),
        }
    }
}

/// SRT connection state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum ConnState {
    /// Handshake in progress.
    Connecting,
    /// Data transfer active.
    Established,
    /// Shutdown requested, draining.
    Closing,
}

/// SRT connection state.
#[allow(dead_code)]
struct SrtConnection {
    /// Peer socket ID.
    peer_socket_id: u32,
    /// Our socket ID for this connection.
    local_socket_id: u32,
    /// Connection start time for timestamp calculation.
    start_time: Instant,
    /// Connection state.
    state: ConnState,
    /// TSBPD reorder buffer.
    tsbpd: TsbpdBuffer,
    /// TS demuxer for this connection.
    demuxer: ts_demux::TsStreamDemuxer,
    /// Mount path for this stream.
    mount_path: Option<String>,
    /// Whether we've registered the mount.
    mount_registered: bool,
    /// Cached mount for this connection (avoids a registry lookup + hash
    /// on every delivered packet).
    mount: Option<Arc<crate::mount::Mount>>,
    /// Stream ID parsed from handshake (for mount routing).
    stream_id: Option<String>,
    /// Last time we received any packet (for keepalive timeout).
    last_recv_time: Instant,
    /// Last time we sent an ACK.
    last_ack_time: Instant,
    /// Monotonically increasing ACK sequence number.
    ack_seq: u32,
    /// Packets received since last ACK.
    pkts_since_ack: u32,
    /// Last NAK send time (throttle).
    last_nak_time: Instant,
    /// Loss ranges already reported via NAK; unchanged gaps are not
    /// re-reported (a peer that never retransmits must not be flooded).
    last_nak_losses: Vec<(u32, u32)>,
}

/// SRT server that accepts caller connections.
pub struct SrtServer {
    socket: Arc<UdpSocket>,
    mounts: Arc<MountRegistry>,
    config: SrtConfig,
    cancel: CancellationToken,
    /// SYN cookies issued during induction, pending conclusion verification.
    pending_cookies: std::sync::Mutex<HashMap<SocketAddr, (u32, Instant)>>,
}

/// How long an issued SYN cookie stays valid before it must be redeemed.
const COOKIE_TTL: Duration = Duration::from_secs(30);

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
            pending_cookies: std::sync::Mutex::new(HashMap::new()),
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
            pending_cookies: std::sync::Mutex::new(HashMap::new()),
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
        let mut ack_seq_counter: u32 = 1;
        let mut tick_interval = tokio::time::interval(Duration::from_millis(10));

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    info!("SRT server shutting down");
                    break;
                }
                _ = tick_interval.tick() => {
                    // Periodic maintenance: ACK, NAK, timeout
                    self.maintenance(&mut connections, &mut ack_seq_counter).await;
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

    /// Periodic maintenance: send ACKs, detect losses (NAK), expire idle connections.
    async fn maintenance(
        &self,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
        ack_seq_counter: &mut u32,
    ) {
        let now = Instant::now();
        let mut expired = Vec::new();

        // Expire unredeemed SYN cookies so a flood of inductions cannot grow the table.
        {
            let mut cookies = self.pending_cookies.lock().unwrap_or_else(
                std::sync::PoisonError::into_inner,
            );
            if cookies.len() > 64 {
                cookies.retain(|_, (_, issued_at)| issued_at.elapsed() <= COOKIE_TTL);
            }
        }

        for (peer, conn) in connections.iter_mut() {
            // Keepalive timeout check
            if now.duration_since(conn.last_recv_time) > self.config.keepalive_timeout {
                expired.push(*peer);
                continue;
            }

            if conn.state != ConnState::Established {
                continue;
            }

            let timestamp = conn.start_time.elapsed().as_micros() as u32;

            // Periodic ACK
            let ack_due = now.duration_since(conn.last_ack_time).as_millis()
                >= self.config.ack_interval_ms as u128;
            if ack_due && conn.pkts_since_ack > 0 {
                let ack_num = *ack_seq_counter;
                *ack_seq_counter += 1;
                let ack = ack_packet(
                    ack_num,
                    conn.tsbpd.ack_number(),
                    conn.peer_socket_id,
                    timestamp,
                );
                let encoded = encode_control_packet(&ack);
                let _ = self.socket.send_to(&encoded, *peer).await;
                conn.last_ack_time = now;
                conn.pkts_since_ack = 0;
            }

            // Loss detection -> NAK, throttled and deduplicated: an
            // unchanged gap is only reported once, and never more often
            // than every 20ms.
            let losses = conn.tsbpd.detect_losses();
            if !losses.is_empty()
                && now.duration_since(conn.last_nak_time).as_millis() >= 20
            {
                if losses != conn.last_nak_losses {
                    let nak = nak_packet(&losses, conn.peer_socket_id, timestamp);
                    let encoded = encode_control_packet(&nak);
                    let _ = self.socket.send_to(&encoded, *peer).await;
                    debug!("SRT: sent NAK for {} loss ranges to {peer}", losses.len());
                    conn.last_nak_losses = losses;
                }
                conn.last_nak_time = now;
            }

            // Release TSBPD ready packets -> demuxer
            let ready = conn.tsbpd.pop_ready();
            if !ready.is_empty() {
                for pkt in ready {
                    conn.demuxer.push(&pkt.payload);
                }
                self.deliver_units(conn, *peer).await;
            }
        }

        // Remove expired connections
        for peer in expired {
            warn!("SRT: connection {peer} timed out (no keepalive)");
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &self.config.metrics
                && let Some(conn) = connections.get(&peer)
                && conn.mount_registered
            {
                use std::sync::atomic::Ordering;
                metrics.srt_streams_active.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(conn) = connections.get(&peer)
                && conn.mount_registered
                && let Some(path) = &conn.mount_path
            {
                self.mounts.unregister(path);
                info!("SRT: unregistered mount {path} with expired connection");
            }
            connections.remove(&peer);
        }
    }

    async fn handle_control(
        &self,
        ctrl: &packet::ControlPacket,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
        next_socket_id: &mut u32,
    ) {
        // Update last recv time for existing connections
        if let Some(conn) = connections.get_mut(&peer) {
            conn.last_recv_time = Instant::now();
        }

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
                        // Remember the issued cookie so the conclusion can be verified.
                        {
                            let mut cookies = self.pending_cookies.lock().unwrap_or_else(
                                std::sync::PoisonError::into_inner,
                            );
                            cookies.insert(peer, (cookie, Instant::now()));
                        }
                        let local_socket_id = *next_socket_id;
                        *next_socket_id += 1;

                        let response = induction_response(local_socket_id, cookie);
                        let response_pkt = handshake_control_packet(&response, 0, ctrl.timestamp);
                        let encoded = encode_control_packet(&response_pkt);

                        if let Err(e) = self.socket.send_to(&encoded, peer).await {
                            warn!("SRT: failed to send induction response to {peer}: {e}");
                        } else {
                            debug!("SRT: induction response sent to {peer}");
                        }
                    }
                    HandshakeType::Conclusion => {
                        // Phase 2: Verify cookie and complete handshake.
                        let cookie_ok = {
                            let mut cookies = self.pending_cookies.lock().unwrap_or_else(
                                std::sync::PoisonError::into_inner,
                            );
                            let valid = cookies
                                .get(&peer)
                                .is_some_and(|(expected, issued_at)| {
                                    *expected == hs.cookie
                                        && issued_at.elapsed() <= COOKIE_TTL
                                });
                            // A conclusion redeems the cookie exactly once.
                            cookies.remove(&peer);
                            valid
                        };
                        if !cookie_ok {
                            warn!(
                                "SRT: rejected conclusion handshake from {peer} (invalid or expired cookie)"
                            );
                            return;
                        }
                        if connections.len() >= self.config.max_connections {
                            warn!(
                                "SRT: rejecting connection from {peer}: at capacity ({} connections)",
                                connections.len()
                            );
                            return;
                        }

                        let local_socket_id = *next_socket_id;
                        *next_socket_id += 1;

                        // Parse Stream ID from extension (for mount routing)
                        let stream_id = parse_stream_id(&ctrl.data);

                        // Publish authorization: reject callers whose stream
                        // id is not allowed (open mode when the list is
                        // empty). The caller learns about the rejection via
                        // a failed handshake.
                        let rejected = match &stream_id {
                            Some(actual) => {
                                !self.config.allowed_stream_ids.is_empty()
                                    && !self.config.allowed_stream_ids.iter().any(|allowed| {
                                        allowed == actual
                                            || allowed.trim_start_matches('/')
                                                == actual.trim_start_matches('/')
                                    })
                            }
                            // No stream id at all: only open mode accepts it.
                            None => !self.config.allowed_stream_ids.is_empty(),
                        };
                        if rejected {
                            warn!(
                                "SRT: rejected stream_id {stream_id:?} from {peer} (not allowed)"
                            );
                            return;
                        }

                        let response =
                            conclusion_response(local_socket_id, hs.initial_sequence, hs.socket_id);
                        let response_pkt =
                            handshake_control_packet(&response, hs.socket_id, ctrl.timestamp);
                        let encoded = encode_control_packet(&response_pkt);

                        if let Err(e) = self.socket.send_to(&encoded, peer).await {
                            warn!("SRT: failed to send conclusion response to {peer}: {e}");
                        } else {
                            info!(
                                "SRT: connection established with {peer} (stream_id={stream_id:?})"
                            );

                            let now = Instant::now();
                            let mut tsbpd = TsbpdBuffer::new(self.config.tsbpd_delay_ms);
                            tsbpd.init(hs.initial_sequence);

                            #[cfg(feature = "metrics")]
                            if let Some(metrics) = &self.config.metrics {
                                use std::sync::atomic::Ordering;
                                metrics.srt_connections_total.fetch_add(1, Ordering::Relaxed);
                            }

                            connections.insert(
                                peer,
                                SrtConnection {
                                    peer_socket_id: hs.socket_id,
                                    local_socket_id,
                                    start_time: now,
                                    state: ConnState::Established,
                                    tsbpd,
                                    demuxer: ts_demux::TsStreamDemuxer::new(),
                                    mount_path: None,
                                    mount_registered: false,
                                    mount: None,
                                    stream_id,
                                    last_recv_time: now,
                                    last_ack_time: now,
                                    last_nak_time: now,
                                    last_nak_losses: Vec::new(),
                                    ack_seq: 0,
                                    pkts_since_ack: 0,
                                },
                            );
                        }
                    }
                    _ => {
                        debug!(
                            "SRT: unhandled handshake type from {peer}: {:?}",
                            hs.handshake_type
                        );
                    }
                }
            }
            ControlType::Keepalive => {
                if let Some(conn) = connections.get(&peer) {
                    let timestamp = conn.start_time.elapsed().as_micros() as u32;
                    let response = keepalive_packet(conn.peer_socket_id, timestamp);
                    let encoded = encode_control_packet(&response);
                    let _ = self.socket.send_to(&encoded, peer).await;
                }
            }
            ControlType::AckAck => {
                // Sender acknowledged our ACK - nothing to do
                debug!("SRT: received ACKACK from {peer}");
            }
            ControlType::Shutdown => {
                info!("SRT: peer {peer} requested shutdown");
                if let Some(conn) = connections.get_mut(&peer) {
                    conn.state = ConnState::Closing;
                    #[cfg(feature = "metrics")]
                    if let Some(metrics) = &self.config.metrics
                        && conn.mount_registered
                    {
                        use std::sync::atomic::Ordering;
                        metrics.srt_streams_active.fetch_sub(1, Ordering::Relaxed);
                    }
                    if conn.mount_registered
                        && let Some(path) = conn.mount_path.clone()
                    {
                        self.mounts.unregister(&path);
                        info!("SRT: unregistered mount {path} on peer shutdown");
                    }
                }
                connections.remove(&peer);
            }
            _ => {
                debug!(
                    "SRT: unhandled control type from {peer}: {:?}",
                    ctrl.control_type
                );
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

        conn.last_recv_time = Instant::now();

        if conn.state != ConnState::Established {
            return;
        }

        // Insert into TSBPD reorder buffer (retransmit detection is
        // wrapping-aware so a packet just before a sequence-space wrap is
        // not mistaken for a retransmission of an old one). Delivery is
        // scheduled from the sender-clock timestamp, absorbing jitter.
        let is_retransmit = tsbpd::seq_before(data_pkt.sequence, conn.tsbpd.ack_number());
        if conn
            .tsbpd
            .push(
                data_pkt.sequence,
                data_pkt.payload.clone(),
                data_pkt.timestamp,
                is_retransmit,
            )
        {
            conn.pkts_since_ack += 1;
        }

        // Try to release immediately ready packets (low-latency path)
        let ready = conn.tsbpd.pop_ready();
        if !ready.is_empty() {
            for pkt in ready {
                conn.demuxer.push(&pkt.payload);
            }
            self.deliver_units(conn, peer).await;
        }
    }

    /// Deliver completed access units from the demuxer to the mount.
    async fn deliver_units(&self, conn: &mut SrtConnection, peer: SocketAddr) {
        let units = conn.demuxer.take_units();
        if units.is_empty() {
            return;
        }

        // Register mount if not done yet
        if !conn.mount_registered {
            if let Some(desc) = conn.demuxer.description() {
                // Use stream_id for mount path if available, else fallback to port-based
                let mount_path = match &conn.stream_id {
                    Some(sid) => {
                        if sid.starts_with('/') {
                            sid.clone()
                        } else {
                            format!("/{sid}")
                        }
                    }
                    None => format!("/srt/{}", peer.port()),
                };

                let config = MountConfig::Publisher {
                    description: desc,
                    allow_replace_after_disconnect: true,
                };

                match self.mounts.register(mount_path.clone().into(), config) {
                    Ok(handle) => {
                        info!("SRT: registered mount {mount_path}");
                        #[cfg(feature = "metrics")]
                        if let Some(metrics) = &self.config.metrics {
                            use std::sync::atomic::Ordering;
                            metrics.srt_streams_active.fetch_add(1, Ordering::Relaxed);
                        }
                        conn.mount_path = Some(mount_path);
                        conn.mount_registered = true;
                        conn.mount = Some(handle.inner.clone());

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
        } else if let Some(mount) = &conn.mount {
            // Fast path: deliver straight through the cached mount handle.
            for unit in units {
                let _ = mount.publish_cached(unit);
            }
        }
    }

    /// Shutdown the server.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

/// Parse Stream ID from HSv5 conclusion handshake extension.
///
/// The Stream ID is carried in a SRT_CMD_SID extension block:
/// - Extension type 0x0005 (SRT_CMD_SID)
/// - Followed by the stream ID as UTF-8 string (padded to 4-byte boundary)
///
/// Common format: `#!::u=user,r=live/stream,h=host`
/// We extract the `r=` field as the mount path.
fn parse_stream_id(hs_data: &[u8]) -> Option<String> {
    // The handshake data has a fixed 48-byte header, then extensions follow.
    // Each extension: 2 bytes type, 2 bytes length (in 32-bit words), then data.
    if hs_data.len() < 48 {
        return None;
    }

    let mut offset = 48; // skip handshake header
    while offset + 4 <= hs_data.len() {
        let ext_type = u16::from_be_bytes([hs_data[offset], hs_data[offset + 1]]);
        let ext_len_words = u16::from_be_bytes([hs_data[offset + 2], hs_data[offset + 3]]);
        let ext_len_bytes = (ext_len_words as usize) * 4;
        offset += 4;

        if offset + ext_len_bytes > hs_data.len() {
            break;
        }

        // SRT_CMD_SID = 5
        if ext_type == 5 {
            let raw = &hs_data[offset..offset + ext_len_bytes];
            // Trim null padding
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let sid = String::from_utf8_lossy(&raw[..end]).to_string();

            // Parse `#!::` format: extract r= field
            if let Some(kv_part) = sid.strip_prefix("#!::") {
                for pair in kv_part.split(',') {
                    if let Some(resource) = pair.strip_prefix("r=") {
                        return Some(resource.to_string());
                    }
                }
                // No r= field, use the whole thing after #!::
                return Some(kv_part.to_string());
            }
            return Some(sid);
        }

        offset += ext_len_bytes;
    }

    None
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

    #[test]
    fn parse_stream_id_with_resource() {
        // Build a fake handshake with SID extension
        let mut hs = vec![0u8; 48]; // header
        // SID: "#!::u=publisher,r=live/cam1" = 28 bytes, pad to 28 (7 words)
        let sid = b"#!::u=publisher,r=live/cam1";
        let padded_len = (sid.len() + 3) & !3; // round up to 4
        let words = (padded_len / 4) as u16;
        hs.extend_from_slice(&5u16.to_be_bytes()); // type = SRT_CMD_SID
        hs.extend_from_slice(&words.to_be_bytes()); // length in words
        hs.extend_from_slice(sid);
        hs.extend_from_slice(&vec![0u8; padded_len - sid.len()]); // null padding

        let result = parse_stream_id(&hs);
        assert_eq!(result, Some("live/cam1".to_string()));
    }

    #[test]
    fn parse_stream_id_plain() {
        let mut hs = vec![0u8; 48];
        hs.extend_from_slice(&5u16.to_be_bytes());
        hs.extend_from_slice(&2u16.to_be_bytes()); // 2 words = 8 bytes
        let sid = b"stream1\0"; // 8 bytes
        hs.extend_from_slice(sid);

        let result = parse_stream_id(&hs);
        assert_eq!(result, Some("stream1".to_string()));
    }

    #[test]
    fn parse_stream_id_none() {
        let hs = vec![0u8; 48]; // no extensions
        assert_eq!(parse_stream_id(&hs), None);
    }
}
