//! Streaming MPEG-TS demuxer for SRT input.
//!
//! Processes TS packets as they arrive from the network and produces
//! AccessUnits suitable for pushing to a mount.
use bytes::BytesMut;
use std::collections::HashMap;
use std::time::Duration;

use rtflow_media::{AccessUnit, Codec, MediaDescription, MediaKind, TrackDescription};

const TS_PACKET_SIZE: usize = rtflow_media::TS_PACKET_SIZE;
const TS_SYNC_BYTE: u8 = 0x47;
const PTS_MODULUS: u64 = 1 << 33;
/// Per-PID cap for PES accumulation (a PES packet larger than this is
/// dropped instead of buffered; see `handle_pes`). Aligned with the RTP
/// access-unit assembly cap so 8K IDR frames (which can reach several MB)
/// are never misclassified as runaway buffers.
const MAX_PES_BUFFER_BYTES: usize = 16 * 1024 * 1024;

/// Streaming MPEG-TS demuxer.
pub struct TsStreamDemuxer {
    /// Accumulated TS data not yet processed.
    buffer: BytesMut,
    /// Discovered tracks (pid -> track info).
    tracks: HashMap<u16, TrackInfo>,
    /// PES assembly buffers (pid -> data).
    pes_buffers: HashMap<u16, BytesMut>,
    /// Track ID assignment counter.
    next_track_id: u8,
    /// Origin PTS for timestamp normalization.
    origin_pts: Option<u64>,
    /// Whether we've detected the program map.
    pmt_pid: Option<u16>,
    /// Completed access units ready for output.
    output: Vec<AccessUnit>,
}

struct TrackInfo {
    track_id: u8,
    codec: Codec,
    clock_rate: u32,
    channels: Option<u8>,
    last_pts: Option<u64>,
    last_dts: Option<u64>,
    /// Codec parameters are known (AAC learns sample rate/channels from its
    /// first ADTS frame, which may arrive after the video keyframe).
    configured: bool,
}

impl TsStreamDemuxer {
    pub fn new() -> Self {
        Self {
            buffer: BytesMut::with_capacity(TS_PACKET_SIZE * 100),
            tracks: HashMap::new(),
            pes_buffers: HashMap::new(),
            next_track_id: 0,
            origin_pts: None,
            pmt_pid: None,
            output: Vec::new(),
        }
    }

    /// Feed raw data into the demuxer.
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
        self.process_buffer();
    }

    /// Take completed access units.
    pub fn take_units(&mut self) -> Vec<AccessUnit> {
        std::mem::take(&mut self.output)
    }

    /// Get the media description based on discovered tracks.
    pub fn description(&self) -> Option<MediaDescription> {
        if self.tracks.is_empty() || self.tracks.values().any(|track| !track.configured) {
            return None;
        }

        let mut tracks: Vec<_> = self.tracks.values().collect();
        tracks.sort_by_key(|t| t.track_id);

        let track_descriptions: Vec<TrackDescription> = tracks
            .iter()
            .map(|t| TrackDescription {
                id: t.track_id,
                kind: match t.codec {
                    Codec::H264 | Codec::H265 | Codec::Vp8 | Codec::Vp9 => MediaKind::Video,
                    _ => MediaKind::Audio,
                },
                codec: t.codec,
                clock_rate: t.clock_rate,
                channels: t.channels,
                payload_type: 96 + t.track_id,
                fmtp: None,
                control: format!("trackID={}", t.track_id).into(),
                width: None,
                height: None,
                frame_rate: None,
                crypto: None,
                direction: None,
                ptime: None,
                rtcp: None,
                bandwidth_as: None,
                rtx: None,
            })
            .collect();

        Some(MediaDescription {
            session_name: "srt-stream".into(),
            tracks: track_descriptions,
            connection: None,
        })
    }

    fn process_buffer(&mut self) {
        // Find sync byte alignment
        while !self.buffer.is_empty() && self.buffer[0] != TS_SYNC_BYTE {
            let _ = self.buffer.split_to(1);
        }

        // Process complete TS packets
        while self.buffer.len() >= TS_PACKET_SIZE {
            if self.buffer[0] != TS_SYNC_BYTE {
                // Lost sync, try to recover
                let _ = self.buffer.split_to(1);
                continue;
            }

            let packet = self.buffer.split_to(TS_PACKET_SIZE);
            self.process_ts_packet(&packet);
        }
    }

    fn process_ts_packet(&mut self, packet: &[u8]) {
        let pid = (((packet[1] & 0x1F) as u16) << 8) | (packet[2] as u16);
        let payload_unit_start = (packet[1] & 0x40) != 0;
        let adaptation_field_control = (packet[3] >> 4) & 0x03;

        // Calculate payload offset
        let mut offset = 4;
        if adaptation_field_control == 0x02 || adaptation_field_control == 0x03 {
            let adaptation_length = packet[4] as usize;
            offset += 1 + adaptation_length;
        }

        if adaptation_field_control == 0x00 || adaptation_field_control == 0x02 {
            return; // No payload
        }

        if offset >= TS_PACKET_SIZE {
            return;
        }

        let payload = &packet[offset..];

        // Handle PAT (PID 0)
        if pid == 0 {
            self.parse_pat(payload, payload_unit_start);
            return;
        }

        // Handle PMT
        if Some(pid) == self.pmt_pid {
            self.parse_pmt(payload, payload_unit_start);
            return;
        }

        // Handle media PIDs
        if self.tracks.contains_key(&pid) {
            self.handle_pes(pid, payload, payload_unit_start);
        }
    }

    fn parse_pat(&mut self, payload: &[u8], unit_start: bool) {
        let data = if unit_start {
            // Skip pointer field
            let pointer = payload[0] as usize;
            &payload[1 + pointer..]
        } else {
            payload
        };

        if data.len() < 12 {
            return;
        }

        // PAT structure: table_id(8) | section_length(12) | transport_stream_id(16) | ...
        // Program entries start at offset 8, each 4 bytes: program_number(16) | PID(13)
        let section_length = (((data[1] & 0x0F) as usize) << 8) | (data[2] as usize);
        let entries_end = (3 + section_length - 4).min(data.len()); // -4 for CRC

        let mut pos = 8;
        while pos + 4 <= entries_end {
            let program_number = ((data[pos] as u16) << 8) | (data[pos + 1] as u16);
            let pmt_pid = (((data[pos + 2] & 0x1F) as u16) << 8) | (data[pos + 3] as u16);

            if program_number != 0 {
                // Not network PID
                self.pmt_pid = Some(pmt_pid);
                break;
            }
            pos += 4;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8], unit_start: bool) {
        let data = if unit_start {
            let pointer = payload[0] as usize;
            &payload[1 + pointer..]
        } else {
            payload
        };

        if data.len() < 12 {
            return;
        }

        let section_length = (((data[1] & 0x0F) as usize) << 8) | (data[2] as usize);
        let program_info_length = (((data[10] & 0x0F) as usize) << 8) | (data[11] as usize);

        let mut pos = 12 + program_info_length;
        let entries_end = (3 + section_length - 4).min(data.len());

        while pos + 5 <= entries_end {
            let stream_type = data[pos];
            let elementary_pid = (((data[pos + 1] & 0x1F) as u16) << 8) | (data[pos + 2] as u16);
            let es_info_length =
                (((data[pos + 3] & 0x0F) as usize) << 8) | (data[pos + 4] as usize);

            let codec = match stream_type {
                0x1B => Some(Codec::H264),
                0x24 => Some(Codec::H265),
                0x0F | 0x11 => Some(Codec::Aac),
                _ => None,
            };

            if let Some(codec) = codec
                && !self.tracks.contains_key(&elementary_pid)
            {
                let track_id = self.next_track_id;
                self.next_track_id += 1;

                let (clock_rate, channels) = match codec {
                    Codec::H264 | Codec::H265 => (90000, None),
                    Codec::Aac => (44100, Some(2)),
                    _ => (90000, None),
                };

                self.tracks.insert(
                    elementary_pid,
                    TrackInfo {
                        track_id,
                        codec,
                        clock_rate,
                        channels,
                        last_pts: None,
                        last_dts: None,
                        configured: !matches!(codec, Codec::Aac),
                    },
                );
            }

            pos += 5 + es_info_length;
        }
    }

    fn handle_pes(&mut self, pid: u16, payload: &[u8], unit_start: bool) {
        if unit_start {
            // Flush previous PES buffer
            if let Some(pes_data) = self.pes_buffers.remove(&pid) {
                self.process_pes_packet(pid, pes_data);
            }
            // One copy into the accumulating buffer; the final AccessUnit
            // is split out zero-copy in process_pes_packet.
            self.pes_buffers.insert(pid, BytesMut::from(payload));
        } else if let Some(buffer) = self.pes_buffers.get_mut(&pid) {
            // Bound the per-PID accumulation: a broken or malicious TS
            // sender that never sets payload_unit_start must not be able
            // to grow this buffer without limit (1 MB cap).
            if buffer.len().saturating_add(payload.len()) > MAX_PES_BUFFER_BYTES {
                tracing::warn!(pid, bytes = buffer.len(), "dropping oversized PES buffer");
                self.pes_buffers.remove(&pid);
                return;
            }
            buffer.extend_from_slice(payload);
        }
    }

    fn process_pes_packet(&mut self, pid: u16, mut data: BytesMut) {
        if data.len() < 9 {
            return;
        }

        // PES header: 0x000001 | stream_id(8) | PES_length(16) | ...
        if data[0] != 0 || data[1] != 0 || data[2] != 1 {
            return;
        }

        let pts_dts_flags = (data[7] >> 6) & 0x03;
        let pes_header_length = data[8] as usize;

        let mut pts: Option<u64> = None;
        let mut dts: Option<u64> = None;

        if pts_dts_flags >= 2 && data.len() >= 14 {
            pts = Some(parse_timestamp(&data[9..14]));
        }
        if pts_dts_flags == 3 && data.len() >= 19 {
            dts = Some(parse_timestamp(&data[14..19]));
        }

        let payload_start = 9 + pes_header_length;
        if payload_start >= data.len() {
            return;
        }

        let payload = &data[payload_start..];
        let Some(track) = self.tracks.get_mut(&pid) else {
            return;
        };

        // Normalize timestamps
        let pts_ticks = pts.unwrap_or(0);
        let dts_ticks = dts.unwrap_or(pts_ticks);

        if self.origin_pts.is_none() {
            self.origin_pts = Some(dts_ticks);
        }
        let origin = self.origin_pts.unwrap();

        // Relative to the first PES timestamp, but interpret the 33-bit
        // difference as signed: the first audio PES can legitimately precede
        // the first video PES (audio is interleaved slightly ahead). A plain
        // modular subtraction turns that into a timestamp near 2^33
        // (≈95443 s), which wrecks downstream segmenters; clamp "before the
        // origin" to zero instead.
        let normalized_dts = relative_ticks(dts_ticks, origin);
        let normalized_pts = relative_ticks(pts_ticks, origin);

        let dts_duration = rtflow_media::clock::ticks_to_duration(normalized_dts, 90_000);
        let pts_duration = rtflow_media::clock::ticks_to_duration(normalized_pts, 90_000);

        // AAC PES packets usually carry several ADTS frames; HLS/fMP4 and the
        // recording writer expect one raw AAC frame per AccessUnit. Emitting
        // the whole PES as one sample compresses ~9 frames into one 1024-tick
        // sample and breaks the audio timeline (non-monotonic DTS at segment
        // boundaries, audible gaps).
        if track.codec == Codec::Aac
            && let Some(frames) = split_adts(payload)
        {
            let (sample_rate, channels) = (frames.0, frames.1);
            if track.clock_rate != sample_rate {
                track.clock_rate = sample_rate;
                track.channels = Some(channels as u8);
            }
            track.configured = true;
            let mut last_pts = normalized_pts;
            for (index, raw) in frames.2.into_iter().enumerate() {
                let offset =
                    Duration::from_secs_f64(1024.0 * index as f64 / f64::from(sample_rate));
                let frame_pts = pts_duration + offset;
                let frame_dts = dts_duration + offset;
                last_pts = last_pts.saturating_add(1024);
                self.output.push(AccessUnit {
                    track_id: track.track_id,
                    codec: track.codec,
                    pts: frame_pts,
                    dts: Some(frame_dts),
                    rtp_timestamp: (normalized_dts % (u32::MAX as u64)) as u32,
                    is_keyframe: true,
                    data: bytes::Bytes::copy_from_slice(raw),
                });
            }
            track.last_pts = Some(last_pts);
            track.last_dts = Some(last_pts);
            return;
        }

        track.configured = true;
        track.last_pts = Some(normalized_pts);
        track.last_dts = Some(normalized_dts);

        // Determine if keyframe (for video)
        let is_keyframe = match track.codec {
            Codec::H264 => is_h264_keyframe(payload),
            Codec::H265 => is_h265_keyframe(payload),
            _ => true, // Audio is always "keyframe"
        };

        let unit = AccessUnit {
            track_id: track.track_id,
            codec: track.codec,
            pts: pts_duration,
            dts: Some(dts_duration),
            rtp_timestamp: (normalized_dts % (u32::MAX as u64)) as u32,
            is_keyframe,
            // Zero-copy: split the payload region out of the PES buffer
            // (the PES header prefix is discarded).
            data: data.split_off(payload_start).freeze(),
        };

        self.output.push(unit);
    }
}

impl Default for TsStreamDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse ADTS AAC frames from a PES payload.
///
/// Returns `(sample_rate, channels, raw_frames)` where each frame has its
/// ADTS header removed. `None` when no valid ADTS sync word is present.
fn split_adts(payload: &[u8]) -> Option<(u32, u16, Vec<&[u8]>)> {
    const SAMPLE_RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let mut frames = Vec::new();
    let mut offset = 0;
    let mut sample_rate = 0;
    let mut channels = 0;
    while offset + 7 <= payload.len() {
        if payload[offset] != 0xFF || payload[offset + 1] & 0xF0 != 0xF0 {
            break;
        }
        let protection_absent = payload[offset + 1] & 0x01;
        let header_len = if protection_absent == 1 { 7 } else { 9 };
        if offset + header_len > payload.len() {
            break;
        }
        let profile = (payload[offset + 2] >> 6) & 0x03;
        let rate_index = ((payload[offset + 2] >> 2) & 0x0F) as usize;
        let channel_config = ((payload[offset + 2] & 0x01) << 2) | (payload[offset + 3] >> 6);
        let frame_len = ((usize::from(payload[offset + 3] & 0x03)) << 11)
            | (usize::from(payload[offset + 4]) << 3)
            | (usize::from(payload[offset + 5]) >> 5);
        if rate_index >= SAMPLE_RATES.len()
            || frame_len < header_len
            || offset + frame_len > payload.len()
        {
            break;
        }
        if sample_rate == 0 {
            sample_rate = SAMPLE_RATES[rate_index];
            channels = u32::from(channel_config);
        }
        // Only AAC-LC (profile 1 in ADTS's 2-bit field) is supported by the
        // downstream fMP4/TS muxers here, matching the HLS pipeline.
        if profile == 1 {
            frames.push(&payload[offset + header_len..offset + frame_len]);
        }
        offset += frame_len;
    }
    if frames.is_empty() || sample_rate == 0 {
        return None;
    }
    Some((sample_rate, channels.max(1) as u16, frames))
}

fn relative_ticks(ticks: u64, origin: u64) -> u64 {
    const HALF: u64 = PTS_MODULUS / 2;
    let delta = (ticks + PTS_MODULUS - origin) % PTS_MODULUS;
    // A delta in the upper half is a (small) negative offset: the PES
    // precedes the origin. Clamp it to zero; the alternative is a timestamp
    // almost one full 33-bit wrap in the future.
    if delta > HALF { 0 } else { delta }
}

fn parse_timestamp(data: &[u8]) -> u64 {
    // PTS/DTS encoding: 4 bits marker | 3 bits TS[32..30] | 1 marker |
    //                   15 bits TS[29..15] | 1 marker | 15 bits TS[14..0] | 1 marker
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;

    ((b0 >> 1) & 0x07) << 30 | (b1 << 22) | ((b2 >> 1) << 15) | (b3 << 7) | (b4 >> 1)
}

fn is_h264_keyframe(data: &[u8]) -> bool {
    // Look for IDR NAL unit (type 5) or SPS (type 7). Scanning the whole
    // frame is wasteful: the start code and first NAL headers sit at the
    // front, so cap the scan window.
    let limit = data.len().min(2048).saturating_sub(4);
    for i in 0..limit {
        if data[i] == 0 && data[i + 1] == 0 {
            let start = if data[i + 2] == 1 {
                i + 3
            } else if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                i + 4
            } else {
                continue;
            };
            if start < data.len() {
                let nal_type = data[start] & 0x1F;
                // IDR slice (5), or an SPS (7) that precedes it.
                if nal_type == 5 || nal_type == 7 {
                    return true;
                }
            }
        }
    }
    false
}

fn is_h265_keyframe(data: &[u8]) -> bool {
    // Look for IDR/CRA/BLA NAL units (types 16-23). The first NALUs
    // (VPS/SPS/IDR) carry the answer, so only scan the frame head.
    let limit = data.len().min(2048).saturating_sub(4);
    for i in 0..limit {
        if data[i] == 0 && data[i + 1] == 0 {
            let start = if data[i + 2] == 1 {
                i + 3
            } else if data[i + 2] == 0 && i + 3 < data.len() && data[i + 3] == 1 {
                i + 4
            } else {
                continue;
            };
            if start < data.len() {
                // Random access point: IDR/CRA/BLA (16–21); reserved
                // types 22/23 are not keyframes.
                if rtflow_media::bitstream::is_h265_random_access(&data[start..]) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_parsing() {
        // PTS = 90000 (1 second at 90kHz)
        // Encoding: 0010 PTS[32..30] 1 PTS[29..15] 1 PTS[14..0] 1
        // 90000 = 0x15F90
        // PTS[32..30] = 0, PTS[29..15] = 2, PTS[14..0] = 0x5F90 = 24464
        // Byte 0: 0010 000 1 = 0x21
        // Byte 1-2: 0000 0000 0000 010 1 = 0x00 0x05 (PTS[29..15]=2, shifted)
        // Byte 3-4: 0111 1111 0010 000 1 = 0xBF 0x21 (PTS[14..0]=24464)
        // Actually let's compute properly:
        // PTS = 90000
        // byte0 = 0x20 | ((PTS >> 29) & 0x0E) | 0x01 = 0x21
        // byte1 = (PTS >> 22) & 0xFF = 0x00
        // byte2 = ((PTS >> 14) & 0xFE) | 0x01 = 0x05
        // byte3 = (PTS >> 7) & 0xFF = 0xBF
        // byte4 = ((PTS << 1) & 0xFE) | 0x01 = 0x21
        let data = [0x21, 0x00, 0x05, 0xBF, 0x21];
        let ts = parse_timestamp(&data);
        assert_eq!(ts, 90000);
    }

    #[test]
    fn aac_pes_is_split_into_raw_frames() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/h264-aac.ts");
        let data = std::fs::read(path).expect("fixture");
        let mut demuxer = TsStreamDemuxer::new();
        for chunk in data.chunks(188) {
            demuxer.push(chunk);
        }
        let units = demuxer.take_units();
        let audio: Vec<_> = units
            .iter()
            .filter(|unit| unit.codec == Codec::Aac)
            .collect();
        assert!(
            audio.len() >= 8,
            "expected split AAC frames, got {}",
            audio.len()
        );
        for unit in &audio {
            assert_ne!(unit.data[0], 0xFF, "ADTS header must be stripped");
        }
        for pair in audio.windows(2) {
            assert!(pair[1].pts > pair[0].pts, "AAC timestamps must advance");
        }
        let description = demuxer.description().expect("description");
        let track = description
            .tracks
            .iter()
            .find(|track| track.codec == Codec::Aac)
            .expect("audio track");
        assert_eq!(track.clock_rate, 48_000);
    }

    #[test]
    fn timestamps_before_origin_are_clamped() {
        assert_eq!(relative_ticks(126_000, 127_920), 0);
        assert_eq!(relative_ticks(127_920, 127_920), 0);
        assert_eq!(relative_ticks(129_840, 127_920), 1_920);
    }

    #[test]
    fn demuxer_creation() {
        let mut demuxer = TsStreamDemuxer::new();
        assert!(demuxer.description().is_none());
        assert!(demuxer.take_units().is_empty());
    }
}
