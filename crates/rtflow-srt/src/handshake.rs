//! SRT HSv5 handshake implementation.
//!
//! The SRT handshake consists of two phases:
//! 1. INDUCTION: Caller sends version=4, listener responds with cookie
//! 2. CONCLUSION: Caller sends version=5 with cookie, listener confirms
//!
//! This implementation supports listener mode only, without encryption.
use super::packet::{ControlPacket, ControlType};
use bytes::{Buf, BufMut, Bytes, BytesMut};

/// SRT handshake packet types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum HandshakeType {
    Done = 0xFFFF,
    Agreement = 0xFFFE,
    Conclusion = 0xFFFD,
    Wavehand = 0xFFFC,
    Induction = 0x0001,
}

impl HandshakeType {
    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            0xFFFF => Some(Self::Done),
            0xFFFE => Some(Self::Agreement),
            0xFFFD => Some(Self::Conclusion),
            0xFFFC => Some(Self::Wavehand),
            0x0001 => Some(Self::Induction),
            _ => None,
        }
    }
}

/// SRT version constants.
pub const SRT_VERSION_4: u32 = 4;
pub const SRT_VERSION_5: u32 = 0x0001_0500; // 1.5.0

/// Parsed SRT handshake.
#[derive(Clone, Debug)]
pub struct Handshake {
    /// UDT version (4 for induction, 5 for conclusion).
    pub version: u32,
    /// Encryption field (0 = no encryption).
    pub encryption: u16,
    /// Extension field.
    pub extension: u16,
    /// Initial packet sequence number.
    pub initial_sequence: u32,
    /// Maximum transmission unit.
    pub mtu: u32,
    /// Maximum flow window size.
    pub flow_window: u32,
    /// Handshake type.
    pub handshake_type: HandshakeType,
    /// Socket ID of the sender.
    pub socket_id: u32,
    /// SYN cookie.
    pub cookie: u32,
    /// Peer IP address (network byte order).
    pub peer_ip: [u8; 16],
    /// SRT socket ID (for HSv5).
    pub srt_socket_id: u32,
    /// SRT version (for HSv5).
    pub srt_version: u32,
    /// SRT flags (for HSv5).
    pub srt_flags: u32,
}

impl Default for Handshake {
    fn default() -> Self {
        Self {
            version: SRT_VERSION_5,
            encryption: 0,
            extension: 0x0002, // HS_EXT_HSREQ
            initial_sequence: 0,
            mtu: 1500,
            flow_window: 8192,
            handshake_type: HandshakeType::Induction,
            socket_id: 0,
            cookie: 0,
            peer_ip: [0u8; 16],
            srt_socket_id: 0,
            srt_version: SRT_VERSION_5,
            srt_flags: 0x0000_00BF, // TSBPDSND | TSBPDRCV | CRYPT | TLPKTDROP | PERIODICNAK
        }
    }
}

/// Parse a handshake from control packet data.
pub fn parse_handshake(data: &[u8]) -> Option<Handshake> {
    if data.len() < 48 {
        return None;
    }

    let mut buf = data;
    let version = buf.get_u32();
    let encryption = buf.get_u16();
    let extension = buf.get_u16();
    let initial_sequence = buf.get_u32();
    let mtu = buf.get_u32();
    let flow_window = buf.get_u32();
    let hs_type_raw = buf.get_u32();
    let socket_id = buf.get_u32();
    let cookie = buf.get_u32();

    let mut peer_ip = [0u8; 16];
    if buf.remaining() >= 16 {
        buf.copy_to_slice(&mut peer_ip);
    }

    let handshake_type = HandshakeType::from_u16((hs_type_raw & 0xFFFF) as u16)?;

    // Parse SRT extensions if present (HSv5)
    let srt_socket_id = socket_id;
    let mut srt_version = version;
    let mut srt_flags = 0;

    if version == SRT_VERSION_5 && buf.remaining() >= 16 {
        // Skip extension headers and parse SRT magic
        // Format: ext_type(16) | ext_size(16) | ext_data...
        while buf.remaining() >= 4 {
            let ext_type = buf.get_u16();
            let ext_size = buf.get_u16() as usize * 4; // size in 32-bit words

            if ext_type == 0x0001 && buf.remaining() >= 16 {
                // SRT_CMD_HSREQ
                let magic = buf.get_u32();
                if magic == 0x4A17 {
                    srt_version = buf.get_u32();
                    srt_flags = buf.get_u32();
                    let _ = buf.get_u32(); // receiver TSBPD delay | sender TSBPD delay
                }
                break;
            } else if buf.remaining() >= ext_size {
                // Skip unknown extension
                let mut skip = vec![0u8; ext_size];
                buf.copy_to_slice(&mut skip);
            } else {
                break;
            }
        }
    }

    Some(Handshake {
        version,
        encryption,
        extension,
        initial_sequence,
        mtu,
        flow_window,
        handshake_type,
        socket_id,
        cookie,
        peer_ip,
        srt_socket_id,
        srt_version,
        srt_flags,
    })
}

/// Encode a handshake to bytes.
pub fn encode_handshake(hs: &Handshake) -> Bytes {
    let mut buf = BytesMut::with_capacity(64);

    buf.put_u32(hs.version);
    buf.put_u16(hs.encryption);
    buf.put_u16(hs.extension);
    buf.put_u32(hs.initial_sequence);
    buf.put_u32(hs.mtu);
    buf.put_u32(hs.flow_window);
    buf.put_u32(hs.handshake_type as u32);
    buf.put_u32(hs.socket_id);
    buf.put_u32(hs.cookie);
    buf.put_slice(&hs.peer_ip);

    // Add SRT extension for HSv5
    if hs.version == SRT_VERSION_5 {
        // HS_EXT_HSREQ (0x0001) on requests, HS_EXT_HSRSP (0x0002) on
        // responses; libsrt matches the extension type strictly.
        buf.put_u16(hs.extension);
        buf.put_u16(4); // 4 x 32-bit words
        buf.put_u32(0x4A17); // SRT magic
        buf.put_u32(hs.srt_version);
        buf.put_u32(hs.srt_flags);
        buf.put_u32(0x0000_0078); // TSBPD delay: 120ms sender | 120ms receiver
    }

    buf.freeze()
}

/// Build an induction response handshake.
///
/// Per SRT spec, the induction exchange uses UDT version 4 with an HSREQ
/// extension; libsrt rejects a version-5 induction response.
pub fn induction_response(socket_id: u32, cookie: u32) -> Handshake {
    Handshake {
        version: SRT_VERSION_4,
        encryption: 0,
        extension: 0x0001, // HS_EXT_HSREQ
        initial_sequence: 0,
        mtu: 1500,
        flow_window: 8192,
        handshake_type: HandshakeType::Induction,
        socket_id,
        cookie,
        peer_ip: [0u8; 16],
        srt_socket_id: socket_id,
        srt_version: SRT_VERSION_5,
        srt_flags: 0,
    }
}

/// Build a conclusion response handshake.
pub fn conclusion_response(
    socket_id: u32,
    initial_sequence: u32,
    peer_socket_id: u32,
) -> Handshake {
    Handshake {
        version: SRT_VERSION_5,
        encryption: 0,
        extension: 0x0002, // HS_EXT_HSRSP
        initial_sequence,
        mtu: 1500,
        flow_window: 8192,
        handshake_type: HandshakeType::Conclusion,
        socket_id,
        cookie: 0,
        peer_ip: [0u8; 16],
        srt_socket_id: peer_socket_id,
        srt_version: SRT_VERSION_5,
        srt_flags: 0x0000_00BF,
    }
}

/// Wrap a handshake in a control packet.
pub fn handshake_control_packet(hs: &Handshake, socket_id: u32, timestamp: u32) -> ControlPacket {
    ControlPacket {
        control_type: ControlType::Handshake,
        type_specific: 0,
        additional_info: 0,
        timestamp,
        socket_id,
        data: encode_handshake(hs),
    }
}

/// Generate a simple SYN cookie from peer address.
pub fn generate_cookie(peer_addr: std::net::SocketAddr, timestamp: u32) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    peer_addr.hash(&mut hasher);
    timestamp.hash(&mut hasher);
    (hasher.finish() & 0xFFFF_FFFF) as u32 | 0x8000_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_roundtrip() {
        let hs = Handshake {
            version: SRT_VERSION_5,
            encryption: 0,
            extension: 0x0002,
            initial_sequence: 12345,
            mtu: 1500,
            flow_window: 8192,
            handshake_type: HandshakeType::Conclusion,
            socket_id: 0xDEADBEEF,
            cookie: 0x12345678,
            peer_ip: [0u8; 16],
            srt_socket_id: 0xDEADBEEF,
            srt_version: SRT_VERSION_5,
            srt_flags: 0xBF,
        };

        let encoded = encode_handshake(&hs);
        let parsed = parse_handshake(&encoded).unwrap();

        assert_eq!(parsed.version, SRT_VERSION_5);
        assert_eq!(parsed.initial_sequence, 12345);
        assert_eq!(parsed.socket_id, 0xDEADBEEF);
        assert_eq!(parsed.cookie, 0x12345678);
        assert_eq!(parsed.handshake_type, HandshakeType::Conclusion);
    }

    #[test]
    fn induction_response_structure() {
        let hs = induction_response(0x1111, 0x2222);
        assert_eq!(hs.handshake_type, HandshakeType::Induction);
        assert_eq!(hs.socket_id, 0x1111);
        assert_eq!(hs.cookie, 0x2222);
    }
}
