use crate::{
    Error, Result,
    media::{Codec, MediaDescription, MediaKind, TrackDescription},
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
                    _ => return Err(Error::Rtsp("unsupported SDP codec".into())),
                };
                track.clock_rate = parts
                    .get(1)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(track.clock_rate);
                track.channels = parts.get(2).and_then(|value| value.parse().ok());
                mapped_payloads.insert(track.payload_type);
            }
        } else if let Some(value) = line.strip_prefix("a=framerate:") {
            if let Some(track) = current.as_mut()
                && let Ok(fps) = value.trim().parse::<f64>()
                && fps.is_finite()
                && fps > 0.0
                && fps <= 1_000.0
            {
                let numerator = (fps * 1_000.0).round() as u32;
                let divisor = gcd(numerator, 1_000);
                track.frame_rate = Some((numerator / divisor, 1_000 / divisor));
            }
        } else if let Some(value) = line.strip_prefix("a=fmtp:") {
            if let Some(track) = current.as_mut() {
                let (payload_type, fmtp) = value
                    .split_once(' ')
                    .ok_or_else(|| Error::Rtsp("bad fmtp".into()))?;
                if payload_type.parse::<u8>().ok() == Some(track.payload_type) {
                    track.fmtp = Some(Arc::from(fmtp));
                }
            }
        } else if let Some(value) = line.strip_prefix("a=control:") {
            if let Some(track) = current.as_mut() {
                // Media control URLs are relative to the session-level
                // control when one exists (RFC 2326 §C.1).
                let control = match &session_control {
                    Some(base)
                        if !value.starts_with('/') && !value.contains("://") && value != "*" =>
                    {
                        format!("{}/{}", base.trim_end_matches('/'), value)
                    }
                    _ => value.to_string(),
                };
                track.control = Arc::from(control);
            } else if session_control.is_none() {
                session_control = Some(value.to_string());
            }
        } else if let Some(_value) = line.strip_prefix("a=crypto:")
            && let Some(_track) = current.as_mut()
        {
            #[cfg(feature = "srtp")]
            {
                let mut parts = _value.splitn(4, ' ');
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
                _track.crypto = Some(SdpCrypto {
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
        output.push_str(&format!(
            "m={media} {media_port} RTP/AVP {}\r\na=rtpmap:{} {codec}/{}{}\r\n",
            track.payload_type,
            track.payload_type,
            track.clock_rate,
            track
                .channels
                .map(|value| format!("/{value}"))
                .unwrap_or_default()
        ));
        if let Some(fmtp) = &track.fmtp {
            output.push_str(&format!("a=fmtp:{} {}\r\n", track.payload_type, fmtp))
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
mod tests {
    use super::*;
    #[test]
    fn static_g711_and_control_resolution() {
        let parsed = parse(b"v=0\r\ns=g711\r\nm=audio 0 RTP/AVP 8\r\na=control:audio\r\n").unwrap();
        assert_eq!(parsed.tracks[0].codec, Codec::Pcma);
        assert_eq!(parsed.tracks[0].clock_rate, 8_000);
        assert_eq!(
            control_uri("rtsp://camera/live", "audio").unwrap(),
            "rtsp://camera/live/audio"
        );
        assert_eq!(
            control_uri("rtsp://camera/live/", "rtsp://other/track").unwrap(),
            "rtsp://other/track"
        );
    }

    #[test]
    fn vp8_vp9_rtpmap_parse() {
        let sdp = b"v=0\r\ns=vp\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 VP8/90000\r\na=control:trackID=0\r\n";
        assert_eq!(parse(sdp).unwrap().tracks[0].codec, Codec::Vp8);
        let sdp = b"v=0\r\ns=vp\r\nm=video 0 RTP/AVP 97\r\na=rtpmap:97 VP9/90000\r\na=control:trackID=0\r\n";
        assert_eq!(parse(sdp).unwrap().tracks[0].codec, Codec::Vp9);
    }

    #[test]
    fn track_limit_is_enforced() {
        let mut sdp = String::from("v=0\r\ns=many\r\n");
        for i in 0..3 {
            sdp.push_str(&format!(
                "m=video 0 RTP/AVP {}\r\na=rtpmap:{} H264/90000\r\na=control:trackID={}\r\n",
                96 + i,
                96 + i,
                i
            ));
        }
        assert!(parse_with_limits(sdp.as_bytes(), SdpLimits { max_tracks: 2 }).is_err());
        assert_eq!(
            parse_with_limits(sdp.as_bytes(), SdpLimits { max_tracks: 3 })
                .unwrap()
                .tracks
                .len(),
            3
        );
    }

    #[test]
    fn sdp_attributes_and_ipv6_encoding() {
        let input = b"v=0\r\ns=attrs\r\nm=video 0 RTP/AVP 96 97\r\na=rtpmap:97 H265/90000\r\na=framerate:25\r\na=control:trackID=0\r\n";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.tracks[0].codec, Codec::H265);
        assert_eq!(parsed.tracks[0].payload_type, 97);
        assert_eq!(parsed.tracks[0].frame_rate, Some((25, 1)));
        let encoded = encode_to(&parsed, Some("::1".parse().unwrap()));
        assert!(encoded.contains("c=IN IP6 ::"));
        assert!(encoded.contains("a=framerate:25"));
    }

    #[test]
    fn savp_profile_and_ptime_are_tolerated() {
        let input = b"v=0\r\ns=secure\r\nm=video 0 RTP/SAVP 96\r\na=rtpmap:96 H264/90000\r\na=ptime:20\r\na=control:trackID=0\r\n";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.tracks[0].codec, Codec::H264);
    }

    #[test]
    fn mp3_and_g726_names_round_trip() {
        for (codec, name) in [
            (Codec::Mp3, "MPA"),
            (Codec::G726_16, "G726-16"),
            (Codec::G726_24, "G726-24"),
            (Codec::G726_32, "G726-32"),
            (Codec::G726_40, "G726-40"),
        ] {
            let track = TrackDescription {
                id: 0,
                kind: MediaKind::Audio,
                codec,
                clock_rate: if codec == Codec::Mp3 { 90_000 } else { 8_000 },
                channels: Some(1),
                payload_type: 96,
                fmtp: None,
                control: Arc::from("trackID=0"),
                width: None,
                height: None,
                frame_rate: None,
                #[cfg(feature = "srtp")]
                crypto: None,
            };
            let description = MediaDescription {
                session_name: Arc::from("audio"),
                tracks: vec![track],
                #[cfg(feature = "multicast")]
                connection: None,
            };
            let encoded = encode(&description);
            assert!(encoded.contains(name), "{encoded}");
            let parsed = parse(encoded.as_bytes()).unwrap();
            assert_eq!(parsed.tracks[0].codec, codec);
        }
    }

    #[test]
    fn two_track_round_trip() {
        let input=b"v=0\r\ns=av\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=0\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000/1\r\na=control:trackID=1\r\n";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.tracks.len(), 2);
        assert_eq!(parse(encode(&parsed).as_bytes()).unwrap().tracks.len(), 2);
    }
}
