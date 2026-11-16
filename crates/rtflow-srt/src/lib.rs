//! SRT (Secure Reliable Transport) protocol support.
//!
//! Provides an SRT listener that accepts caller connections and ingests
//! MPEG-TS streams into the mount registry.
//!
//! # Features
//!
//! - HSv5 caller-listener handshake, byte-compatible with libsrt 1.4/1.5
//! - HSREQ/HSRSP negotiation (TSBPD latency = max of both sides)
//! - Stream ID (`#!::r=...` or a plain path, optional `?token=`) routing
//! - Full ACK / ACKACK with RTT estimation, NAK loss reports, keepalive,
//!   shutdown and DROPREQ handling
//! - TSBPD delivery with TLPKTDROP and fake ACKs for skipped packets
//! - MPEG-TS demuxing to AccessUnit and mount registration
//!
//! # Limitations
//!
//! - Listener mode only (no caller/rendezvous)
//! - No encryption: connections advertising a cipher are rejected with
//!   `SRT_REJ_CRYPTO`
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
pub mod handshake;
pub mod packet;
pub mod ts_demux;
pub mod tsbpd;

use handshake::{
    Handshake, HandshakeType, HsExtFields, HsExtension, HsFlags, SRT_CMD_FILTER, SRT_CMD_GROUP,
    SRT_VERSION, SRT_VERSION_4, SRT_VERSION_5, conclusion_response, generate_cookie,
    handshake_control_packet, induction_response, parse_handshake, reject, rejection_response,
};
use packet::{
    AckFields, ControlPacket, ControlType, SrtPacket, ack_packet, encode_control_packet,
    keepalive_packet, nak_packet, parse_packet_borrowed, shutdown_packet,
};
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

pub use config::{PublishAuthenticator, SrtConfig};
pub use connection::SrtConnection;
