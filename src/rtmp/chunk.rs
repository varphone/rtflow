//! RTMP chunk stream protocol (chunking and dechunking).
//!
//! Implements chunk basic header (fmt 0-3), message header, and
//! chunk size negotiation per the RTMP specification.
use bytes::{BufMut, Bytes, BytesMut};
use std::collections::HashMap;
use tracing::warn;

/// Default chunk size per RTMP spec.
pub const DEFAULT_CHUNK_SIZE: u32 = 128;

/// Maximum chunk size we allow (16 MB).
pub const MAX_CHUNK_SIZE: u32 = 16 * 1024 * 1024;

/// Maximum bytes buffered waiting for more chunk data. A peer that
/// repeatedly declares a large message but never completes it must not
/// be able to grow `pending` without bound.
pub const MAX_PENDING_BYTES: usize = MAX_CHUNK_SIZE as usize + 4096;

/// RTMP message type IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MessageType {
    SetChunkSize = 1,
    Abort = 2,
    Acknowledgement = 3,
    UserControl = 4,
    WindowAckSize = 5,
    SetPeerBandwidth = 6,
    Audio = 8,
    Video = 9,
    DataAmf3 = 15,
    CommandAmf3 = 17,
    DataAmf0 = 18,
    CommandAmf0 = 20,
    Aggregate = 22,
}

impl MessageType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::SetChunkSize),
            2 => Some(Self::Abort),
            3 => Some(Self::Acknowledgement),
            4 => Some(Self::UserControl),
            5 => Some(Self::WindowAckSize),
            6 => Some(Self::SetPeerBandwidth),
            8 => Some(Self::Audio),
            9 => Some(Self::Video),
            15 => Some(Self::DataAmf3),
            17 => Some(Self::CommandAmf3),
            18 => Some(Self::DataAmf0),
            20 => Some(Self::CommandAmf0),
            22 => Some(Self::Aggregate),
            _ => None,
        }
    }
}

/// A complete RTMP message (after reassembly from chunks).
#[derive(Clone, Debug)]
pub struct RtmpMessage {
    pub chunk_stream_id: u32,
    pub timestamp: u32,
    pub msg_type: u8,
    pub msg_stream_id: u32,
    pub payload: Bytes,
}

/// Chunk stream state for reassembly.
#[derive(Default)]
struct ChunkStreamState {
    timestamp: u32,
    timestamp_delta: u32,
    msg_length: u32,
    msg_type: u8,
    msg_stream_id: u32,
    buffer: BytesMut,
    extended_timestamp: bool,
}

/// Dechunker: accumulates incoming bytes and emits complete messages.
pub struct Dechunker {
    chunk_size: u32,
    streams: HashMap<u32, ChunkStreamState>,
    pending: BytesMut,
    /// Set when buffered input exceeded [`MAX_PENDING_BYTES`]; the
    /// connection should be dropped.
    overflowed: bool,
}

impl Dechunker {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            streams: HashMap::new(),
            pending: BytesMut::new(),
            overflowed: false,
        }
    }

    /// Whether the input buffer overflowed and the peer must be dropped.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Set the incoming chunk size (from peer's SetChunkSize message).
    pub fn set_chunk_size(&mut self, size: u32) {
        self.chunk_size = size.min(MAX_CHUNK_SIZE);
    }

    /// Number of bytes buffered waiting for a complete message.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Feed raw bytes from the network.
    pub fn feed(&mut self, data: &[u8]) {
        if self.overflowed {
            return;
        }
        if self.pending.len().saturating_add(data.len()) > MAX_PENDING_BYTES {
            warn!(
                "RTMP: dechunker input buffer overflow ({} bytes buffered), dropping connection",
                self.pending.len()
            );
            self.pending.clear();
            self.streams.clear();
            self.overflowed = true;
            return;
        }
        self.pending.extend_from_slice(data);
    }

    /// Try to extract complete messages from buffered data.
    pub fn poll_messages(&mut self) -> Vec<RtmpMessage> {
        let mut messages = Vec::new();
        loop {
            match self.try_read_chunk() {
                Ok(Some(msg)) => {
                    // A SetChunkSize control message takes effect immediately
                    // after itself (RTMP spec): later messages in the same
                    // buffer must be parsed with the new chunk size. Without
                    // this, parsing depends on TCP segmentation — a peer
                    // that coalesces SetChunkSize with a large message
                    // leaves the large message truncated.
                    if msg.msg_type == MessageType::SetChunkSize as u8 && msg.payload.len() >= 4 {
                        let size = u32::from_be_bytes([
                            msg.payload[0],
                            msg.payload[1],
                            msg.payload[2],
                            msg.payload[3],
                        ]);
                        if (1..=MAX_CHUNK_SIZE).contains(&size) {
                            self.chunk_size = size;
                        }
                    }
                    messages.push(msg);
                }
                Ok(None) => {} // chunk consumed but message incomplete, continue
                Err(ChunkError::NeedMoreData) => break,
                Err(ChunkError::Protocol) => {
                    self.pending.clear();
                    break;
                }
            }
        }
        messages
    }

    fn try_read_chunk(&mut self) -> Result<Option<RtmpMessage>, ChunkError> {
        if self.pending.is_empty() {
            return Err(ChunkError::NeedMoreData);
        }

        let data = &self.pending[..];
        let mut pos = 0;

        // Basic header
        let first = data[pos];
        pos += 1;
        let fmt = (first >> 6) & 0x03;
        let mut csid = (first & 0x3F) as u32;

        if csid == 0 {
            if pos >= data.len() {
                return Err(ChunkError::NeedMoreData);
            }
            csid = data[pos] as u32 + 64;
            pos += 1;
        } else if csid == 1 {
            if pos + 1 >= data.len() {
                return Err(ChunkError::NeedMoreData);
            }
            csid = data[pos] as u32 + (data[pos + 1] as u32) * 256 + 64;
            pos += 2;
        }

        // Message header size depends on fmt
        let header_size = match fmt {
            0 => 11,
            1 => 7,
            2 => 3,
            _ => 0,
        };

        if pos + header_size > data.len() {
            return Err(ChunkError::NeedMoreData);
        }

        let header_start = pos;
        pos += header_size;

        // Check for extended timestamp
        let needs_extended = if header_size > 0 {
            let ts_field = match fmt {
                0 => {
                    let b = &data[header_start..header_start + 3];
                    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32)
                }
                1 | 2 => {
                    let b = &data[header_start..header_start + 3];
                    ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32)
                }
                _ => 0,
            };
            ts_field == 0xFFFFFF
        } else {
            false
        };

        if needs_extended {
            if pos + 4 > data.len() {
                return Err(ChunkError::NeedMoreData);
            }
            pos += 4;
        }

        // Get or create chunk stream state
        let state = self.streams.entry(csid).or_default();

        // Parse header fields
        match fmt {
            0 => {
                let b = &data[header_start..header_start + 11];
                let ts = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                state.msg_length = ((b[3] as u32) << 16) | ((b[4] as u32) << 8) | (b[5] as u32);
                state.msg_type = b[6];
                state.msg_stream_id = u32::from_le_bytes([b[7], b[8], b[9], b[10]]);
                if ts == 0xFFFFFF {
                    let ext = &data[header_start + 11..header_start + 15];
                    state.timestamp = u32::from_be_bytes([ext[0], ext[1], ext[2], ext[3]]);
                    state.extended_timestamp = true;
                    // fmt0 carries an absolute timestamp: the delta from any
                    // previous fmt1/2/3 header must not leak into the next
                    // delta-based header.
                    state.timestamp_delta = 0;
                } else {
                    state.timestamp = ts;
                    state.timestamp_delta = 0;
                    state.extended_timestamp = false;
                }
                state.buffer.clear();
            }
            1 => {
                let b = &data[header_start..header_start + 7];
                let delta = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                state.msg_length = ((b[3] as u32) << 16) | ((b[4] as u32) << 8) | (b[5] as u32);
                state.msg_type = b[6];
                if delta == 0xFFFFFF {
                    let ext = &data[header_start + 7..header_start + 11];
                    state.timestamp_delta = u32::from_be_bytes([ext[0], ext[1], ext[2], ext[3]]);
                    state.extended_timestamp = true;
                } else {
                    state.timestamp_delta = delta;
                    state.extended_timestamp = false;
                }
                state.timestamp = state.timestamp.wrapping_add(state.timestamp_delta);
                state.buffer.clear();
            }
            2 => {
                let b = &data[header_start..header_start + 3];
                let delta = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                if delta == 0xFFFFFF {
                    let ext = &data[header_start + 3..header_start + 7];
                    state.timestamp_delta = u32::from_be_bytes([ext[0], ext[1], ext[2], ext[3]]);
                    state.extended_timestamp = true;
                } else {
                    state.timestamp_delta = delta;
                    state.extended_timestamp = false;
                }
                state.timestamp = state.timestamp.wrapping_add(state.timestamp_delta);
            }
            _ => {
                // fmt 3: continuation
                if state.buffer.is_empty() {
                    // New message with same type/length: apply delta
                    state.timestamp = state.timestamp.wrapping_add(state.timestamp_delta);
                }
            }
        }

        // Calculate how many payload bytes are in this chunk
        let remaining_msg = state.msg_length.saturating_sub(state.buffer.len() as u32);
        let chunk_payload_len = remaining_msg.min(self.chunk_size);

        if pos + chunk_payload_len as usize > data.len() {
            return Err(ChunkError::NeedMoreData);
        }

        state
            .buffer
            .extend_from_slice(&data[pos..pos + chunk_payload_len as usize]);
        pos += chunk_payload_len as usize;

        // Consume processed bytes
        let _ = self.pending.split_to(pos);

        // Check if message is complete
        if state.buffer.len() as u32 >= state.msg_length {
            let payload = state.buffer.split().freeze();
            let msg = RtmpMessage {
                chunk_stream_id: csid,
                timestamp: state.timestamp,
                msg_type: state.msg_type,
                msg_stream_id: state.msg_stream_id,
                payload,
            };
            Ok(Some(msg))
        } else {
            Ok(None) // chunk consumed, message incomplete
        }
    }
}

#[derive(Debug)]
enum ChunkError {
    NeedMoreData,
    Protocol,
}

/// Chunker: splits messages into chunks for sending.
pub struct Chunker {
    chunk_size: u32,
}

impl Chunker {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Set outgoing chunk size.
    pub fn set_chunk_size(&mut self, size: u32) {
        self.chunk_size = size.min(MAX_CHUNK_SIZE);
    }

    pub fn chunk_size(&self) -> u32 {
        self.chunk_size
    }

    /// Encode a complete message into chunks, reusing `out`'s capacity.
    pub fn encode_into(&self, msg: &RtmpMessage, out: &mut BytesMut) {
        out.clear();
        let payload = &msg.payload;
        let mut offset = 0;
        let mut first = true;

        while offset < payload.len() {
            if first {
                // fmt 0 header
                self.write_basic_header(out, 0, msg.chunk_stream_id);
                self.write_type0_header(
                    out,
                    msg.timestamp,
                    payload.len() as u32,
                    msg.msg_type,
                    msg.msg_stream_id,
                );
                first = false;
            } else {
                // fmt 3 header (continuation)
                self.write_basic_header(out, 3, msg.chunk_stream_id);
            }

            let end = (offset + self.chunk_size as usize).min(payload.len());
            out.put_slice(&payload[offset..end]);
            offset = end;
        }

        // Handle empty payload
        if payload.is_empty() {
            self.write_basic_header(out, 0, msg.chunk_stream_id);
            self.write_type0_header(out, msg.timestamp, 0, msg.msg_type, msg.msg_stream_id);
        }
    }

    /// Encode a complete message into chunks (allocates a fresh buffer).
    pub fn encode(&self, msg: &RtmpMessage) -> BytesMut {
        let mut out = BytesMut::with_capacity(msg.payload.len() + 64);
        self.encode_into(msg, &mut out);
        out
    }

    fn write_basic_header(&self, out: &mut BytesMut, fmt: u8, csid: u32) {
        if csid >= 64 + 256 {
            // 3-byte form
            let csid = csid - 64;
            out.put_u8((fmt << 6) | 1);
            out.put_u8((csid & 0xFF) as u8);
            out.put_u8(((csid >> 8) & 0xFF) as u8);
        } else if csid >= 64 {
            // 2-byte form
            out.put_u8((fmt << 6) | 0);
            out.put_u8((csid - 64) as u8);
        } else {
            out.put_u8((fmt << 6) | (csid as u8));
        }
    }

    fn write_type0_header(
        &self,
        out: &mut BytesMut,
        timestamp: u32,
        msg_length: u32,
        msg_type: u8,
        msg_stream_id: u32,
    ) {
        if timestamp >= 0xFFFFFF {
            out.put_u8(0xFF);
            out.put_u8(0xFF);
            out.put_u8(0xFF);
        } else {
            out.put_u8((timestamp >> 16) as u8);
            out.put_u8((timestamp >> 8) as u8);
            out.put_u8(timestamp as u8);
        }
        out.put_u8((msg_length >> 16) as u8);
        out.put_u8((msg_length >> 8) as u8);
        out.put_u8(msg_length as u8);
        out.put_u8(msg_type);
        out.put_u32_le(msg_stream_id);
        if timestamp >= 0xFFFFFF {
            out.put_u32(timestamp);
        }
    }
}

/// Build a SetChunkSize protocol control message.
pub fn set_chunk_size_message(size: u32) -> RtmpMessage {
    let mut payload = BytesMut::with_capacity(4);
    payload.put_u32(size & 0x7FFF_FFFF);
    RtmpMessage {
        chunk_stream_id: 2,
        timestamp: 0,
        msg_type: MessageType::SetChunkSize as u8,
        msg_stream_id: 0,
        payload: payload.freeze(),
    }
}

/// Build a WindowAckSize message.
pub fn window_ack_size_message(size: u32) -> RtmpMessage {
    let mut payload = BytesMut::with_capacity(4);
    payload.put_u32(size);
    RtmpMessage {
        chunk_stream_id: 2,
        timestamp: 0,
        msg_type: MessageType::WindowAckSize as u8,
        msg_stream_id: 0,
        payload: payload.freeze(),
    }
}

/// Build a SetPeerBandwidth message.
pub fn set_peer_bandwidth_message(size: u32, limit_type: u8) -> RtmpMessage {
    let mut payload = BytesMut::with_capacity(5);
    payload.put_u32(size);
    payload.put_u8(limit_type);
    RtmpMessage {
        chunk_stream_id: 2,
        timestamp: 0,
        msg_type: MessageType::SetPeerBandwidth as u8,
        msg_stream_id: 0,
        payload: payload.freeze(),
    }
}

/// Build a UserControl message (e.g. StreamBegin).
pub fn user_control_message(event_type: u16, data: u32) -> RtmpMessage {
    let mut payload = BytesMut::with_capacity(6);
    payload.put_u16(event_type);
    payload.put_u32(data);
    RtmpMessage {
        chunk_stream_id: 2,
        timestamp: 0,
        msg_type: MessageType::UserControl as u8,
        msg_stream_id: 0,
        payload: payload.freeze(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_and_dechunk_roundtrip() {
        let msg = RtmpMessage {
            chunk_stream_id: 3,
            timestamp: 1000,
            msg_type: MessageType::CommandAmf0 as u8,
            msg_stream_id: 1,
            payload: Bytes::from(vec![0u8; 300]), // larger than default chunk size
        };

        let chunker = Chunker::new();
        let encoded = chunker.encode(&msg);

        let mut dechunker = Dechunker::new();
        dechunker.feed(&encoded);
        let messages = dechunker.poll_messages();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].chunk_stream_id, 3);
        assert_eq!(messages[0].timestamp, 1000);
        assert_eq!(messages[0].msg_type, MessageType::CommandAmf0 as u8);
        assert_eq!(messages[0].msg_stream_id, 1);
        assert_eq!(messages[0].payload.len(), 300);
    }

    #[test]
    fn chunk_small_message() {
        let msg = RtmpMessage {
            chunk_stream_id: 4,
            timestamp: 0,
            msg_type: MessageType::Audio as u8,
            msg_stream_id: 1,
            payload: Bytes::from(vec![1u8; 64]),
        };

        let chunker = Chunker::new();
        let encoded = chunker.encode(&msg);

        let mut dechunker = Dechunker::new();
        dechunker.feed(&encoded);
        let messages = dechunker.poll_messages();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload.len(), 64);
    }

    #[test]
    fn set_chunk_size_message_encode() {
        let msg = set_chunk_size_message(4096);
        assert_eq!(msg.msg_type, MessageType::SetChunkSize as u8);
        assert_eq!(msg.payload.len(), 4);
        assert_eq!(
            u32::from_be_bytes([
                msg.payload[0],
                msg.payload[1],
                msg.payload[2],
                msg.payload[3]
            ]),
            4096
        );
    }
}
