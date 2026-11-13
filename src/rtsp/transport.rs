//! Transport-independent RTP delivery primitives shared by client and server.
use crate::{Error, Result};
use bytes::{Buf, Bytes, BytesMut};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    net::{IpAddr, SocketAddr},
    ops::RangeInclusive,
    str::FromStr,
    sync::Arc,
};
use tokio::net::UdpSocket;

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TransportSpec {
    Udp {
        client_rtp: u16,
        client_rtcp: u16,
        server_rtp: Option<u16>,
        server_rtcp: Option<u16>,
        /// RTCP is multiplexed on the RTP port (`rtcp-mux`, RFC 7826 §4.1).
        rtcp_mux: bool,
    },
    TcpInterleaved {
        rtp_channel: u8,
        rtcp_channel: u8,
        /// RTCP is multiplexed on the RTP interleaved channel.
        rtcp_mux: bool,
    },
}
impl TransportSpec {
    pub fn parse(value: &str) -> Result<Self> {
        let mut alternatives = Self::parse_all(value)?;
        if alternatives.len() != 1 {
            return Err(Error::Rtsp(
                "expected exactly one Transport alternative".into(),
            ));
        }
        Ok(alternatives.remove(0))
    }

    pub fn parse_all(value: &str) -> Result<Vec<Self>> {
        let alternatives = value
            .split(',')
            .map(str::trim)
            .map(Self::parse_one)
            .collect::<Result<Vec<_>>>()?;
        if alternatives.is_empty() {
            return Err(Error::Rtsp("empty Transport".into()));
        }
        Ok(alternatives)
    }

    fn parse_one(value: &str) -> Result<Self> {
        let mut parts = value.split(';').map(str::trim);
        let profile = parts
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::Rtsp("empty Transport".into()))?;
        let mut parameters = std::collections::BTreeMap::new();
        let mut unicast = false;
        let mut rtcp_mux = false;
        for part in parts {
            if part.eq_ignore_ascii_case("rtcp-mux") {
                if rtcp_mux {
                    return Err(Error::Rtsp("duplicate rtcp-mux Transport parameter".into()));
                }
                rtcp_mux = true;
                continue;
            }
            if part.eq_ignore_ascii_case("unicast") {
                if unicast {
                    return Err(Error::Rtsp("duplicate unicast Transport parameter".into()));
                }
                unicast = true;
                continue;
            }
            if part.eq_ignore_ascii_case("multicast") {
                return Err(Error::Rtsp("multicast Transport is unsupported".into()));
            }
            let (name, value) = part
                .split_once('=')
                .ok_or_else(|| Error::Rtsp("malformed Transport parameter".into()))?;
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name.is_empty() || value.is_empty() || parameters.insert(name, value).is_some() {
                return Err(Error::Rtsp(
                    "duplicate or malformed Transport parameter".into(),
                ));
            }
        }
        if !unicast {
            return Err(Error::Rtsp("only unicast Transport is supported".into()));
        }
        if let Some(mode) = parameters.get("mode")
            && !mode.eq_ignore_ascii_case("play")
            && !mode.eq_ignore_ascii_case("record")
        {
            return Err(Error::Rtsp("unsupported Transport mode".into()));
        }

        if profile.eq_ignore_ascii_case("RTP/AVP/TCP") {
            let channels = parameters
                .get("interleaved")
                .ok_or_else(|| Error::Rtsp("TCP Transport has no interleaved channels".into()))?;
            let (rtp, rtcp) = port_or_pair(channels, rtcp_mux)?;
            return Ok(Self::TcpInterleaved {
                rtp_channel: u8::try_from(rtp)
                    .map_err(|_| Error::Rtsp("RTP interleaved channel exceeds 255".into()))?,
                rtcp_channel: u8::try_from(rtcp)
                    .map_err(|_| Error::Rtsp("RTCP interleaved channel exceeds 255".into()))?,
                rtcp_mux,
            });
        }
        if !profile.eq_ignore_ascii_case("RTP/AVP") && !profile.eq_ignore_ascii_case("RTP/AVP/UDP")
        {
            return Err(Error::Rtsp("unsupported RTP transport profile".into()));
        }
        let client = parameters
            .get("client_port")
            .ok_or_else(|| Error::Rtsp("UDP Transport has no client_port".into()))?;
        let (client_rtp, client_rtcp) = port_or_pair(client, rtcp_mux)?;
        if client_rtp == 0 {
            return Err(Error::Rtsp("UDP client_port must be non-zero".into()));
        }
        let (server_rtp, server_rtcp) = parameters
            .get("server_port")
            .map(|value| port_or_pair(value, rtcp_mux))
            .transpose()?
            .map_or((None, None), |(a, b)| (Some(a), Some(b)));
        Ok(Self::Udp {
            client_rtp,
            client_rtcp,
            server_rtp,
            server_rtcp,
            rtcp_mux,
        })
    }

    pub fn render(&self) -> String {
        match self {
            Self::Udp {
                client_rtp,
                client_rtcp,
                server_rtp,
                server_rtcp,
                rtcp_mux,
            } => {
                let mut s = if *rtcp_mux {
                    format!("RTP/AVP;unicast;client_port={client_rtp};rtcp-mux")
                } else {
                    format!("RTP/AVP;unicast;client_port={client_rtp}-{client_rtcp}")
                };
                if let (Some(a), Some(b)) = (server_rtp, server_rtcp) {
                    if *rtcp_mux {
                        s.push_str(&format!(";server_port={a}"))
                    } else {
                        s.push_str(&format!(";server_port={a}-{b}"))
                    }
                }
                s
            }
            Self::TcpInterleaved {
                rtp_channel,
                rtcp_channel,
                rtcp_mux,
            } => {
                if *rtcp_mux {
                    format!("RTP/AVP/TCP;unicast;interleaved={rtp_channel};rtcp-mux")
                } else {
                    format!("RTP/AVP/TCP;unicast;interleaved={rtp_channel}-{rtcp_channel}")
                }
            }
        }
    }
}
fn port_or_pair(s: &str, allow_single: bool) -> Result<(u16, u16)> {
    if let Some((a, b)) = s.split_once('-') {
        let a = u16::from_str(a.trim()).map_err(|_| Error::Rtsp("invalid RTP port".into()))?;
        let b = u16::from_str(b.trim()).map_err(|_| Error::Rtsp("invalid RTCP port".into()))?;
        if b != a.checked_add(1).unwrap_or_default() {
            return Err(Error::Rtsp(
                "RTP/RTCP values must be an adjacent pair".into(),
            ));
        }
        return Ok((a, b));
    }
    if allow_single {
        let value = u16::from_str(s.trim()).map_err(|_| Error::Rtsp("invalid RTP port".into()))?;
        return Ok((value, value));
    }
    Err(Error::Rtsp("port pair must use '-'".into()))
}
#[derive(Clone, Debug)]
pub struct InterleavedFrame {
    pub channel: u8,
    pub payload: Bytes,
}
impl InterleavedFrame {
    pub fn try_encode(&self) -> Result<Bytes> {
        let length =
            u16::try_from(self.payload.len()).map_err(|_| Error::Limit("interleaved RTP frame"))?;
        let mut out = BytesMut::with_capacity(4 + self.payload.len());
        out.extend_from_slice(&[b'$', self.channel]);
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(&self.payload);
        Ok(out.freeze())
    }

    pub fn encode(&self) -> Bytes {
        self.try_encode()
            .expect("interleaved payload must fit its 16-bit wire length")
    }
    pub fn parse(buf: &mut BytesMut, max: usize) -> Result<Option<Self>> {
        if buf.first() != Some(&b'$') {
            return Ok(None);
        }
        if buf.len() < 4 {
            return Ok(None);
        }
        let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        if len > max {
            return Err(Error::Limit("interleaved RTP frame"));
        }
        if buf.len() < len + 4 {
            return Ok(None);
        }
        let channel = buf[1];
        buf.advance(4);
        let payload = buf.split_to(len).freeze();
        Ok(Some(Self { channel, payload }))
    }
}
#[derive(Clone, Debug)]
pub struct UdpPair {
    pub rtp: Arc<UdpSocket>,
    pub rtcp: Arc<UdpSocket>,
}
const UDP_RECEIVE_BUFFER_SIZE: usize = 4 * 1024 * 1024;

fn bind_udp(addr: SocketAddr, recv_buffer: usize) -> Result<UdpSocket> {
    let domain = match addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(Error::Io)?;
    socket.set_nonblocking(true).map_err(Error::Io)?;
    socket
        .set_recv_buffer_size(recv_buffer)
        .map_err(Error::Io)?;
    socket.bind(&addr.into()).map_err(Error::Io)?;
    UdpSocket::from_std(socket.into()).map_err(Error::Io)
}

impl UdpPair {
    pub async fn bind(ip: IpAddr, range: Option<RangeInclusive<u16>>) -> Result<Self> {
        Self::bind_with_recv_buffer(ip, range, UDP_RECEIVE_BUFFER_SIZE).await
    }

    /// Like [`Self::bind`] with an explicit socket receive-buffer size.
    /// Memory-constrained clients can trade burst tolerance for a smaller
    /// footprint (the default is 4MiB per socket).
    pub async fn bind_with_recv_buffer(
        ip: IpAddr,
        range: Option<RangeInclusive<u16>>,
        recv_buffer: usize,
    ) -> Result<Self> {
        if let Some(range) = range {
            for rtp_port in range.clone() {
                let Some(rtcp_port) = rtp_port.checked_add(1) else {
                    continue;
                };
                if !range.contains(&rtcp_port) {
                    continue;
                }
                if let Ok(rtp) = bind_udp(SocketAddr::new(ip, rtp_port), recv_buffer)
                    && let Ok(rtcp) = bind_udp(SocketAddr::new(ip, rtcp_port), recv_buffer)
                {
                    return Ok(Self {
                        rtp: Arc::new(rtp),
                        rtcp: Arc::new(rtcp),
                    });
                }
            }
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "no free RTP/RTCP port pair",
            )));
        }
        for _ in 0..64 {
            let rtp = bind_udp(SocketAddr::new(ip, 0), recv_buffer)?;
            let port = rtp.local_addr()?.port();
            let Some(rtcp_port) = port.checked_add(1) else {
                continue;
            };
            if let Ok(rtcp) = bind_udp(SocketAddr::new(ip, rtcp_port), recv_buffer) {
                return Ok(Self {
                    rtp: Arc::new(rtp),
                    rtcp: Arc::new(rtcp),
                });
            }
        }
        Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "unable to allocate adjacent RTP/RTCP ports",
        )))
    }
    /// Bind a single UDP socket shared by RTP and RTCP (`rtcp-mux`).
    pub async fn bind_mux(
        ip: IpAddr,
        range: Option<RangeInclusive<u16>>,
        recv_buffer: usize,
    ) -> Result<Self> {
        let socket = if let Some(range) = range {
            range
                .into_iter()
                .find_map(|port| bind_udp(SocketAddr::new(ip, port), recv_buffer).ok())
                .ok_or_else(|| {
                    Error::Io(std::io::Error::new(
                        std::io::ErrorKind::AddrNotAvailable,
                        "no free RTP port",
                    ))
                })?
        } else {
            bind_udp(SocketAddr::new(ip, 0), recv_buffer)?
        };
        let socket = Arc::new(socket);
        Ok(Self {
            rtp: socket.clone(),
            rtcp: socket,
        })
    }
    pub fn rtp_port(&self) -> Result<u16> {
        Ok(self.rtp.local_addr()?.port())
    }
    pub fn rtcp_port(&self) -> Result<u16> {
        Ok(self.rtcp.local_addr()?.port())
    }
}

#[derive(Clone, Debug)]
pub enum PeerTransport {
    Udp(SocketAddr),
    Interleaved { rtp_channel: u8, rtcp_channel: u8 },
}

/// Upstream transport selection for proxy and client connections.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamTransport {
    /// Attempt UDP first, then reconnect and use TCP interleaving if SETUP fails.
    Auto,
    Udp,
    TcpInterleaved,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn udp_roundtrip() {
        let t = TransportSpec::parse("RTP/AVP;unicast;client_port=1000-1001").unwrap();
        assert_eq!(t.render(), "RTP/AVP;unicast;client_port=1000-1001");
        assert_eq!(
            TransportSpec::parse("RTP/AVP/UDP;unicast;client_port=1000-1001").unwrap(),
            t
        );
    }
    #[test]
    fn parses_transport_alternatives_in_client_order() {
        let alternatives = TransportSpec::parse_all(
            "RTP/AVP;unicast;client_port=5000-5001, RTP/AVP/TCP;unicast;interleaved=0-1",
        )
        .unwrap();
        assert!(matches!(alternatives[0], TransportSpec::Udp { .. }));
        assert!(matches!(
            alternatives[1],
            TransportSpec::TcpInterleaved { .. }
        ));
        assert!(
            TransportSpec::parse(
                "RTP/AVP;unicast;client_port=5000-5001, RTP/AVP/TCP;unicast;interleaved=0-1"
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_invalid_interleaved_bounds() {
        assert!(TransportSpec::parse("RTP/AVP/TCP;interleaved=256-257").is_err());
        assert!(
            InterleavedFrame {
                channel: 0,
                payload: Bytes::from(vec![0; u16::MAX as usize + 1]),
            }
            .try_encode()
            .is_err()
        );
    }
    #[test]
    fn interleaved_roundtrip() {
        let mut b = BytesMut::from(&b"$\x00\x00\x01x"[..]);
        assert_eq!(
            InterleavedFrame::parse(&mut b, 10)
                .unwrap()
                .unwrap()
                .payload,
            b"x"[..]
        )
    }
}
