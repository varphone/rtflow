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

fn parse_data_packet(data: &[u8], first_word: u32) -> Option<SrtPacket> {
    let sequence = first_word & 0x7FFF_FFFF;
    let second_word = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let position = ((second_word >> 30) & 0x03) as u8;
    let message_number = second_word & 0x03FF_FFFF;
    let timestamp = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let socket_id = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);

    Some(SrtPacket::Data(DataPacket {
        sequence,
        position,
        message_number,
        timestamp,
        socket_id,
        payload: Bytes::copy_from_slice(&data[SRT_HEADER_SIZE..]),
    }))
}

fn parse_control_packet(data: &[u8], first_word: u32) -> Option<SrtPacket> {
    let type_field = ((first_word >> 16) & 0x7FFF) as u16;
    let control_type = ControlType::from_u16(type_field)?;
    let type_specific = (first_word & 0xFFFF) as u16;
    let additional_info = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let timestamp = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let socket_id = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);

    Some(SrtPacket::Control(ControlPacket {
        control_type,
        type_specific,
        additional_info,
        timestamp,
        socket_id,
        data: Bytes::copy_from_slice(&data[SRT_HEADER_SIZE..]),
    }))
}

/// Encode a data packet.
pub fn encode_data_packet(pkt: &DataPacket) -> BytesMut {
    let mut buf = BytesMut::with_capacity(SRT_HEADER_SIZE + pkt.payload.len());

    let first_word = pkt.sequence & 0x7FFF_FFFF; // data packet: bit 31 = 0
    buf.put_u32(first_word);

    let second_word = ((pkt.position as u32) << 30) | (pkt.message_number & 0x03FF_FFFF);
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

/// Build an ACK control packet.
pub fn ack_packet(
    ack_seq: u32,
    ack_number: u32,
    socket_id: u32,
    timestamp: u32,
) -> ControlPacket {
    let mut data = BytesMut::with_capacity(4);
    data.put_u32(ack_number);
    ControlPacket {
        control_type: ControlType::Ack,
        // Per SRT spec the type-specific field carries the ACK sequence
        // counter; the confirmed sequence goes in the payload.
        type_specific: ack_seq as u16,
        additional_info: 0,
        timestamp,
        socket_id,
        data: data.freeze(),
    }
}

/// Build an ACKACK control packet.
pub fn ackack_packet(ack_number: u32, socket_id: u32, timestamp: u32) -> ControlPacket {
    ControlPacket {
        control_type: ControlType::AckAck,
        type_specific: 0,
        additional_info: ack_number,
        timestamp,
        socket_id,
        data: Bytes::new(),
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
