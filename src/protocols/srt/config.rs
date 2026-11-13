use super::*;

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
    /// Hard cap on TSBPD reorder-buffer packets (memory bound for
    /// small-RAM IPC; see `TsbpdBuffer::new`). Excess arrivals are
    /// dropped instead of growing the reorder map.
    pub tsbpd_max_buffered: usize,
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
            tsbpd_max_buffered: tsbpd::DEFAULT_MAX_BUFFERED,
            ack_interval_ms: 10,
            keepalive_timeout: Duration::from_secs(30),
            #[cfg(feature = "metrics")]
            metrics: None,
            allowed_stream_ids: Vec::new(),
        }
    }
}
