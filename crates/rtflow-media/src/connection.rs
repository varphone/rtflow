//! SDP connection-info value type (`c=` line).
//!
//! The multicast helpers sit behind the `multicast` feature; the `rtflow-multicast` crate.

use std::net::IpAddr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SdpConnection {
    pub addr_type: String,
    pub address: IpAddr,
    pub ttl: Option<u8>,
    /// RTP port advertised by the SDP `m=` line (multicast).
    pub port: Option<u16>,
}

impl SdpConnection {
    pub fn is_multicast(&self) -> bool {
        rtflow_multicast::MulticastConfig::is_multicast(&self.address)
    }
    pub fn to_multicast_config(&self, port: u16) -> Option<rtflow_multicast::MulticastConfig> {
        if !self.is_multicast() {
            return None;
        }
        let mut cfg = rtflow_multicast::MulticastConfig::new(self.address, port);
        if let Some(ttl) = self.ttl {
            cfg = cfg.with_ttl(ttl);
        }
        Some(cfg)
    }
}
