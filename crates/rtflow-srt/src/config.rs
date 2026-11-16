use super::*;

/// SRT listener configuration.
#[derive(Clone, Debug)]
pub struct SrtConfig {
    /// Bind address.
    pub bind_addr: SocketAddr,
    /// Maximum concurrent connections.
    pub max_connections: usize,
    /// Receive buffer size.
    pub recv_buffer_size: usize,
    /// Receiver TSBPD latency in milliseconds. The effective latency of a
    /// connection is `max(this, peer_receiver_latency)`; `0` accepts the
    /// peer's value (and 0 in both directions disables the buffer delay).
    pub latency_ms: u32,
    /// Advertised MTU.
    pub mtu: u32,
    /// Advertised flow window (packets in flight).
    pub flow_window: u32,
    /// Drop packets that missed their play time (TLPKTDROP).
    pub tlpktdrop: bool,
    /// Keepalive interval when no other packet has been sent.
    pub keepalive_interval: Duration,
    /// Disconnect a peer that sent nothing for this long.
    pub peer_idle_timeout: Duration,
    /// ACK interval (full ACK cadence).
    pub ack_interval_ms: u64,
    /// NAK interval (loss reports).
    pub nak_interval_ms: u64,
    /// Hard cap on TSBPD reorder-buffer packets.
    pub tsbpd_max_buffered: usize,
    /// Optional metrics registry for protocol-level counters.
    #[cfg(feature = "metrics")]
    pub metrics: Option<Arc<rtflow_core::metrics::MetricsRegistry>>,
    /// Allowed stream ids for publish (SRT `streamid` parameter). Empty
    /// means open mode.
    pub allowed_stream_ids: Vec<String>,
    /// Optional publish authorization (HMAC token, API key, path policy).
    pub authenticator: Option<Arc<dyn PublishAuthenticator>>,
}

/// SRT publish authorization hook.
///
/// The stream id is passed exactly as received (`live/cam` or
/// `live/cam?token=...`), so implementations can extract credentials and
/// apply the same policy as the other protocols.
pub trait PublishAuthenticator: Send + Sync + std::fmt::Debug {
    fn authorize(&self, stream_id: &str, peer: SocketAddr) -> bool;
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:9000".parse().unwrap(),
            max_connections: 10,
            recv_buffer_size: 8192 * 188,
            latency_ms: 120,
            mtu: 1500,
            flow_window: 8192,
            tlpktdrop: true,
            keepalive_interval: Duration::from_secs(1),
            peer_idle_timeout: Duration::from_secs(30),
            ack_interval_ms: 10,
            nak_interval_ms: 20,
            tsbpd_max_buffered: tsbpd::DEFAULT_MAX_BUFFERED,
            #[cfg(feature = "metrics")]
            metrics: None,
            allowed_stream_ids: Vec::new(),
            authenticator: None,
        }
    }
}
