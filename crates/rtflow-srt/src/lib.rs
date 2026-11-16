//! SRT (Secure Reliable Transport) protocol support.
//!
//! Provides an SRT listener that accepts caller connections and ingests
//! MPEG-TS streams into the mount registry.
//!
//! # Features
//!
//! - HSv5 listener, caller (pull) and sender (push) roles, byte-compatible
//!   with libsrt
//! - HSREQ/HSRSP negotiation (TSBPD latency = max of both sides)
//! - HaiCrypt AES-CTR encryption (passphrase, 128/192/256-bit keys)
//! - Stream ID (`#!::r=...` or a plain path, optional `?token=`) routing
//! - Full ACK / ACKACK with RTT estimation, NAK loss reports, keepalive,
//!   shutdown and DROPREQ handling
//! - TSBPD delivery with TLPKTDROP and fake ACKs for skipped packets
//! - MPEG-TS demuxing to AccessUnit and mount registration
//!
//! # Limitations
//!
//! - No rendezvous mode (peer-to-peer simultaneous connect; the gateway
//!   always has a reachable listener or connects to one)
//! - No FEC packet filter (libsrt's optional forward-error-correction is
//!   negotiated only when both sides enable it; the gateway relies on ARQ)
//! - No group/bonding (multiple connections treated as one stream)
//! - Key refresh is supported on receipt (mid-stream KMREQ) and by the
//!   sender (`SrtCallerConfig::key_refresh_packets`)
//! - MPEG-TS payloads only (other payloads are consumed but not exposed)
//!
//! # Example
//!
//! ```no_run
//! use rtflow_srt::SrtServer;
//! use rtflow_mount::MountRegistry;
//! use std::sync::Arc;
//!
//! # async fn example() -> rtflow_core::error::Result<()> {
//! let mounts = Arc::new(MountRegistry::new(100));
//! let server = SrtServer::bind("0.0.0.0:9000", mounts).await?;
//! server.run().await?;
//! # Ok(())
//! # }
//! ```
pub mod caller;
pub mod crypto;
pub mod handshake;
pub mod packet;
pub mod sender;
pub mod ts_demux;
pub mod tsbpd;

use packet::{SrtPacket, parse_packet_borrowed};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use tsbpd::TsbpdBuffer;

use rtflow_core::error::{Error, Result};
use rtflow_mount::{MountConfig, MountRegistry};

pub struct SrtServer {
    socket: Arc<UdpSocket>,
    mounts: Arc<MountRegistry>,
    config: SrtConfig,
    cancel: CancellationToken,
    /// SYN cookies issued during induction, pending conclusion verification.
    pending_cookies: std::sync::Mutex<HashMap<SocketAddr, PendingCookie>>,
    /// Snapshot of live connections, refreshed by the maintenance loop.
    stats: Arc<std::sync::RwLock<Vec<SrtConnectionStats>>>,
}

/// Per-connection SRT statistics exposed for monitoring.
#[derive(Clone, Debug)]
pub struct SrtConnectionStats {
    pub peer: SocketAddr,
    pub stream_id: Option<String>,
    pub mount: Option<String>,
    pub encrypted: bool,
    /// Negotiated receiver latency, milliseconds.
    pub latency_ms: u32,
    /// Smoothed round-trip time, microseconds.
    pub rtt_us: u32,
    pub packets_received: u64,
    pub bytes_received: u64,
    /// Bytes sent (sender role only).
    pub bytes_sent: u64,
    pub delivered: u64,
    pub skipped: u64,
    pub loss_events: u64,
    pub undecryptable: u64,
    pub duration_secs: u64,
    pub srt_version: u32,
}

/// Handshake state kept between the induction and conclusion messages.
pub(crate) struct PendingCookie {
    pub(crate) cookie: u32,
    pub(crate) listener_socket_id: u32,
    pub(crate) caller_socket_id: u32,
    pub(crate) issued_at: Instant,
}

/// How long an issued SYN cookie stays valid before it must be redeemed.
const COOKIE_TTL: Duration = Duration::from_secs(30);

mod config;
mod connection;
mod server;
mod session;
#[cfg(test)]
mod tests;

pub use caller::{SrtCaller, SrtCallerConfig};
pub use config::{PublishAuthenticator, SrtConfig};
pub use connection::SrtConnection;
pub use sender::SrtSender;
