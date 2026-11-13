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
    parse_packet_borrowed,
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

mod config;
mod connection;
mod server;
mod session;
#[cfg(test)]
mod tests;

pub use config::SrtConfig;
pub use connection::SrtConnection;
