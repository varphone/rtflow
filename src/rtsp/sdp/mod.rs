use crate::{
    Error, Result,
    media::{
        Codec, MediaDescription, MediaKind, RtcpAddress, RtxConfig, SdpDirection, TrackDescription,
    },
};
use std::{collections::HashSet, sync::Arc};

/// SDP connection information (RFC 4566 §5.7).
#[cfg(feature = "multicast")]
#[cfg(feature = "multicast")]
pub use crate::media::connection::SdpConnection;
#[cfg(feature = "multicast")]
impl SdpConnection {
    pub fn is_multicast(&self) -> bool {
        crate::multicast::MulticastConfig::is_multicast(&self.address)
    }
    pub fn to_multicast_config(&self, port: u16) -> Option<crate::multicast::MulticastConfig> {
        if !self.is_multicast() {
            return None;
        }
        let mut cfg = crate::multicast::MulticastConfig::new(self.address, port);
        if let Some(ttl) = self.ttl {
            cfg = cfg.with_ttl(ttl);
        }
        Some(cfg)
    }
}

/// SDP crypto attribute (RFC 4568).
#[cfg(feature = "srtp")]
#[cfg(feature = "srtp")]
pub use crate::media::crypto::SdpCrypto;

/// Bounds applied while parsing an SDP description.
#[derive(Clone, Copy, Debug)]
pub struct SdpLimits {
    /// Maximum number of media sections (tracks) accepted.
    pub max_tracks: usize,
}

impl Default for SdpLimits {
    fn default() -> Self {
        Self { max_tracks: 64 }
    }
}

pub fn parse(data: &[u8]) -> Result<MediaDescription> {
    parse_with_limits(data, SdpLimits::default())
}

/// Parse SDP with explicit resource bounds.
pub fn parse_with_limits(data: &[u8], limits: SdpLimits) -> Result<MediaDescription> {
    if limits.max_tracks == 0 {
        return Err(Error::Limit("SDP tracks"));
    }
    let text = std::str::from_utf8(data).map_err(|_| Error::Rtsp("SDP is not UTF-8".into()))?;
    let mut name = Arc::<str>::from("RTSP stream");
    // Session-level a=control (RFC 2326) seen before any m= line; media
    // controls are resolved relative to it.
    let mut session_control: Option<String> = None;
    #[cfg(feature = "multicast")]
    let mut connection: Option<SdpConnection> = None;
    let mut tracks = Vec::new();
    let mut current: Option<TrackDescription> = None;
    let mut mapped_payloads = HashSet::new();
    let mut payload_candidates: HashSet<u8> = HashSet::new();
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("s=") {
            name = Arc::from(value)
        } else if let Some(_value) = line.strip_prefix("c=") {
            #[cfg(feature = "multicast")]
            {
                connection = Some(parse_connection(_value)?);
            }
        } else if let Some(value) = line.strip_prefix("b=AS:") {
            if let Some(track) = current.as_mut()
                && let Ok(bandwidth) = value.trim().parse::<u32>()
            {
                track.bandwidth_as = Some(bandwidth);
            }
        } else if let Some(value) = line.strip_prefix("m=") {
            if tracks.len() + usize::from(current.is_some()) >= limits.max_tracks {
                return Err(Error::Limit("SDP tracks"));
            }
            if let Some(track) = current.take() {
                tracks.push(track)
            }
            let fields: Vec<_> = value.split_whitespace().collect();
            if fields.len() < 4 {
                continue;
            }
            let kind = match fields[0] {
                "video" => MediaKind::Video,
                "audio" => MediaKind::Audio,
                _ => continue,
            };
            // RTSP uses RTP/AVP; RTP/SAVP appears when SRTP is offered. Other
            // profiles (e.g. WebRTC UDP/TLS/RTP/SAVPF) are not RTSP media.
            let protocol = fields[2].to_ascii_uppercase();
            if !matches!(protocol.as_str(), "RTP/AVP" | "RTP/SAVP") {
                continue;
            }
            let payloads: Vec<u8> = fields[3..]
                .iter()
                .filter_map(|value| value.parse::<u8>().ok())
                .filter(|payload| *payload <= 127)
                .collect();
            let Some(&payload_type) = payloads.first() else {
                continue;
            };
            #[cfg(feature = "multicast")]
            if let Some(connection) = connection.as_mut()
                && connection.address.is_multicast()
                && connection.port.is_none()
                && let Ok(port) = fields[1].parse::<u16>()
                && port > 0
            {
                connection.port = Some(port);
            }
            payload_candidates = payloads.into_iter().collect();
            let (codec, clock_rate, channels) = match (kind, payload_type) {
                (MediaKind::Audio, 0) => (Codec::Pcmu, 8_000, Some(1)),
                (MediaKind::Audio, 8) => (Codec::Pcma, 8_000, Some(1)),
                (MediaKind::Audio, 14) => (Codec::Mp3, 90_000, None),
                (MediaKind::Video, 34) => (Codec::H263, 90_000, None),
                (MediaKind::Video, 26) => (Codec::Jpeg, 90_000, None),
                (MediaKind::Video, value) if value >= 96 => (Codec::H264, 90_000, None),
                (MediaKind::Audio, value) if value >= 96 => (Codec::Aac, 48_000, Some(2)),
                _ => {
                    return Err(Error::Rtsp(format!(
                        "unsupported static RTP payload type {payload_type} for {kind:?}"
                    )));
                }
            };
            current = Some(TrackDescription {
                id: tracks.len() as u8,
                kind,
                codec,
                clock_rate,
                channels,
                payload_type,
                fmtp: None,
                control: Arc::from(""),
                width: None,
                height: None,
                frame_rate: None,
                #[cfg(feature = "srtp")]
                crypto: None,
                direction: None,
                ptime: None,
                rtcp: None,
                bandwidth_as: None,
                rtx: None,
            });
        } else if let Some(value) = line.strip_prefix("a=rtpmap:") {
            let (pt, encoding) = value
                .split_once(' ')
                .ok_or_else(|| Error::Rtsp("bad rtpmap".into()))?;
            let parts: Vec<_> = encoding.split('/').collect();
            let pt: u8 = pt
                .parse()
                .map_err(|_| Error::Rtsp("invalid rtpmap payload type".into()))?;
            if let Some(track) = current.as_mut()
                && payload_candidates.contains(&pt)
            {
                if parts[0].eq_ignore_ascii_case("RTX") {
                    track.rtx = Some(RtxConfig {
                        payload_type: pt,
                        ssrc: 0,
                        apt: 0,
                    });
                    continue;
                }
                track.payload_type = pt;
                track.codec = match parts[0].to_ascii_uppercase().as_str() {
                    "H264" => Codec::H264,
                    "H265" | "HEVC" => Codec::H265,
                    "MPEG4-GENERIC" => Codec::Aac,
                    "PCMA" => Codec::Pcma,
                    "PCMU" => Codec::Pcmu,
                    "OPUS" => Codec::Opus,
                    "VP8" => Codec::Vp8,
                    "VP9" => Codec::Vp9,
                    "MPA" | "MP3" | "MPEG-AUDIO" => Codec::Mp3,
                    "G726-16" => Codec::G726_16,
                    "G726-24" => Codec::G726_24,
                    "G726-32" => Codec::G726_32,
                    "G726-40" => Codec::G726_40,
                    "AV1" => Codec::Av1,
                    "JPEG" => Codec::Jpeg,
                    "H263-1998" | "H263" | "H263-2000" => Codec::H263,
                    "MP4V-ES" => Codec::Mpeg4,
                    _ => return Err(Error::Rtsp("unsupported SDP codec".into())),
                };
                track.clock_rate = parts
                    .get(1)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(track.clock_rate);
                track.channels = parts.get(2).and_then(|value| value.parse().ok());
                mapped_payloads.insert(track.payload_type);
            }
        } else if let Some(value) = line.strip_prefix("a=") {
            let value = value.trim();
            if let Some(track) = current.as_mut() {
                if let Some(direction) = SdpDirection::parse(value) {
                    track.direction = Some(direction);
                } else if let Some(ptime) = value.strip_prefix("ptime:") {
                    track.ptime = ptime.trim().parse::<u32>().ok().filter(|value| *value > 0);
                } else if let Some(rtcp) = value.strip_prefix("rtcp:") {
                    let mut fields = rtcp.split_whitespace();
                    if let Some(port) = fields.next().and_then(|value| value.parse::<u16>().ok()) {
                        let address = fields
                            .find(|field| field.parse::<std::net::IpAddr>().is_ok())
                            .and_then(|field| field.parse::<std::net::IpAddr>().ok());
                        track.rtcp = Some(RtcpAddress { port, address });
                    }
                } else if let Some(framerate) = value.strip_prefix("framerate:") {
                    if let Ok(fps) = framerate.trim().parse::<f64>()
                        && fps.is_finite()
                        && fps > 0.0
                        && fps <= 1_000.0
                    {
                        let numerator = (fps * 1_000.0).round() as u32;
                        let divisor = gcd(numerator, 1_000);
                        track.frame_rate = Some((numerator / divisor, 1_000 / divisor));
                    }
                } else if let Some(framesize) = value.strip_prefix("framesize:") {
                    let mut fields = framesize.split_whitespace();
                    if let (Some(_pt), Some(size)) = (fields.next(), fields.next())
                        && let Some((width, height)) = size.split_once('-')
                    {
                        track.width = width.parse().ok();
                        track.height = height.parse().ok();
                    }
                } else if let Some(fmtp) = value.strip_prefix("fmtp:") {
                    let (payload_type, fmtp) = fmtp
                        .split_once(' ')
                        .ok_or_else(|| Error::Rtsp("bad fmtp".into()))?;
                    let parsed_pt = payload_type.parse::<u8>().ok();
                    if let Some(rtx) = track.rtx.as_mut()
                        && parsed_pt == Some(rtx.payload_type)
                    {
                        if let Some(apt) = fmtp
                            .split(';')
                            .find_map(|part| part.trim().strip_prefix("apt="))
                            .and_then(|value| value.parse::<u8>().ok())
                        {
                            rtx.apt = apt;
                        }
                    } else if parsed_pt == Some(track.payload_type) {
                        track.fmtp = Some(Arc::from(fmtp));
                    }
                } else if let Some(control) = value.strip_prefix("control:") {
                    // Media control URLs are relative to the session-level
                    // control when one exists (RFC 2326 §C.1).
                    let control = match &session_control {
                        Some(base)
                            if !control.starts_with('/')
                                && !control.contains("://")
                                && control != "*" =>
                        {
                            format!("{}/{}", base.trim_end_matches('/'), control)
                        }
                        _ => control.to_string(),
                    };
                    track.control = Arc::from(control);
                }
            } else if let Some(control) = value.strip_prefix("control:")
                && session_control.is_none()
            {
                session_control = Some(control.to_string());
            }
            #[cfg(feature = "srtp")]
            if let Some(crypto) = value.strip_prefix("crypto:")
                && let Some(track) = current.as_mut()
            {
                let mut parts = crypto.splitn(4, ' ');
                let tag = parts
                    .next()
                    .and_then(|t| t.parse::<u32>().ok())
                    .ok_or_else(|| Error::Rtsp("bad SDP crypto tag".into()))?;
                let crypto_suite = parts
                    .next()
                    .ok_or_else(|| Error::Rtsp("bad SDP crypto suite".into()))?
                    .to_owned();
                let key_params = parts
                    .next()
                    .ok_or_else(|| Error::Rtsp("bad SDP crypto key-params".into()))?
                    .to_owned();
                let session_params = parts.next().map(str::to_owned);
                track.crypto = Some(SdpCrypto {
                    tag,
                    crypto_suite,
                    key_params,
                    session_params,
                });
            }
        }
    }
    if let Some(track) = current {
        tracks.push(track)
    }
    if tracks.is_empty() {
        return Err(Error::Rtsp("SDP has no supported tracks".into()));
    }
    for track in &mut tracks {
        if track.payload_type >= 96 && !mapped_payloads.contains(&track.payload_type) {
            return Err(Error::Rtsp(format!(
                "dynamic payload type {} has no rtpmap",
                track.payload_type
            )));
        }
        if let Some(rtx) = track.rtx.as_mut()
            && rtx.apt == 0
        {
            rtx.apt = track.payload_type;
        }
        if track.control.is_empty() {
            track.control = Arc::from(format!("trackID={}", track.id));
        }
    }
    Ok(MediaDescription {
        session_name: name,
        tracks,
        #[cfg(feature = "multicast")]
        connection,
    })
}

#[cfg(feature = "multicast")]
fn parse_connection(value: &str) -> Result<SdpConnection> {
    let parts: Vec<_> = value.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(Error::Rtsp("SDP c= line too short".into()));
    }
    let addr_type = parts[1].to_owned();
    let addr_part = parts[2];
    let (addr_str, ttl) = if let Some((addr, ttl_str)) = addr_part.split_once('/') {
        let ttl: u8 = ttl_str
            .parse()
            .map_err(|_| Error::Rtsp("invalid SDP connection TTL".into()))?;
        (addr, Some(ttl))
    } else {
        (addr_part, None)
    };
    let address: std::net::IpAddr = addr_str
        .parse()
        .map_err(|_| Error::Rtsp("invalid SDP connection address".into()))?;
    Ok(SdpConnection {
        addr_type,
        address,
        ttl,
        port: None,
    })
}

pub fn control_uri(base: &str, control: &str) -> Result<String> {
    if control == "*" || control.is_empty() {
        return Ok(base.to_owned());
    }
    if let Ok(absolute) = url::Url::parse(control) {
        return Ok(absolute.into());
    }
    let mut base = url::Url::parse(base)
        .map_err(|error| Error::Rtsp(format!("invalid SDP control base: {error}")))?;
    base.set_query(None);
    base.set_fragment(None);
    if control.starts_with('/') {
        base.set_path(control);
        return Ok(base.into());
    }
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    base.join(control)
        .map(Into::into)
        .map_err(|error| Error::Rtsp(format!("invalid SDP control URI: {error}")))
}

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left.max(1)
}

pub fn encode(description: &MediaDescription) -> String {
    encode_to(description, None)
}

/// Encode SDP, choosing the `c=` address family from `local_ip`.
///
/// `None` keeps the historical `c=IN IP4 0.0.0.0` (valid for IPv4 and
/// accepted by most clients); an IPv6 local address emits `c=IN IP6 ::`.
pub fn encode_to(description: &MediaDescription, local_ip: Option<std::net::IpAddr>) -> String {
    encode_with(description, local_ip, false)
}

/// Encode SDP with optional `a=rtcp-mux` advertisement.
pub fn encode_with(
    description: &MediaDescription,
    local_ip: Option<std::net::IpAddr>,
    rtcp_mux: bool,
) -> String {
    encode_with_options(description, local_ip, rtcp_mux, false)
}

/// Encode SDP with optional RTX (RFC 4588) advertisement.
pub fn encode_with_rtx(
    description: &MediaDescription,
    local_ip: Option<std::net::IpAddr>,
    rtcp_mux: bool,
    rtx: bool,
) -> String {
    encode_with_options(description, local_ip, rtcp_mux, rtx)
}

fn encode_with_options(
    description: &MediaDescription,
    local_ip: Option<std::net::IpAddr>,
    rtcp_mux: bool,
    rtx: bool,
) -> String {
    let rtx_configs = if rtx {
        crate::media::rtx_configs(description)
    } else {
        std::collections::HashMap::new()
    };
    let mut output = format!(
        "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns={}\r\nt=0 0\r\n",
        description.session_name
    );
    // RFC 2326 §C.1: a unicast SDP should carry a c= line too (0.0.0.0
    // means "this host"); multicast uses the explicit connection.
    #[cfg(feature = "multicast")]
    let connection = description.connection.as_ref().map(|conn| {
        let ttl_str = conn.ttl.map(|t| format!("/{t}")).unwrap_or_default();
        format!("c=IN {} {}{}\r\n", conn.addr_type, conn.address, ttl_str)
    });
    #[cfg(not(feature = "multicast"))]
    let connection: Option<String> = None;
    if let Some(connection) = connection {
        output.push_str(&connection);
    } else if matches!(local_ip, Some(std::net::IpAddr::V6(_))) {
        output.push_str("c=IN IP6 ::\r\n");
    } else {
        output.push_str("c=IN IP4 0.0.0.0\r\n");
    }
    for track in &description.tracks {
        let media = match track.kind {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        };
        let codec = match track.codec {
            Codec::H264 => "H264",
            Codec::H265 => "H265",
            Codec::Aac => "MPEG4-GENERIC",
            Codec::Pcma => "PCMA",
            Codec::Pcmu => "PCMU",
            Codec::Opus => "opus",
            Codec::Vp8 => "VP8",
            Codec::Vp9 => "VP9",
            Codec::Mp3 => "MPA",
            Codec::G726_16 => "G726-16",
            Codec::G726_24 => "G726-24",
            Codec::G726_32 => "G726-32",
            Codec::G726_40 => "G726-40",
            Codec::Av1 => "AV1",
            Codec::Jpeg => "JPEG",
            Codec::H263 => "H263",
            Codec::Mpeg4 => "MP4V-ES",
        };
        #[cfg(feature = "multicast")]
        let media_port = description
            .connection
            .as_ref()
            .filter(|connection| connection.address.is_multicast())
            .and_then(|connection| connection.port)
            .unwrap_or(0);
        #[cfg(not(feature = "multicast"))]
        let media_port = 0;
        #[cfg(feature = "srtp")]
        let profile = if track.crypto.is_some() {
            "RTP/SAVP"
        } else {
            "RTP/AVP"
        };
        #[cfg(not(feature = "srtp"))]
        let profile = "RTP/AVP";
        if let Some(rtx) = rtx_configs.get(&track.id) {
            output.push_str(&format!(
                "m={media} {media_port} {profile} {} {}\r\n",
                track.payload_type, rtx.payload_type
            ));
        } else {
            output.push_str(&format!(
                "m={media} {media_port} {profile} {}\r\n",
                track.payload_type
            ));
        }
        // PT 14 is the static MPEG audio payload; adding an rtpmap makes
        // FFmpeg treat the stream as dynamic MPA (MP2) instead of MP3.
        if !(track.codec == Codec::Mp3 && track.payload_type == 14) {
            output.push_str(&format!(
                "a=rtpmap:{} {codec}/{}{}\r\n",
                track.payload_type,
                track.clock_rate,
                track
                    .channels
                    .map(|value| format!("/{value}"))
                    .unwrap_or_default()
            ));
        }
        if let Some(bandwidth) = track.bandwidth_as {
            output.push_str(&format!("b=AS:{bandwidth}\r\n"));
        }
        if let (Some(width), Some(height)) = (track.width, track.height)
            && matches!(track.codec, Codec::Jpeg | Codec::H263)
        {
            output.push_str(&format!(
                "a=framesize:{} {width}-{height}\r\n",
                track.payload_type
            ));
        }
        if let Some(direction) = track.direction {
            output.push_str(&format!("a={}\r\n", direction.as_str()));
        }
        if let Some(ptime) = track.ptime {
            output.push_str(&format!("a=ptime:{ptime}\r\n"));
        }
        if let Some(rtcp) = track.rtcp {
            if let Some(address) = rtcp.address {
                let family = if address.is_ipv6() { "IP6" } else { "IP4" };
                output.push_str(&format!("a=rtcp:{} IN {family} {address}\r\n", rtcp.port));
            } else {
                output.push_str(&format!("a=rtcp:{}\r\n", rtcp.port));
            }
        }
        if let Some(fmtp) = &track.fmtp {
            output.push_str(&format!("a=fmtp:{} {}\r\n", track.payload_type, fmtp))
        }
        if let Some(rtx) = rtx_configs.get(&track.id) {
            output.push_str(&format!(
                "a=rtpmap:{} rtx/{}\r\n",
                rtx.payload_type, track.clock_rate
            ));
            output.push_str(&format!(
                "a=fmtp:{} apt={}\r\n",
                rtx.payload_type, track.payload_type
            ));
        }
        if rtcp_mux {
            output.push_str("a=rtcp-mux\r\n");
        }
        if let Some((numerator, denominator)) = track.frame_rate
            && numerator > 0
            && denominator > 0
        {
            let mut rate = format!("{:.3}", f64::from(numerator) / f64::from(denominator));
            while rate.ends_with('0') {
                rate.pop();
            }
            if rate.ends_with('.') {
                rate.pop();
            }
            output.push_str(&format!("a=framerate:{rate}\r\n"));
        }
        #[cfg(feature = "srtp")]
        if let Some(crypto) = &track.crypto {
            output.push_str(&format!(
                "a=crypto:{} {} {}",
                crypto.tag, crypto.crypto_suite, crypto.key_params
            ));
            if let Some(params) = &crypto.session_params {
                output.push_str(&format!(" {params}"));
            }
            output.push_str("\r\n");
        }
        output.push_str(&format!("a=control:{}\r\n", track.control));
    }
    output
}

#[cfg(test)]
mod tests;
