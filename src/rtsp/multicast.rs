//! Multicast RTP transport (RFC 3550 §2).
//!
//! Provides multicast group management for RTP delivery to multiple receivers
//! without per-subscriber unicast streams. Uses IGMP join/leave on the
//! underlying UDP socket.

use crate::{Error, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::net::UdpSocket;

/// Multicast group configuration.
///
/// Parsed from SDP `c=IN IP4 <group>/<ttl>` lines or constructed directly
/// for server-side multicast publishing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MulticastConfig {
    /// Multicast group address (e.g. 239.1.1.1).
    pub group: IpAddr,
    /// RTP port number.
    pub port: u16,
    /// Multicast TTL (time-to-live). Default 1 = link-local.
    pub ttl: u8,
    /// Optional local interface address to bind to.
    pub interface: Option<IpAddr>,
}

impl MulticastConfig {
    /// Create a new multicast configuration.
    pub fn new(group: IpAddr, port: u16) -> Self {
        Self {
            group,
            port,
            ttl: 1,
            interface: None,
        }
    }

    /// Set the multicast TTL.
    pub fn with_ttl(mut self, ttl: u8) -> Self {
        self.ttl = ttl;
        self
    }

    /// Set the local interface.
    pub fn with_interface(mut self, interface: IpAddr) -> Self {
        self.interface = Some(interface);
        self
    }

    /// Create a multicast UDP socket, bind it, and join the group.
    ///
    /// The socket is bound to `0.0.0.0:<port>` (required for multicast receive)
    /// and joined to the multicast group on the specified or default interface.
    pub async fn bind(&self) -> Result<UdpSocket> {
        let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), self.port);
        let domain = socket2::Domain::IPV4;
        let raw = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
            .map_err(Error::Io)?;
        // Multiple receivers on one host must be able to join the same
        // group/port (standard multicast deployment).
        raw.set_reuse_address(true).map_err(Error::Io)?;
        raw.set_nonblocking(true).map_err(Error::Io)?;
        raw.bind(&bind_addr.into()).map_err(Error::Io)?;
        let socket = UdpSocket::from_std(raw.into()).map_err(Error::Io)?;

        if let IpAddr::V4(group_v4) = self.group {
            let interface_v4 = match self.interface {
                Some(IpAddr::V4(iface)) => iface,
                _ => Ipv4Addr::UNSPECIFIED,
            };
            socket.join_multicast_v4(group_v4, interface_v4)?;
            socket.set_multicast_ttl_v4(u32::from(self.ttl))?;
        } else {
            return Err(Error::Unsupported(
                "IPv6 multicast not yet supported".into(),
            ));
        }

        Ok(socket)
    }

    /// Check if an address is a multicast address.
    pub fn is_multicast(addr: &IpAddr) -> bool {
        match addr {
            IpAddr::V4(v4) => v4.is_multicast(),
            IpAddr::V6(v6) => v6.is_multicast(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_multicast_detects_ranges() {
        assert!(MulticastConfig::is_multicast(&"239.1.1.1".parse().unwrap()));
        assert!(MulticastConfig::is_multicast(&"224.0.0.1".parse().unwrap()));
        assert!(!MulticastConfig::is_multicast(
            &"192.168.1.1".parse().unwrap()
        ));
        assert!(!MulticastConfig::is_multicast(&"10.0.0.1".parse().unwrap()));
        assert!(MulticastConfig::is_multicast(&"ff02::1".parse().unwrap()));
    }

    #[test]
    fn config_builder() {
        let cfg = MulticastConfig::new("239.1.1.1".parse().unwrap(), 5004)
            .with_ttl(16)
            .with_interface("192.168.1.100".parse().unwrap());
        assert_eq!(cfg.ttl, 16);
        assert_eq!(cfg.interface, Some("192.168.1.100".parse().unwrap()));
    }
}
