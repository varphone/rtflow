//! SDP connection-info value type (`c=` line).
//!
//! The struct lives with the media types; the multicast helpers are implemented
//! in [`crate::rtsp::sdp`], next to the RTSP transport configuration they build.

use std::net::IpAddr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SdpConnection {
    pub addr_type: String,
    pub address: IpAddr,
    pub ttl: Option<u8>,
    /// RTP port advertised by the SDP `m=` line (multicast).
    pub port: Option<u16>,
}
