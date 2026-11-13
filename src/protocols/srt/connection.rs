use super::*;

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
    pub(super) peer_socket_id: u32,
    /// Our socket ID for this connection.
    pub(super) local_socket_id: u32,
    /// Connection start time for timestamp calculation.
    pub(super) start_time: Instant,
    /// Connection state.
    pub(super) state: ConnState,
    /// TSBPD reorder buffer.
    pub(super) tsbpd: TsbpdBuffer,
    /// TS demuxer for this connection.
    pub(super) demuxer: ts_demux::TsStreamDemuxer,
    /// Mount path for this stream.
    pub(super) mount_path: Option<String>,
    /// Whether we've registered the mount.
    pub(super) mount_registered: bool,
    /// Cached mount for this connection (avoids a registry lookup + hash
    /// on every delivered packet).
    pub(super) mount: Option<Arc<crate::mount::Mount>>,
    /// Stream ID parsed from handshake (for mount routing).
    pub(super) stream_id: Option<String>,
    /// Last time we received any packet (for keepalive timeout).
    pub(super) last_recv_time: Instant,
    /// Last time we sent an ACK.
    pub(super) last_ack_time: Instant,
    /// Monotonically increasing ACK sequence number.
    pub(super) ack_seq: u32,
    /// Packets received since last ACK.
    pub(super) pkts_since_ack: u32,
    /// Last NAK send time (throttle).
    pub(super) last_nak_time: Instant,
    /// Loss ranges already reported via NAK; unchanged gaps are not
    /// re-reported (a peer that never retransmits must not be flooded).
    pub(super) last_nak_losses: Vec<(u32, u32)>,
}
