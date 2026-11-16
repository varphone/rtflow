use super::*;

/// SRT connection state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConnState {
    /// Data transfer active.
    Established,
    /// Shutdown requested, draining.
    Closing,
}

/// Per-connection receiver state.
pub struct SrtConnection {
    /// Peer socket ID (destination for our packets).
    pub(super) peer_socket_id: u32,
    /// Our socket ID (destination for the peer's packets).
    pub(super) local_socket_id: u32,
    /// Connection start time for timestamp calculation.
    pub(super) start_time: Instant,
    pub(super) state: ConnState,
    /// TSBPD reorder buffer.
    pub(super) tsbpd: TsbpdBuffer,
    /// TS demuxer for this connection.
    pub(super) demuxer: ts_demux::TsStreamDemuxer,
    /// Mount path for this stream.
    pub(super) mount_path: Option<String>,
    /// Whether the mount has been registered.
    pub(super) mount_registered: bool,
    /// Cached mount for this connection.
    pub(super) mount: Option<Arc<rtflow_mount::Mount>>,
    /// Publisher lease held for the lifetime of the connection, so the mount
    /// reports `publisher_connected`.
    pub(super) publisher: Option<rtflow_mount::LocalPublisher>,
    /// AES-CTR receiver state when the connection is encrypted.
    pub(super) crypto: Option<crate::crypto::SessionCrypto>,
    /// Packets dropped because they were not decryptable.
    pub(super) undecryptable: u64,
    /// Raw Stream ID from the handshake (may carry `?token=...`).
    pub(super) stream_id: Option<String>,
    /// Peer capabilities negotiated in HSREQ.
    pub(super) peer_caps: HandshakeInfo,
    /// Negotiated receiver latency, milliseconds.
    pub(super) latency_ms: u32,
    pub(super) last_recv_time: Instant,
    pub(super) last_sent_time: Instant,
    pub(super) last_keepalive_time: Instant,
    /// Last ACK / NAK send times.
    pub(super) last_ack_time: Instant,
    pub(super) last_nak_time: Instant,
    /// Packets accepted since the last ACK.
    pub(super) pkts_since_ack: u32,
    /// Monotonic ACK sequence (starts at 1).
    pub(super) ack_seq: u32,
    /// ACK sequence and send time awaiting ACKACK (RTT probe).
    pub(super) last_ack_sent: Option<(u32, Instant)>,
    /// Smoothed RTT / variance in microseconds.
    pub(super) rtt_us: u32,
    pub(super) rtt_var_us: u32,
    /// Receive counters for ACK rate estimates.
    pub(super) packets_received: u64,
    pub(super) bytes_received: u64,
    /// Loss ranges already reported via NAK.
    pub(super) last_nak_losses: Vec<(u32, u32)>,
}

/// Capabilities negotiated during the handshake.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct HandshakeInfo {
    pub(super) srt_version: u32,
    pub(super) flags: u32,
    pub(super) recv_latency_ms: u16,
    pub(super) send_latency_ms: u16,
}
