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
pub enum TransportSpec {
    Udp {
        client_rtp: u16,
        client_rtcp: u16,
        server_rtp: Option<u16>,
        server_rtcp: Option<u16>,
    },
    TcpInterleaved {
        rtp_channel: u8,
        rtcp_channel: u8,
    },
}
impl TransportSpec {
    pub fn parse(value: &str) -> Result<Self> {
        let parts: Vec<_> = value.split(';').map(str::trim).collect();
        let profile = parts
            .first()
            .ok_or_else(|| Error::Rtsp("empty Transport".into()))?;
        if profile.eq_ignore_ascii_case("RTP/AVP/TCP") {
            let channels = parts
                .iter()
                .find_map(|p| p.strip_prefix("interleaved="))
                .ok_or_else(|| Error::Rtsp("TCP Transport has no interleaved channels".into()))?;
            let (rtp, rtcp) = port_pair(channels)?;
            return Ok(Self::TcpInterleaved {
                rtp_channel: u8::try_from(rtp)
                    .map_err(|_| Error::Rtsp("RTP interleaved channel exceeds 255".into()))?,
                rtcp_channel: u8::try_from(rtcp)
                    .map_err(|_| Error::Rtsp("RTCP interleaved channel exceeds 255".into()))?,
            });
        }
        if !profile.eq_ignore_ascii_case("RTP/AVP") && !profile.eq_ignore_ascii_case("RTP/AVP/UDP")
        {
            return Err(Error::Rtsp("unsupported RTP transport profile".into()));
        }
        let client = parts
            .iter()
            .find_map(|p| p.strip_prefix("client_port="))
            .ok_or_else(|| Error::Rtsp("UDP Transport has no client_port".into()))?;
        let (client_rtp, client_rtcp) = port_pair(client)?;
        let (server_rtp, server_rtcp) = parts
            .iter()
            .find_map(|p| p.strip_prefix("server_port="))
            .map(port_pair)
            .transpose()?
            .map_or((None, None), |(a, b)| (Some(a), Some(b)));
        Ok(Self::Udp {
            client_rtp,
            client_rtcp,
            server_rtp,
            server_rtcp,
        })
    }
    pub fn render(&self) -> String {
        match self {
            Self::Udp {
                client_rtp,
                client_rtcp,
                server_rtp,
                server_rtcp,
            } => {
                let mut s = format!("RTP/AVP;unicast;client_port={client_rtp}-{client_rtcp}");
                if let (Some(a), Some(b)) = (server_rtp, server_rtcp) {
                    s.push_str(&format!(";server_port={a}-{b}"))
                };
                s
            }
            Self::TcpInterleaved {
                rtp_channel,
                rtcp_channel,
            } => format!("RTP/AVP/TCP;unicast;interleaved={rtp_channel}-{rtcp_channel}"),
        }
    }
}
fn port_pair(s: &str) -> Result<(u16, u16)> {
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| Error::Rtsp("port pair must use '-'".into()))?;
    let a = u16::from_str(a).map_err(|_| Error::Rtsp("invalid RTP port".into()))?;
    let b = u16::from_str(b).map_err(|_| Error::Rtsp("invalid RTCP port".into()))?;
    Ok((a, b))
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

fn bind_udp(addr: SocketAddr) -> Result<UdpSocket> {
    let domain = match addr {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).map_err(Error::Io)?;
    socket.set_nonblocking(true).map_err(Error::Io)?;
    socket
        .set_recv_buffer_size(UDP_RECEIVE_BUFFER_SIZE)
        .map_err(Error::Io)?;
    socket.bind(&addr.into()).map_err(Error::Io)?;
    UdpSocket::from_std(socket.into()).map_err(Error::Io)
}

impl UdpPair {
    pub async fn bind(ip: IpAddr, range: Option<RangeInclusive<u16>>) -> Result<Self> {
        if let Some(range) = range {
            for rtp_port in range.clone() {
                let Some(rtcp_port) = rtp_port.checked_add(1) else {
                    continue;
                };
                if !range.contains(&rtcp_port) {
                    continue;
                }
                if let Ok(rtp) = bind_udp(SocketAddr::new(ip, rtp_port))
                    && let Ok(rtcp) = bind_udp(SocketAddr::new(ip, rtcp_port))
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
            let rtp = bind_udp(SocketAddr::new(ip, 0))?;
            let port = rtp.local_addr()?.port();
            let Some(rtcp_port) = port.checked_add(1) else {
                continue;
            };
            if let Ok(rtcp) = bind_udp(SocketAddr::new(ip, rtcp_port)) {
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
