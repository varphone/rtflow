use crate::{
    Error, Result,
    media::{Codec, MediaDescription, MediaKind, TrackDescription},
};
use std::{collections::HashSet, sync::Arc};

/// SDP connection information (RFC 4566 §5.7).
#[cfg(feature = "multicast")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SdpConnection {
    pub addr_type: String,
    pub address: std::net::IpAddr,
    pub ttl: Option<u8>,
}

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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SdpCrypto {
    pub tag: u32,
    pub crypto_suite: String,
    pub key_params: String,
    pub session_params: Option<String>,
}

#[cfg(feature = "srtp")]
impl SdpCrypto {
    pub fn decode_key_material(&self) -> Result<Vec<u8>> {
        let inline = self
            .key_params
            .split(';')
            .find_map(|p| p.strip_prefix("inline:"))
            .ok_or_else(|| Error::Rtsp("SRTP crypto key-params missing inline".into()))?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(inline)
            .map_err(|_| Error::Rtsp("SRTP crypto inline is not valid base64".into()))
    }
}

pub fn parse(data: &[u8]) -> Result<MediaDescription> {
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
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("s=") {
            name = Arc::from(value)
        } else if let Some(_value) = line.strip_prefix("c=") {
            #[cfg(feature = "multicast")]
            {
                connection = Some(parse_connection(_value)?);
            }
        } else if let Some(value) = line.strip_prefix("m=") {
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
            let payload_type: u8 = fields[3]
                .parse()
                .map_err(|_| Error::Rtsp("invalid SDP payload type".into()))?;
            if payload_type > 127 {
                return Err(Error::Rtsp("SDP payload type exceeds 127".into()));
            }
            let (codec, clock_rate, channels) = match (kind, payload_type) {
                (MediaKind::Audio, 0) => (Codec::Pcmu, 8_000, Some(1)),
                (MediaKind::Audio, 8) => (Codec::Pcma, 8_000, Some(1)),
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
            if let Some(track) = current.as_mut()
                && track.payload_type.to_string() == pt
            {
                track.codec = match parts[0].to_ascii_uppercase().as_str() {
                    "H264" => Codec::H264,
                    "H265" | "HEVC" => Codec::H265,
                    "MPEG4-GENERIC" => Codec::Aac,
                    "PCMA" => Codec::Pcma,
                    "PCMU" => Codec::Pcmu,
                    _ => return Err(Error::Rtsp("unsupported SDP codec".into())),
                };
                track.clock_rate = parts
                    .get(1)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(track.clock_rate);
                track.channels = parts.get(2).and_then(|value| value.parse().ok());
                mapped_payloads.insert(track.payload_type);
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

pub fn encode(description: &MediaDescription) -> String {
    let mut output = format!(
        "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns={}\r\nt=0 0\r\n",
        description.session_name
    );
    // RFC 2326 §C.1: a unicast SDP should carry a c= line too (0.0.0.0
    // means "this host"); multicast uses the explicit connection.
    #[cfg(feature = "multicast")]
    if let Some(conn) = &description.connection {
        let ttl_str = conn.ttl.map(|t| format!("/{t}")).unwrap_or_default();
        output.push_str(&format!(
            "c=IN {} {}{}\r\n",
            conn.addr_type, conn.address, ttl_str
        ));
    } else {
        output.push_str("c=IN IP4 0.0.0.0\r\n");
    }
    #[cfg(not(feature = "multicast"))]
    output.push_str("c=IN IP4 0.0.0.0\r\n");
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
        };
        output.push_str(&format!(
            "m={media} 0 RTP/AVP {}\r\na=rtpmap:{} {codec}/{}{}\r\n",
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
    fn two_track_round_trip() {
        let input=b"v=0\r\ns=av\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=0\r\nm=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000/1\r\na=control:trackID=1\r\n";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.tracks.len(), 2);
        assert_eq!(parse(encode(&parsed).as_bytes()).unwrap().tracks.len(), 2);
    }
}
