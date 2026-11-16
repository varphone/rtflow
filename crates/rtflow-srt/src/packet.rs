//! SRT packet header parsing and construction.
//!
//! SRT packets have a 16-byte header followed by payload.
//! The first bit distinguishes data packets (0) from control packets (1).
use bytes::{BufMut, Bytes, BytesMut};

/// SRT packet header size in bytes.
pub const SRT_HEADER_SIZE: usize = 16;

/// SRT data packet.
#[derive(Clone, Debug)]
pub struct DataPacket {
    /// Packet sequence number (31 bits).
    pub sequence: u32,
    /// Position in message: 0=middle, 1=first, 2=last, 3=single.
    pub position: u8,
    /// Message number.
    pub message_number: u32,
    /// Timestamp relative to connection start (microseconds).
    pub timestamp: u32,
    /// Destination socket ID.
    pub socket_id: u32,
    /// Order flag: deliver messages in order when set.
    pub ordered: bool,
    /// Retransmitted flag (set by the sender on retransmissions).
    pub retransmitted: bool,
    /// Key-based encryption flag: 0 = none, 1 = even key, 2 = odd key.
    pub key_flags: u8,
    /// Payload data.
    pub payload: Bytes,
}

/// SRT control packet types.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum ControlType {
    Handshake = 0x0000,
    Keepalive = 0x0001,
    Ack = 0x0002,
    Nak = 0x0003,
    CongestionWarning = 0x0004,
    Shutdown = 0x0005,
    AckAck = 0x0006,
    DropReq = 0x0007,
    PeerError = 0x0008,
    UserDefined = 0x7FFF,
}

impl ControlType {
    pub fn from_u16(v: u16) -> Option<Self> {
        match v {
            0x0000 => Some(Self::Handshake),
            0x0001 => Some(Self::Keepalive),
            0x0002 => Some(Self::Ack),
            0x0003 => Some(Self::Nak),
            0x0004 => Some(Self::CongestionWarning),
            0x0005 => Some(Self::Shutdown),
            0x0006 => Some(Self::AckAck),
            0x0007 => Some(Self::DropReq),
            0x0008 => Some(Self::PeerError),
            0x7FFF => Some(Self::UserDefined),
            _ => None,
        }
    }
}

/// SRT control packet.
#[derive(Clone, Debug)]
pub struct ControlPacket {
    /// Control packet type.
    pub control_type: ControlType,
    /// Type-specific information.
    pub type_specific: u16,
    /// Additional info (ACK sequence, etc.).
    pub additional_info: u32,
    /// Timestamp.
    pub timestamp: u32,
    /// Destination socket ID.
    pub socket_id: u32,
    /// Control data payload.
    pub data: Bytes,
}

/// A parsed SRT packet (either data or control).
#[derive(Clone, Debug)]
pub enum SrtPacket {
    Data(DataPacket),
    Control(ControlPacket),
}

/// Parse an SRT packet from raw bytes.
pub fn parse_packet(data: &[u8]) -> Option<SrtPacket> {
    if data.len() < SRT_HEADER_SIZE {
        return None;
    }

    let first_word = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let is_control = (first_word >> 31) & 1 == 1;

    if is_control {
        parse_control_packet(data, first_word)
    } else {
        parse_data_packet(data, first_word)
    }
}

/// Zero-copy parse variant for the receive loop: the payload becomes a
/// shared view (`Bytes::slice`) into the received buffer instead of a
/// per-packet copy. SRT payloads are typically 1316 bytes, so the hot
/// path saves one memcpy per packet.
pub fn parse_packet_borrowed(data: Bytes) -> Option<SrtPacket> {
    if data.len() < SRT_HEADER_SIZE {
        return None;
    }
    let first_word = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let is_control = (first_word >> 31) & 1 == 1;
    if is_control {
        parse_control_fields(&data, first_word).map(|(f, _)| {
            SrtPacket::Control(ControlPacket {
                control_type: f.control_type,
                type_specific: f.type_specific,
                additional_info: f.additional_info,
                timestamp: f.timestamp,
                socket_id: f.socket_id,
                data: data.slice(SRT_HEADER_SIZE..),
            })
        })
    } else {
        parse_data_fields(&data, first_word).map(|(f, _)| {
            SrtPacket::Data(DataPacket {
                sequence: f.sequence,
                position: f.position,
                ordered: f.ordered,
                retransmitted: f.retransmitted,
                key_flags: f.key_flags,
                message_number: f.message_number,
                timestamp: f.timestamp,
                socket_id: f.socket_id,
                payload: data.slice(SRT_HEADER_SIZE..),
            })
        })
    }
}

struct DataFields {
    sequence: u32,
    position: u8,
    ordered: bool,
    retransmitted: bool,
    key_flags: u8,
    message_number: u32,
    timestamp: u32,
    socket_id: u32,
}

struct ControlFields {
    control_type: ControlType,
    type_specific: u16,
    additional_info: u32,
    timestamp: u32,
    socket_id: u32,
}

fn parse_data_fields(data: &[u8], first_word: u32) -> Option<(DataFields, &[u8])> {
    let sequence = first_word & 0x7FFF_FFFF;
    let second_word = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let position = ((second_word >> 30) & 0x03) as u8;
    let ordered = (second_word >> 29) & 1 == 1;
    let retransmitted = (second_word >> 26) & 1 == 1;
    let key_flags = ((second_word >> 27) & 0x03) as u8;
    let message_number = second_word & 0x03FF_FFFF;
    let timestamp = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let socket_id = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
    Some((
        DataFields {
            sequence,
            position,
            ordered,
            retransmitted,
            key_flags,
            message_number,
            timestamp,
            socket_id,
        },
        &data[SRT_HEADER_SIZE..],
    ))
}

fn parse_control_fields(data: &[u8], first_word: u32) -> Option<(ControlFields, &[u8])> {
    let type_field = ((first_word >> 16) & 0x7FFF) as u16;
    let control_type = ControlType::from_u16(type_field)?;
    let type_specific = (first_word & 0xFFFF) as u16;
    let additional_info = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let timestamp = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let socket_id = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
    Some((
        ControlFields {
            control_type,
            type_specific,
            additional_info,
            timestamp,
            socket_id,
        },
        &data[SRT_HEADER_SIZE..],
    ))
}

fn parse_data_packet(data: &[u8], first_word: u32) -> Option<SrtPacket> {
    let (f, payload) = parse_data_fields(data, first_word)?;
    Some(SrtPacket::Data(DataPacket {
        sequence: f.sequence,
        position: f.position,
        ordered: f.ordered,
        retransmitted: f.retransmitted,
        key_flags: f.key_flags,
        message_number: f.message_number,
        timestamp: f.timestamp,
        socket_id: f.socket_id,
        payload: Bytes::copy_from_slice(payload),
    }))
}

fn parse_control_packet(data: &[u8], first_word: u32) -> Option<SrtPacket> {
    let (f, payload) = parse_control_fields(data, first_word)?;
    Some(SrtPacket::Control(ControlPacket {
        control_type: f.control_type,
        type_specific: f.type_specific,
        additional_info: f.additional_info,
        timestamp: f.timestamp,
        socket_id: f.socket_id,
        data: Bytes::copy_from_slice(payload),
    }))
}

/// Encode a data packet.
pub fn encode_data_packet(pkt: &DataPacket) -> BytesMut {
    let mut buf = BytesMut::with_capacity(SRT_HEADER_SIZE + pkt.payload.len());

    let first_word = pkt.sequence & 0x7FFF_FFFF; // data packet: bit 31 = 0
    buf.put_u32(first_word);

    let mut second_word = ((pkt.position as u32) << 30) | (pkt.message_number & 0x03FF_FFFF);
    if pkt.ordered {
        second_word |= 1 << 29;
    }
    if pkt.retransmitted {
        second_word |= 1 << 26;
    }
    second_word |= (u32::from(pkt.key_flags) & 0x03) << 27;
    buf.put_u32(second_word);

    buf.put_u32(pkt.timestamp);
    buf.put_u32(pkt.socket_id);
    buf.put_slice(&pkt.payload);

    buf
}

/// Encode a control packet.
pub fn encode_control_packet(pkt: &ControlPacket) -> BytesMut {
    let mut buf = BytesMut::with_capacity(SRT_HEADER_SIZE + pkt.data.len());

    let first_word = 0x8000_0000 | ((pkt.control_type as u32) << 16) | (pkt.type_specific as u32);
    buf.put_u32(first_word);

    buf.put_u32(pkt.additional_info);
    buf.put_u32(pkt.timestamp);
    buf.put_u32(pkt.socket_id);
    buf.put_slice(&pkt.data);

    buf
}

/// Build a keepalive control packet.
pub fn keepalive_packet(socket_id: u32, timestamp: u32) -> ControlPacket {
    ControlPacket {
        control_type: ControlType::Keepalive,
        type_specific: 0,
        additional_info: 0,
        timestamp,
        socket_id,
        data: Bytes::new(),
    }
}

/// Fields carried by a full ACK control packet.
#[derive(Clone, Copy, Debug, Default)]
pub struct AckFields {
    /// Acknowledgement Number (monotonic, starts at 1).
    pub ack_sequence: u32,
    /// First unacknowledged data packet sequence number.
    pub last_ack_sequence: u32,
    /// Round-trip time estimate, microseconds.
    pub rtt_us: u32,
    /// RTT variance, microseconds.
    pub rtt_var_us: u32,
    /// Available receiver buffer, packets.
    pub available_buffer: u32,
    /// Packet receiving rate, packets/second.
    pub packet_rate: u32,
    /// Estimated link capacity, packets/second.
    pub link_capacity: u32,
    /// Receiving rate, bytes/second.
    pub byte_rate: u32,
}

/// Build a full ACK control packet (7 CIF words, as libsrt sends).
pub fn ack_packet(fields: &AckFields, socket_id: u32, timestamp: u32) -> ControlPacket {
    let mut data = BytesMut::with_capacity(28);
    data.put_u32(fields.last_ack_sequence & 0x7FFF_FFFF);
    data.put_u32(fields.rtt_us);
    data.put_u32(fields.rtt_var_us);
    data.put_u32(fields.available_buffer);
    data.put_u32(fields.packet_rate);
    data.put_u32(fields.link_capacity);
    data.put_u32(fields.byte_rate);
    ControlPacket {
        control_type: ControlType::Ack,
        // The 16-bit subtype is reserved for ACK packets.
        type_specific: 0,
        additional_info: fields.ack_sequence,
        timestamp,
        socket_id,
        data: data.freeze(),
    }
}

/// Parse an ACK control packet (full, small or light).
pub fn parse_ack(packet: &ControlPacket) -> Option<AckFields> {
    if packet.control_type != ControlType::Ack || packet.data.len() < 4 {
        return None;
    }
    let word = |index: usize| -> Option<u32> {
        let start = index * 4;
        packet
            .data
            .get(start..start + 4)
            .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    Some(AckFields {
        ack_sequence: packet.additional_info,
        last_ack_sequence: word(0)? & 0x7FFF_FFFF,
        rtt_us: word(1).unwrap_or(0),
        rtt_var_us: word(2).unwrap_or(0),
        available_buffer: word(3).unwrap_or(0),
        packet_rate: word(4).unwrap_or(0),
        link_capacity: word(5).unwrap_or(0),
        byte_rate: word(6).unwrap_or(0),
    })
}

/// Build an ACKACK control packet acknowledging the given ACK sequence.
pub fn ackack_packet(ack_sequence: u32, socket_id: u32, timestamp: u32) -> ControlPacket {
    ControlPacket {
        control_type: ControlType::AckAck,
        type_specific: 0,
        additional_info: ack_sequence,
        timestamp,
        socket_id,
        data: Bytes::new(),
    }
}

/// Build a user-defined extension control packet (KMREQ/KMRSP refresh).
pub fn user_defined_packet(
    extended_type: u16,
    data: Bytes,
    socket_id: u32,
    timestamp: u32,
) -> ControlPacket {
    ControlPacket {
        control_type: ControlType::UserDefined,
        type_specific: extended_type,
        additional_info: 0,
        timestamp,
        socket_id,
        data,
    }
}

/// Build a SHUTDOWN control packet.
pub fn shutdown_packet(socket_id: u32, timestamp: u32) -> ControlPacket {
    ControlPacket {
        control_type: ControlType::Shutdown,
        type_specific: 0,
        additional_info: 0,
        timestamp,
        socket_id,
        data: Bytes::new(),
    }
}

/// Build a DROPREQ control packet (sender → receiver).
pub fn drop_request_packet(
    message_number: u32,
    first_sequence: u32,
    last_sequence: u32,
    socket_id: u32,
    timestamp: u32,
) -> ControlPacket {
    let mut data = BytesMut::with_capacity(8);
    data.put_u32(first_sequence & 0x7FFF_FFFF);
    data.put_u32(last_sequence & 0x7FFF_FFFF);
    ControlPacket {
        control_type: ControlType::DropReq,
        type_specific: 0,
        additional_info: message_number,
        timestamp,
        socket_id,
        data: data.freeze(),
    }
}

/// Build a NAK (loss report) control packet.
///
/// The loss list is encoded as pairs of (start, end) inclusive sequence ranges.
/// Single-packet losses use start == end.
pub fn nak_packet(losses: &[(u32, u32)], socket_id: u32, timestamp: u32) -> ControlPacket {
    let mut data = BytesMut::with_capacity(losses.len() * 8);
    for &(start, end) in losses {
        if start == end {
            // Single loss: just the sequence number (bit 31 = 0)
            data.put_u32(start & 0x7FFF_FFFF);
        } else {
            // Range: start (bit 31 = 1) followed by end
            data.put_u32(0x8000_0000 | (start & 0x7FFF_FFFF));
            data.put_u32(end & 0x7FFF_FFFF);
        }
    }
    ControlPacket {
        control_type: ControlType::Nak,
        type_specific: 0,
        additional_info: 0,
        timestamp,
        socket_id,
        data: data.freeze(),
    }
}

/// Parse a NAK loss list from control data.
pub fn parse_nak_loss_list(data: &[u8]) -> Vec<(u32, u32)> {
    let mut losses = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let word = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        i += 4;
        if (word >> 31) & 1 == 1 {
            // Range: next word is end
            let start = word & 0x7FFF_FFFF;
            if i + 4 <= data.len() {
                let end = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
                i += 4;
                losses.push((start, end & 0x7FFF_FFFF));
            }
        } else {
            losses.push((word, word));
        }
    }
    losses
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_packet_roundtrip() {
        let pkt = DataPacket {
            sequence: 12345,
            position: 3, // single packet message
            message_number: 42,
            timestamp: 1000000,
            socket_id: 0xDEADBEEF,
            ordered: true,
            retransmitted: false,
            key_flags: 0,
            payload: Bytes::from(vec![1, 2, 3, 4, 5]),
        };

        let encoded = encode_data_packet(&pkt);
        let parsed = parse_packet(&encoded).unwrap();

        match parsed {
            SrtPacket::Data(d) => {
                assert_eq!(d.sequence, 12345);
                assert_eq!(d.position, 3);
                assert_eq!(d.message_number, 42);
                assert_eq!(d.timestamp, 1000000);
                assert_eq!(d.socket_id, 0xDEADBEEF);
                assert_eq!(&d.payload[..], &[1, 2, 3, 4, 5]);
            }
            _ => panic!("expected data packet"),
        }
    }

    #[test]
    fn control_packet_roundtrip() {
        let pkt = keepalive_packet(0x12345678, 500000);
        let encoded = encode_control_packet(&pkt);
        let parsed = parse_packet(&encoded).unwrap();

        match parsed {
            SrtPacket::Control(c) => {
                assert_eq!(c.control_type, ControlType::Keepalive);
                assert_eq!(c.socket_id, 0x12345678);
                assert_eq!(c.timestamp, 500000);
            }
            _ => panic!("expected control packet"),
        }
    }
}
