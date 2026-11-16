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
/// Total reassembly budget across all chunk streams on one connection.
pub const MAX_REASSEMBLY_BYTES: usize = 32 * 1024 * 1024;
/// Maximum concurrent chunk streams. RTMP permits 65,536 CSIDs, but a real
/// publisher uses a handful; keeping the map bounded avoids per-connection
/// state growth from a peer that only sends headers.
pub const MAX_CHUNK_STREAMS: usize = 4096;

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
    /// True after a fmt0/1/2 header opened a message and until its payload is
    /// complete. fmt3 with this false starts a *new* message (reusing the
    /// previous delta); with it true it is a continuation and must not apply
    /// the delta again.
    in_message: bool,
}

/// Dechunker: accumulates incoming bytes and emits complete messages.
pub struct Dechunker {
    chunk_size: u32,
    streams: HashMap<u32, ChunkStreamState>,
    pending: BytesMut,
    /// Bytes retained in per-stream message buffers.
    buffered: usize,
    /// Set when input/reassembly exceeded its budget.
    overflowed: bool,
}

impl Default for Dechunker {
    fn default() -> Self {
        Self::new()
    }
}

impl Dechunker {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            streams: HashMap::new(),
            pending: BytesMut::new(),
            buffered: 0,
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
            self.buffered = 0;
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

        // Basic header.
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

        // Parse the message header into locals first. Nothing is committed to
        // the chunk-stream state until the whole chunk payload is available:
        // committing a timestamp delta and then returning NeedMoreData makes
        // the next poll apply the same delta again, which inflates timestamps
        // according to how TCP happened to split the stream.
        let mut new_message = true;
        let mut timestamp = 0u32;
        let mut delta = 0u32;
        let mut msg_length: Option<u32> = None;
        let mut msg_type: Option<u8> = None;
        let mut msg_stream_id: Option<u32> = None;
        let mut extended = false;

        match fmt {
            0 => {
                if pos + 11 > data.len() {
                    return Err(ChunkError::NeedMoreData);
                }
                let b = &data[pos..pos + 11];
                let ts = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                msg_length = Some(((b[3] as u32) << 16) | ((b[4] as u32) << 8) | (b[5] as u32));
                msg_type = Some(b[6]);
                msg_stream_id = Some(u32::from_le_bytes([b[7], b[8], b[9], b[10]]));
                pos += 11;
                if ts == 0xFFFFFF {
                    if pos + 4 > data.len() {
                        return Err(ChunkError::NeedMoreData);
                    }
                    timestamp = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]);
                    pos += 4;
                    extended = true;
                } else {
                    timestamp = ts;
                }
                delta = 0;
            }
            1 => {
                if pos + 7 > data.len() {
                    return Err(ChunkError::NeedMoreData);
                }
                let b = &data[pos..pos + 7];
                delta = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                msg_length = Some(((b[3] as u32) << 16) | ((b[4] as u32) << 8) | (b[5] as u32));
                msg_type = Some(b[6]);
                pos += 7;
                if delta == 0xFFFFFF {
                    if pos + 4 > data.len() {
                        return Err(ChunkError::NeedMoreData);
                    }
                    delta = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]);
                    pos += 4;
                    extended = true;
                }
            }
            2 => {
                if pos + 3 > data.len() {
                    return Err(ChunkError::NeedMoreData);
                }
                let b = &data[pos..pos + 3];
                delta = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
                pos += 3;
                if delta == 0xFFFFFF {
                    if pos + 4 > data.len() {
                        return Err(ChunkError::NeedMoreData);
                    }
                    delta = u32::from_be_bytes([
                        data[pos],
                        data[pos + 1],
                        data[pos + 2],
                        data[pos + 3],
                    ]);
                    pos += 4;
                    extended = true;
                }
            }
            _ => {
                // fmt 3: either a continuation chunk or a new message reusing
                // the previous header. Only a new message advances the
                // timestamp by the stored delta.
                new_message = self
                    .streams
                    .get(&csid)
                    .is_none_or(|state| !state.in_message);
                if new_message && let Some(state) = self.streams.get(&csid) {
                    delta = state.timestamp_delta;
                }
                if self
                    .streams
                    .get(&csid)
                    .is_some_and(|state| state.extended_timestamp)
                {
                    if pos + 4 > data.len() {
                        return Err(ChunkError::NeedMoreData);
                    }
                    pos += 4;
                    extended = true;
                }
            }
        }

        let state = self.streams.get(&csid);
        if fmt >= 2 && state.is_none() {
            // fmt 2/3 without a prior type 0/1 header is unrecoverable.
            return Err(ChunkError::Protocol);
        }
        if !new_message && state.is_none() {
            return Err(ChunkError::Protocol);
        }
        let msg_length = msg_length
            .or_else(|| state.map(|state| state.msg_length))
            .ok_or(ChunkError::Protocol)?;
        let msg_type = msg_type
            .or_else(|| state.map(|state| state.msg_type))
            .ok_or(ChunkError::Protocol)?;
        let msg_stream_id = msg_stream_id
            .or_else(|| state.map(|state| state.msg_stream_id))
            .ok_or(ChunkError::Protocol)?;
        let buffered = if new_message {
            0
        } else {
            state.map(|state| state.buffer.len() as u32).unwrap_or(0)
        };
        let remaining = msg_length.saturating_sub(buffered);
        let take = remaining.min(self.chunk_size);
        if pos + take as usize > data.len() {
            return Err(ChunkError::NeedMoreData);
        }
        let payload_chunk = data[pos..pos + take as usize].to_vec();
        let consumed = pos + take as usize;

        if !self.streams.contains_key(&csid) && self.streams.len() >= MAX_CHUNK_STREAMS {
            self.overflowed = true;
            return Err(ChunkError::Protocol);
        }
        let state = self.streams.entry(csid).or_default();
        if new_message {
            self.buffered = self.buffered.saturating_sub(state.buffer.len());
            state.buffer.clear();
            state.msg_length = msg_length;
            state.msg_type = msg_type;
            state.msg_stream_id = msg_stream_id;
            state.in_message = true;
            if fmt == 0 {
                state.timestamp = timestamp;
                state.timestamp_delta = 0;
            } else {
                state.timestamp_delta = delta;
                state.timestamp = state.timestamp.wrapping_add(delta);
            }
            state.extended_timestamp = extended;
        } else if extended {
            state.extended_timestamp = true;
        }
        state.buffer.extend_from_slice(&payload_chunk);
        self.buffered = self.buffered.saturating_add(take as usize);
        let _ = self.pending.split_to(consumed);

        if state.buffer.len() as u32 >= state.msg_length {
            self.buffered = self.buffered.saturating_sub(state.buffer.len());
            state.in_message = false;
            let payload = state.buffer.split().freeze();
            Ok(Some(RtmpMessage {
                chunk_stream_id: csid,
                timestamp: state.timestamp,
                msg_type: state.msg_type,
                msg_stream_id: state.msg_stream_id,
                payload,
            }))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug)]
enum ChunkError {
    NeedMoreData,
    // Protocol-level failure (experimental RTMP; kept for the state
    // machine shape even though no path constructs it yet).
    #[allow(dead_code)]
    Protocol,
}

/// Chunker: splits messages into chunks for sending.
pub struct Chunker {
    chunk_size: u32,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
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
            out.put_u8(fmt << 6);
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
    fn chunk_stream_state_is_bounded() {
        let chunker = Chunker::new();
        let mut dechunker = Dechunker::new();
        for csid in 0..(MAX_CHUNK_STREAMS as u32 + 2) {
            let msg = RtmpMessage {
                chunk_stream_id: csid + 4,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 1,
                payload: Bytes::from_static(&[0]),
            };
            dechunker.feed(&chunker.encode(&msg));
            let _ = dechunker.poll_messages();
        }
        assert!(dechunker.overflowed);
    }

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
    fn fmt3_new_messages_advance_by_delta() {
        let payload = vec![0u8; 20];
        let mut encoded = bytes::BytesMut::new();
        // fmt0, csid 4, ts 0, len 20, type audio, stream 1
        encoded.extend_from_slice(&[0x04, 0, 0, 0, 0, 0, 20, 8, 0, 0, 0, 1]);
        encoded.extend_from_slice(&payload);
        // fmt1, delta 21
        encoded.extend_from_slice(&[0x44, 0, 0, 21, 0, 0, 20, 8]);
        encoded.extend_from_slice(&payload);
        // fmt3 x2
        encoded.extend_from_slice(&[0xC4]);
        encoded.extend_from_slice(&payload);
        encoded.extend_from_slice(&[0xC4]);
        encoded.extend_from_slice(&payload);

        let mut dechunker = Dechunker::new();
        dechunker.feed(&encoded);
        let messages = dechunker.poll_messages();
        let stamps: Vec<u32> = messages.iter().map(|m| m.timestamp).collect();
        assert_eq!(stamps, vec![0, 21, 42, 63]);
    }

    #[test]
    fn incremental_feed_preserves_timestamps() {
        let payload = vec![0u8; 20];
        let mut encoded = bytes::BytesMut::new();
        encoded.extend_from_slice(&[0x04, 0, 0, 0, 0, 0, 20, 8, 0, 0, 0, 1]);
        encoded.extend_from_slice(&payload);
        encoded.extend_from_slice(&[0x44, 0, 0, 21, 0, 0, 20, 8]);
        encoded.extend_from_slice(&payload);
        for _ in 0..5 {
            encoded.extend_from_slice(&[0xC4]);
            encoded.extend_from_slice(&payload);
        }

        // TCP may split headers and payloads at any byte boundary. The
        // dechunker must not commit a timestamp delta before the whole chunk
        // is available, or it applies the same delta again on the next poll.
        for step in [1usize, 3, 7, encoded.len()] {
            let mut dechunker = Dechunker::new();
            let mut messages = Vec::new();
            for part in encoded.chunks(step) {
                dechunker.feed(part);
                messages.extend(dechunker.poll_messages());
            }
            let stamps: Vec<u32> = messages.iter().map(|m| m.timestamp).collect();
            assert_eq!(
                stamps,
                vec![0, 21, 42, 63, 84, 105, 126],
                "incremental feed step {step}"
            );
            assert_eq!(messages.len(), 7);
        }
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
