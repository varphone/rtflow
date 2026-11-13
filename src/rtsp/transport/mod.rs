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

/// RTSP `Transport: mode=` parameter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportMode {
    Play,
    Record,
}

impl TransportMode {
    fn parse(value: &str) -> Result<Self> {
        if value.eq_ignore_ascii_case("play") {
            Ok(Self::Play)
        } else if value.eq_ignore_ascii_case("record") {
            Ok(Self::Record)
        } else {
            Err(Error::Rtsp("unsupported Transport mode".into()))
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Record => "record",
        }
    }
}

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
        /// SRTP (`RTP/SAVP`) is required for this transport.
        secure: bool,
        /// Optional multicast/unicast destination hint.
        destination: Option<IpAddr>,
        /// Optional source-address hint.
        source: Option<IpAddr>,
        /// Optional `mode=play|record`.
        mode: Option<TransportMode>,
    },
    TcpInterleaved {
        rtp_channel: u8,
        rtcp_channel: u8,
        /// RTCP is multiplexed on the RTP interleaved channel.
        rtcp_mux: bool,
        /// SRTP (`RTP/SAVP`) is required for this transport.
        secure: bool,
        /// Optional `mode=play|record`.
        mode: Option<TransportMode>,
    },
    /// Server-published multicast group (`RTP/AVP;multicast`).
    Multicast {
        destination: Option<IpAddr>,
        port: Option<u16>,
        ttl: Option<u8>,
        source: Option<IpAddr>,
        mode: Option<TransportMode>,
        /// SRTP (`RTP/SAVP;multicast`) is required for this group.
        secure: bool,
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
        let mut multicast = false;
        let mut rtcp_mux = false;
        let mut destination: Option<IpAddr> = None;
        let mut source: Option<IpAddr> = None;
        let mut mode: Option<TransportMode> = None;
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
                if multicast {
                    return Err(Error::Rtsp(
                        "duplicate multicast Transport parameter".into(),
                    ));
                }
                multicast = true;
                continue;
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
        if multicast {
            let secure = profile.eq_ignore_ascii_case("RTP/SAVP");
            if !secure && !profile.eq_ignore_ascii_case("RTP/AVP") {
                return Err(Error::Rtsp(
                    "unsupported multicast Transport profile".into(),
                ));
            }
            if unicast {
                return Err(Error::Rtsp(
                    "Transport cannot be both unicast and multicast".into(),
                ));
            }
            let destination = parameters
                .get("destination")
                .map(|value| {
                    value
                        .parse::<IpAddr>()
                        .map_err(|_| Error::Rtsp("invalid multicast destination".into()))
                })
                .transpose()?;
            let port = parameters
                .get("port")
                .map(|value| {
                    value
                        .parse::<u16>()
                        .map_err(|_| Error::Rtsp("invalid multicast port".into()))
                })
                .transpose()?;
            let ttl = parameters
                .get("ttl")
                .map(|value| {
                    value
                        .parse::<u8>()
                        .map_err(|_| Error::Rtsp("invalid multicast TTL".into()))
                })
                .transpose()?;
            let source = parameters
                .get("source")
                .map(|value| {
                    value
                        .parse::<IpAddr>()
                        .map_err(|_| Error::Rtsp("invalid multicast source".into()))
                })
                .transpose()?;
            let mode = parameters
                .get("mode")
                .map(|value| TransportMode::parse(value))
                .transpose()?;
            return Ok(Self::Multicast {
                destination,
                port,
                ttl,
                source,
                mode,
                secure,
            });
        }
        if !unicast {
            return Err(Error::Rtsp("only unicast Transport is supported".into()));
        }
        if let Some(value) = parameters.get("destination") {
            destination = Some(
                value
                    .parse()
                    .map_err(|_| Error::Rtsp("invalid Transport destination".into()))?,
            );
        }
        if let Some(value) = parameters.get("source") {
            source = Some(
                value
                    .parse()
                    .map_err(|_| Error::Rtsp("invalid Transport source".into()))?,
            );
        }
        if let Some(value) = parameters.get("mode") {
            mode = Some(TransportMode::parse(value)?);
        }

        let secure = profile.eq_ignore_ascii_case("RTP/SAVP/TCP");
        if profile.eq_ignore_ascii_case("RTP/AVP/TCP") || secure {
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
                secure,
                mode,
            });
        }
        let secure = profile.eq_ignore_ascii_case("RTP/SAVP")
            || profile.eq_ignore_ascii_case("RTP/SAVP/UDP");
        if !secure
            && !profile.eq_ignore_ascii_case("RTP/AVP")
            && !profile.eq_ignore_ascii_case("RTP/AVP/UDP")
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
            secure,
            destination,
            source,
            mode,
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
                secure,
                destination,
                source,
                mode,
            } => {
                let profile = if *secure { "RTP/SAVP" } else { "RTP/AVP" };
                let mut s = if *rtcp_mux {
                    format!("{profile};unicast;client_port={client_rtp};rtcp-mux")
                } else {
                    format!("{profile};unicast;client_port={client_rtp}-{client_rtcp}")
                };
                if let (Some(a), Some(b)) = (server_rtp, server_rtcp) {
                    if *rtcp_mux {
                        s.push_str(&format!(";server_port={a}"))
                    } else {
                        s.push_str(&format!(";server_port={a}-{b}"))
                    }
                }
                if let Some(destination) = destination {
                    s.push_str(&format!(";destination={destination}"));
                }
                if let Some(source) = source {
                    s.push_str(&format!(";source={source}"));
                }
                if let Some(mode) = mode {
                    s.push_str(&format!(";mode={}", mode.as_str()));
                }
                s
            }
            Self::TcpInterleaved {
                rtp_channel,
                rtcp_channel,
                rtcp_mux,
                secure,
                mode,
            } => {
                let profile = if *secure {
                    "RTP/SAVP/TCP"
                } else {
                    "RTP/AVP/TCP"
                };
                let mut rendered = if *rtcp_mux {
                    format!("{profile};unicast;interleaved={rtp_channel};rtcp-mux")
                } else {
                    format!("{profile};unicast;interleaved={rtp_channel}-{rtcp_channel}")
                };
                if let Some(mode) = mode {
                    rendered.push_str(&format!(";mode={}", mode.as_str()));
                }
                rendered
            }
            Self::Multicast {
                destination,
                port,
                ttl,
                source,
                mode,
                secure,
            } => {
                let mut rendered = if *secure {
                    String::from("RTP/SAVP;multicast")
                } else {
                    String::from("RTP/AVP;multicast")
                };
                if let Some(destination) = destination {
                    rendered.push_str(&format!(";destination={destination}"));
                }
                if let Some(port) = port {
                    rendered.push_str(&format!(";port={port}"));
                }
                if let Some(ttl) = ttl {
                    rendered.push_str(&format!(";ttl={ttl}"));
                }
                if let Some(source) = source {
                    rendered.push_str(&format!(";source={source}"));
                }
                if let Some(mode) = mode {
                    rendered.push_str(&format!(";mode={}", mode.as_str()));
                }
                rendered
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
    /// Set the IPv4 multicast TTL on both sockets (sender side).
    #[cfg(feature = "multicast")]
    pub fn set_multicast_ttl(&self, ttl: u8) -> Result<()> {
        let ttl = u32::from(ttl);
        socket2::SockRef::from(&*self.rtp)
            .set_multicast_ttl_v4(ttl)
            .map_err(Error::Io)?;
        socket2::SockRef::from(&*self.rtcp)
            .set_multicast_ttl_v4(ttl)
            .map_err(Error::Io)?;
        Ok(())
    }
    /// Build a pair from already-bound sockets (multicast joins).
    pub fn from_sockets(rtp: UdpSocket, rtcp: UdpSocket) -> Self {
        Self {
            rtp: Arc::new(rtp),
            rtcp: Arc::new(rtcp),
        }
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
mod tests;
