//! SRT HSv5 handshake (caller-listener) per draft-sharabayko-srt.
//!
//! Wire layout: a 48-byte CIF followed by TLV extension blocks. The
//! implementation is byte-compatible with libsrt: handshake type values,
//! the SRT magic carried in the induction response's extension field,
//! HSREQ/HSRSP content, and the byte-reversed 32-bit words used by the
//! Stream ID extension.
use super::packet::{ControlPacket, ControlType};
use bytes::{BufMut, Bytes, BytesMut};

/// SRT library version advertised in HSREQ/HSRSP (1.5.0).
pub const SRT_VERSION: u32 = 0x0001_0500;
/// Version of the caller's induction request (UDT compatibility).
pub const SRT_VERSION_4: u32 = 4;
/// HSv5.
pub const SRT_VERSION_5: u32 = 5;
/// Magic value in the Extension Field of the listener's induction response.
pub const SRT_MAGIC: u16 = 0x4A17;

/// Extension Field bits of a CONCLUSION handshake.
pub const HS_EXT_HSREQ: u16 = 0x1;
pub const HS_EXT_KMREQ: u16 = 0x2;
pub const HS_EXT_CONFIG: u16 = 0x4;

/// Extension command codes.
pub const SRT_CMD_HSREQ: u16 = 1;
pub const SRT_CMD_HSRSP: u16 = 2;
pub const SRT_CMD_KMREQ: u16 = 3;
pub const SRT_CMD_KMRSP: u16 = 4;
pub const SRT_CMD_SID: u16 = 5;
pub const SRT_CMD_CONGESTION: u16 = 6;
pub const SRT_CMD_FILTER: u16 = 7;
pub const SRT_CMD_GROUP: u16 = 8;

/// Maximum Stream ID length on the wire.
pub const MAX_STREAM_ID: usize = 512;

/// Handshake Type field values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandshakeType {
    /// Rendezvous only; rejected by the listener.
    Wavehand,
    Induction,
    Conclusion,
    Agreement,
    /// A rejected connection: `1000 + SRT_REJ_*`.
    Rejection(u32),
}

impl HandshakeType {
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Wavehand),
            1 => Some(Self::Induction),
            0xFFFF_FFFF => Some(Self::Conclusion),
            0xFFFF_FFFE => Some(Self::Agreement),
            1000..=0x7FFF_FFFF => Some(Self::Rejection(value)),
            _ => None,
        }
    }

    pub fn to_u32(self) -> u32 {
        match self {
            Self::Wavehand => 0,
            Self::Induction => 1,
            Self::Conclusion => 0xFFFF_FFFF,
            Self::Agreement => 0xFFFF_FFFE,
            Self::Rejection(code) => code,
        }
    }
}

/// `SRT_REJ_*` rejection codes. The Handshake Type field carries the code
/// itself (1000..1017); a peer reports `code - 1000` as the reason.
pub mod reject {
    pub const UNKNOWN: u32 = 1000;
    pub const SYSTEM: u32 = 1001;
    pub const PEER: u32 = 1002;
    pub const RESOURCE: u32 = 1003;
    pub const ROGUE: u32 = 1004;
    pub const BACKLOG: u32 = 1005;
    pub const IPE: u32 = 1006;
    pub const CLOSE: u32 = 1007;
    pub const VERSION: u32 = 1008;
    pub const RDVCOOKIE: u32 = 1009;
    pub const BADSECRET: u32 = 1010;
    pub const UNSECURE: u32 = 1011;
    pub const MESSAGEAPI: u32 = 1012;
    pub const CONGESTION: u32 = 1013;
    pub const FILTER: u32 = 1014;
    pub const GROUP: u32 = 1015;
    pub const TIMEOUT: u32 = 1016;
    pub const CRYPTO: u32 = 1017;
}

/// SRT configuration flags carried by HSREQ/HSRSP.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HsFlags(pub u32);

impl HsFlags {
    pub const TSBPDSND: u32 = 0x01;
    pub const TSBPDRCV: u32 = 0x02;
    pub const CRYPT: u32 = 0x04;
    pub const TLPKTDROP: u32 = 0x08;
    pub const PERIODICNAK: u32 = 0x10;
    pub const REXMITFLG: u32 = 0x20;
    pub const STREAM: u32 = 0x40;
    pub const PACKET_FILTER: u32 = 0x80;

    /// Live-mode defaults: TSBPD both directions, TLPKTDROP, periodic NAK,
    /// retransmit flag and the legacy CRYPT capability bit.
    pub fn live_defaults() -> Self {
        Self(
            Self::TSBPDSND
                | Self::TSBPDRCV
                | Self::CRYPT
                | Self::TLPKTDROP
                | Self::PERIODICNAK
                | Self::REXMITFLG,
        )
    }

    pub fn contains(self, bits: u32) -> bool {
        self.0 & bits == bits
    }
}

/// Payload of HSREQ/HSRSP (3 words on the wire).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HsExtFields {
    pub srt_version: u32,
    pub flags: HsFlags,
    /// Receiver TSBPD delay, milliseconds.
    pub recv_latency_ms: u16,
    /// Sender TSBPD delay, milliseconds.
    pub send_latency_ms: u16,
}

impl HsExtFields {
    fn parse(content: &[u8]) -> Option<Self> {
        if content.len() < 12 {
            return None;
        }
        let srt_version = u32::from_be_bytes(content[0..4].try_into().ok()?);
        let flags = HsFlags(u32::from_be_bytes(content[4..8].try_into().ok()?));
        let latency = u32::from_be_bytes(content[8..12].try_into().ok()?);
        Some(Self {
            srt_version,
            flags,
            recv_latency_ms: (latency >> 16) as u16,
            send_latency_ms: latency as u16,
        })
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.srt_version.to_be_bytes());
        out.extend_from_slice(&self.flags.0.to_be_bytes());
        let latency = (u32::from(self.recv_latency_ms) << 16) | u32::from(self.send_latency_ms);
        out.extend_from_slice(&latency.to_be_bytes());
    }
}

/// One handshake extension block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HsExtension {
    HsReq(HsExtFields),
    HsRsp(HsExtFields),
    /// Stream ID: arbitrary bytes on the wire, decoded lossily.
    StreamId(String),
    KmReq(Vec<u8>),
    KmRsp(Vec<u8>),
    Congestion(String),
    /// A known command with malformed content; the caller answers ROGUE.
    Invalid {
        cmd: u16,
        data: Vec<u8>,
    },
    Unknown {
        cmd: u16,
        data: Vec<u8>,
    },
}

impl HsExtension {
    fn command(&self) -> u16 {
        match self {
            Self::HsReq(_) => SRT_CMD_HSREQ,
            Self::HsRsp(_) => SRT_CMD_HSRSP,
            Self::StreamId(_) => SRT_CMD_SID,
            Self::KmReq(_) => SRT_CMD_KMREQ,
            Self::KmRsp(_) => SRT_CMD_KMRSP,
            Self::Congestion(_) => SRT_CMD_CONGESTION,
            Self::Invalid { cmd, .. } | Self::Unknown { cmd, .. } => *cmd,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let mut content = Vec::new();
        match self {
            Self::HsReq(fields) | Self::HsRsp(fields) => fields.encode(&mut content),
            Self::StreamId(value) | Self::Congestion(value) => {
                pack_string_words(value, &mut content)
            }
            Self::KmReq(data) | Self::KmRsp(data) => content.extend_from_slice(data),
            Self::Invalid { data, .. } | Self::Unknown { data, .. } => {
                content.extend_from_slice(data)
            }
        }
        while content.len() % 4 != 0 {
            content.push(0);
        }
        out.extend_from_slice(&self.command().to_be_bytes());
        out.extend_from_slice(&((content.len() / 4) as u16).to_be_bytes());
        out.extend_from_slice(&content);
    }

    fn extension_bits(&self) -> u16 {
        match self {
            Self::HsReq(_) | Self::HsRsp(_) => HS_EXT_HSREQ,
            Self::KmReq(_) | Self::KmRsp(_) => HS_EXT_KMREQ,
            _ => HS_EXT_CONFIG,
        }
    }
}

/// Parsed handshake CIF plus extensions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Handshake {
    pub version: u32,
    pub encryption: u16,
    /// Extension Field (flags, magic, or UDT_DGRAM=2).
    pub extension_field: u16,
    /// Initial packet sequence number (31 bits).
    pub initial_sequence: u32,
    pub mtu: u32,
    pub flow_window: u32,
    pub handshake_type: HandshakeType,
    /// The sender's SRT socket ID.
    pub socket_id: u32,
    pub cookie: u32,
    /// Peer IP address (16 bytes, per-word byte order).
    pub peer_ip: [u8; 16],
    pub extensions: Vec<HsExtension>,
}

impl Handshake {
    pub fn hs_ext(&self) -> Option<&HsExtFields> {
        self.extensions.iter().find_map(|ext| match ext {
            HsExtension::HsReq(fields) | HsExtension::HsRsp(fields) => Some(fields),
            _ => None,
        })
    }

    pub fn stream_id(&self) -> Option<&str> {
        self.extensions.iter().find_map(|ext| match ext {
            HsExtension::StreamId(value) => Some(value.as_str()),
            _ => None,
        })
    }

    pub fn requests_encryption(&self) -> bool {
        self.encryption != 0
            || self
                .extensions
                .iter()
                .any(|ext| matches!(ext, HsExtension::KmReq(_) | HsExtension::KmRsp(_)))
    }

    /// Extension Field implied by the attached blocks.
    pub fn extension_bits(&self) -> u16 {
        self.extensions
            .iter()
            .fold(0, |bits, ext| bits | ext.extension_bits())
    }

    /// IPv4 peer address in the spec's per-word byte order.
    pub fn encode_ipv4(address: std::net::Ipv4Addr) -> [u8; 16] {
        let mut out = [0u8; 16];
        let mut octets = address.octets();
        octets.reverse();
        out[..4].copy_from_slice(&octets);
        out
    }
}

/// Parse a handshake CIF (everything after the 16-byte control header).
pub fn parse_handshake(data: &[u8]) -> Option<Handshake> {
    if data.len() < 48 {
        return None;
    }
    let version = u32::from_be_bytes(data[0..4].try_into().ok()?);
    let encryption = u16::from_be_bytes(data[4..6].try_into().ok()?);
    let extension_field = u16::from_be_bytes(data[6..8].try_into().ok()?);
    let initial_sequence = u32::from_be_bytes(data[8..12].try_into().ok()?);
    // The top bit of the ISN field is reserved and must be clear; the maximum
    // sequence number itself is not a valid initial value.
    if initial_sequence >= 0x7FFF_FFFF {
        return None;
    }
    let mtu = u32::from_be_bytes(data[12..16].try_into().ok()?);
    let flow_window = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let handshake_type =
        HandshakeType::from_u32(u32::from_be_bytes(data[20..24].try_into().ok()?))?;
    let socket_id = u32::from_be_bytes(data[24..28].try_into().ok()?);
    let cookie = u32::from_be_bytes(data[28..32].try_into().ok()?);
    let mut peer_ip = [0u8; 16];
    peer_ip.copy_from_slice(&data[32..48]);

    let extensions = parse_extensions(&data[48..])?;
    Some(Handshake {
        version,
        encryption,
        extension_field,
        initial_sequence,
        mtu,
        flow_window,
        handshake_type,
        socket_id,
        cookie,
        peer_ip,
        extensions,
    })
}

fn parse_extensions(mut rest: &[u8]) -> Option<Vec<HsExtension>> {
    let mut extensions = Vec::new();
    while rest.len() >= 4 {
        let cmd = u16::from_be_bytes(rest[0..2].try_into().ok()?);
        let words = u16::from_be_bytes(rest[2..4].try_into().ok()?);
        let length = words as usize * 4;
        rest = &rest[4..];
        if rest.len() < length {
            return None;
        }
        let content = &rest[..length];
        rest = &rest[length..];
        extensions.push(parse_extension(cmd, content));
    }
    Some(extensions)
}

fn parse_extension(cmd: u16, content: &[u8]) -> HsExtension {
    match cmd {
        SRT_CMD_HSREQ => match HsExtFields::parse(content) {
            Some(fields) => HsExtension::HsReq(fields),
            None => HsExtension::Invalid {
                cmd,
                data: content.to_vec(),
            },
        },
        SRT_CMD_HSRSP => match HsExtFields::parse(content) {
            Some(fields) => HsExtension::HsRsp(fields),
            None => HsExtension::Invalid {
                cmd,
                data: content.to_vec(),
            },
        },
        SRT_CMD_SID => {
            if content.is_empty() || content.len() > MAX_STREAM_ID {
                return HsExtension::Invalid {
                    cmd,
                    data: content.to_vec(),
                };
            }
            HsExtension::StreamId(
                String::from_utf8_lossy(&unpack_string_words(content)).into_owned(),
            )
        }
        SRT_CMD_CONGESTION => HsExtension::Congestion(
            String::from_utf8_lossy(&unpack_string_words(content)).into_owned(),
        ),
        SRT_CMD_KMREQ => HsExtension::KmReq(content.to_vec()),
        SRT_CMD_KMRSP => HsExtension::KmRsp(content.to_vec()),
        _ => HsExtension::Unknown {
            cmd,
            data: content.to_vec(),
        },
    }
}

/// Encode a handshake CIF plus optional extensions.
pub fn encode_handshake(handshake: &Handshake) -> Bytes {
    let mut buf = BytesMut::with_capacity(64);
    buf.put_u32(handshake.version);
    buf.put_u16(handshake.encryption);
    let extension_field = if handshake.extensions.is_empty() {
        handshake.extension_field
    } else {
        handshake.extension_bits()
    };
    buf.put_u16(extension_field);
    buf.put_u32(handshake.initial_sequence & 0x7FFF_FFFF);
    buf.put_u32(handshake.mtu);
    buf.put_u32(handshake.flow_window);
    buf.put_u32(handshake.handshake_type.to_u32());
    buf.put_u32(handshake.socket_id);
    buf.put_u32(handshake.cookie);
    buf.put_slice(&handshake.peer_ip);
    let mut extensions = Vec::new();
    for extension in &handshake.extensions {
        extension.encode(&mut extensions);
    }
    buf.put_slice(&extensions);
    buf.freeze()
}

/// Pack a string into 32-bit words, NUL-padded, each word byte-reversed
/// ("abcdefg" -> "dcba\0gfe"), as required by the Stream ID extension.
fn pack_string_words(value: &str, out: &mut Vec<u8>) {
    for chunk in value.as_bytes().chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        word.reverse();
        out.extend_from_slice(&word);
    }
}

fn unpack_string_words(content: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(content.len());
    for chunk in content.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        word.reverse();
        bytes.extend_from_slice(&word);
    }
    while bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

/// Wrap a handshake in a control packet destined for `socket_id`.
pub fn handshake_control_packet(
    handshake: &Handshake,
    socket_id: u32,
    timestamp: u32,
) -> ControlPacket {
    ControlPacket {
        control_type: ControlType::Handshake,
        type_specific: 0,
        additional_info: 0,
        timestamp,
        socket_id,
        data: encode_handshake(handshake),
    }
}

/// Listener's INDUCTION response: HSv5, SRT magic in the extension field,
/// no extension blocks, caller's socket id echoed, cookie set.
pub fn induction_response(caller_socket_id: u32, cookie: u32) -> Handshake {
    Handshake {
        version: SRT_VERSION_5,
        encryption: 0,
        extension_field: SRT_MAGIC,
        initial_sequence: 0,
        mtu: 1500,
        flow_window: 8192,
        handshake_type: HandshakeType::Induction,
        socket_id: caller_socket_id,
        cookie,
        peer_ip: [0u8; 16],
        extensions: Vec::new(),
    }
}

/// Listener's CONCLUSION response (accept).
pub fn conclusion_response(
    listener_socket_id: u32,
    caller: &Handshake,
    fields: HsExtFields,
) -> Handshake {
    Handshake {
        version: SRT_VERSION_5,
        encryption: 0,
        extension_field: HS_EXT_HSREQ,
        initial_sequence: caller.initial_sequence,
        mtu: caller.mtu.min(1500),
        flow_window: caller.flow_window.clamp(1, 8192),
        handshake_type: HandshakeType::Conclusion,
        socket_id: listener_socket_id,
        cookie: caller.cookie,
        peer_ip: [0u8; 16],
        extensions: vec![HsExtension::HsRsp(fields)],
    }
}

/// Listener's CONCLUSION response carrying a rejection reason.
pub fn rejection_response(listener_socket_id: u32, caller: &Handshake, reason: u32) -> Handshake {
    Handshake {
        version: SRT_VERSION_5,
        encryption: 0,
        extension_field: 0,
        initial_sequence: caller.initial_sequence,
        mtu: 1500,
        flow_window: 8192,
        handshake_type: HandshakeType::Rejection(reason.max(1000)),
        socket_id: listener_socket_id,
        cookie: caller.cookie,
        peer_ip: [0u8; 16],
        extensions: Vec::new(),
    }
}

/// Craft a SYN cookie from the peer address and current time (1 s accuracy).
pub fn generate_cookie(peer_addr: std::net::SocketAddr, timestamp: u32) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    peer_addr.hash(&mut hasher);
    // Fold to roughly one-minute granularity as the spec suggests; keeps the
    // cookie stable enough for retries yet unpredictable.
    (timestamp / 60).hash(&mut hasher);
    (hasher.finish() & 0xFFFF_FFFF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller_induction() -> Handshake {
        Handshake {
            version: SRT_VERSION_4,
            encryption: 0,
            extension_field: 2,
            initial_sequence: 0x1234_5678,
            mtu: 1500,
            flow_window: 8192,
            handshake_type: HandshakeType::Induction,
            socket_id: 0x0102_0304,
            cookie: 0,
            peer_ip: Handshake::encode_ipv4(std::net::Ipv4Addr::new(192, 168, 1, 10)),
            extensions: Vec::new(),
        }
    }

    #[test]
    fn handshake_type_values_match_spec() {
        assert_eq!(HandshakeType::Wavehand.to_u32(), 0);
        assert_eq!(HandshakeType::Induction.to_u32(), 1);
        assert_eq!(HandshakeType::Conclusion.to_u32(), 0xFFFF_FFFF);
        assert_eq!(HandshakeType::Agreement.to_u32(), 0xFFFF_FFFE);
        assert_eq!(
            HandshakeType::from_u32(0xFFFF_FFFF),
            Some(HandshakeType::Conclusion)
        );
        assert_eq!(
            HandshakeType::from_u32(1004),
            Some(HandshakeType::Rejection(1004))
        );
        // DONE is not valid in caller-listener mode.
        assert_eq!(HandshakeType::from_u32(0xFFFF_FFFD), None);
    }

    #[test]
    fn induction_request_byte_exact() {
        let bytes = encode_handshake(&caller_induction());
        assert_eq!(bytes.len(), 48);
        assert_eq!(&bytes[0..4], &[0, 0, 0, 4]);
        assert_eq!(&bytes[4..8], &[0, 0, 0, 2]);
        assert_eq!(&bytes[8..12], &[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(&bytes[20..24], &[0, 0, 0, 1]);
        assert_eq!(&bytes[24..28], &[1, 2, 3, 4]);
        assert_eq!(&bytes[32..36], &[10, 1, 168, 192]);
    }

    #[test]
    fn induction_response_has_magic_and_cookie() {
        let response = induction_response(0x0102_0304, 0x6BEE_F00D);
        let bytes = encode_handshake(&response);
        assert_eq!(&bytes[0..4], &[0, 0, 0, 5]);
        assert_eq!(&bytes[6..8], &0x4A17u16.to_be_bytes());
        assert_eq!(&bytes[20..24], &1u32.to_be_bytes());
        assert_eq!(&bytes[24..28], &0x0102_0304u32.to_be_bytes());
        assert_eq!(&bytes[28..32], &0x6BEE_F00Du32.to_be_bytes());
        assert_eq!(bytes.len(), 48);

        let parsed = parse_handshake(&bytes).unwrap();
        assert_eq!(parsed.handshake_type, HandshakeType::Induction);
        assert_eq!(parsed.cookie, 0x6BEE_F00D);
    }

    #[test]
    fn conclusion_request_and_response_roundtrip() {
        let request = Handshake {
            version: SRT_VERSION_5,
            encryption: 0,
            extension_field: HS_EXT_HSREQ | HS_EXT_CONFIG,
            initial_sequence: 0x1234_5678,
            mtu: 1500,
            flow_window: 8192,
            handshake_type: HandshakeType::Conclusion,
            socket_id: 0x0102_0304,
            cookie: 0x6BEE_F00D,
            peer_ip: [0u8; 16],
            extensions: vec![
                HsExtension::HsReq(HsExtFields {
                    srt_version: 0x0001_0404,
                    flags: HsFlags::live_defaults(),
                    recv_latency_ms: 120,
                    send_latency_ms: 0,
                }),
                HsExtension::StreamId("abcdefg".into()),
            ],
        };
        let bytes = encode_handshake(&request);
        // Fixed CIF + HSREQ (4+12) + SID (4+8)
        assert_eq!(bytes.len(), 76);
        assert_eq!(&bytes[48..52], &[0, 1, 0, 3]); // HSREQ, 3 words
        assert_eq!(&bytes[52..56], &0x0001_0404u32.to_be_bytes());
        assert_eq!(&bytes[56..60], &0x3Fu32.to_be_bytes());
        assert_eq!(&bytes[60..64], &[0x00, 0x78, 0x00, 0x00]);
        assert_eq!(&bytes[64..68], &[0, 5, 0, 2]); // SID, 2 words
        // "abcdefg" -> "dcba\0gfe"
        assert_eq!(&bytes[68..76], b"dcba\0gfe");

        let parsed = parse_handshake(&bytes).unwrap();
        assert_eq!(parsed.stream_id(), Some("abcdefg"));
        let fields = parsed.hs_ext().unwrap();
        assert_eq!(fields.recv_latency_ms, 120);
        assert_eq!(fields.send_latency_ms, 0);

        let response = conclusion_response(0x00A1_B2C3, &parsed, *fields);
        let parsed_response = parse_handshake(&encode_handshake(&response)).unwrap();
        assert_eq!(parsed_response.handshake_type, HandshakeType::Conclusion);
        assert_eq!(parsed_response.socket_id, 0x00A1_B2C3);
        assert_eq!(parsed_response.initial_sequence, parsed.initial_sequence);
    }

    #[test]
    fn rejection_uses_reason_code() {
        let caller = caller_induction();
        let response = rejection_response(7, &caller, reject::CRYPTO);
        let parsed = parse_handshake(&encode_handshake(&response)).unwrap();
        assert_eq!(parsed.handshake_type, HandshakeType::Rejection(1017));
    }

    #[test]
    fn malformed_extension_is_invalid() {
        let mut bytes = encode_handshake(&caller_induction()).to_vec();
        // HSREQ with 2 words instead of 3, appended to a conclusion CIF.
        let mut cif = vec![0u8; 48];
        cif[0..4].copy_from_slice(&SRT_VERSION_5.to_be_bytes());
        cif[20..24].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        // cmd=HSREQ, length=2 words, but only 2 words of content follow
        // (the spec requires 3), i.e. 8 bytes.
        cif.extend_from_slice(&[0, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]);
        let _ = &mut bytes;
        let parsed = parse_handshake(&cif).unwrap();
        assert!(matches!(
            parsed.extensions.first(),
            Some(HsExtension::Invalid { cmd: 1, .. })
        ));
    }
}
