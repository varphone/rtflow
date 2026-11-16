mod audio;
mod h264;
mod h265;
mod vp;

use crate::RtpPacket;
use bytes::{Bytes, BytesMut};
use rtflow_core::error::{Error, Result};
use rtflow_media::{AccessUnit, Codec, TrackId};

/// Upper bound for a reassembled access unit (FU fragments + multi-NAL
/// aggregation). A malicious or broken peer must not be able to grow
/// per-connection buffers without limit; 16 MB covers 8K HEVC frames with
/// headroom while bounding each session's memory.
const MAX_ASSEMBLY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct Depacketizer {
    codec: Codec,
    track_id: TrackId,
    clock_rate: u32,
    assembling: Option<BytesMut>,
    /// Keyframe flag of the NAL currently being reassembled from
    /// fragmentation units (H.264 FU-A / H.265 FU). The FU start packet
    /// carries the NAL type; continuation packets do not, so the flag is
    /// latched here until the FU end packet completes the NAL.
    assembling_is_keyframe: bool,
    /// VP8/VP9 frame reassembly (RFC 7741 / draft-ietf-payload-vp9): fragments share one timestamp,
    /// the S bit of the payload descriptor starts a new frame.
    vp_assembling: Option<BytesMut>,
    /// Keyframe flag of the VP8/VP9 frame being assembled (VP8 frame header
    /// P bit, only present on the fragment with S=1).
    vp_is_keyframe: bool,
    access_unit: Vec<Bytes>,
    /// Bytes accumulated in `access_unit` (avoids O(n) re-scans on every
    /// packet for the assembly limit check).
    access_unit_bytes: usize,
    current_timestamp: Option<u32>,
    expected_sequence: Option<u16>,
    damaged_timestamp: Option<u32>,
    /// Keyframe flag accumulated while assembling the current AU (avoids a
    /// second full-frame Annex-B scan after assembly).
    assembled_is_keyframe: bool,
    /// Last raw RTP timestamp, for wrap-extended PTS.
    last_rtp_ts: u32,
    /// Wrap-free RTP timestamp clock (units of `clock_rate` ticks).
    extended_ts: i128,
    /// Whether the first RTP timestamp has been anchored.
    timestamp_initialized: bool,
    av1: super::av1::Av1State,
    /// NPT base added to the RTP-timestamp-derived PTS.
    pts_base: std::time::Duration,
}
impl Depacketizer {
    pub fn new(codec: Codec, track_id: TrackId, clock_rate: u32) -> Self {
        Self {
            codec,
            track_id,
            clock_rate,
            assembling: None,
            assembling_is_keyframe: false,
            vp_assembling: None,
            vp_is_keyframe: false,
            access_unit: Vec::new(),
            access_unit_bytes: 0,
            current_timestamp: None,
            expected_sequence: None,
            damaged_timestamp: None,
            assembled_is_keyframe: false,
            last_rtp_ts: 0,
            extended_ts: 0,
            timestamp_initialized: false,
            av1: super::av1::Av1State::default(),
            pts_base: std::time::Duration::ZERO,
        }
    }

    /// PTS from the RTP timestamp, extended across the 32-bit wrap:
    /// without extension the PTS resets to zero every ~13.1h at 90kHz.
    /// The delta is interpreted as signed so out-of-order timestamps and
    /// wrap-arounds both extend correctly.
    /// Anchor the RTP timestamp clock and PTS to the PLAY `RTP-Info`
    /// `rtptime`/`Range` response, so the first access unit starts at the
    /// requested NPT position rather than at an arbitrary RTP tick.
    pub fn set_time_base(&mut self, rtp_base: u32, pts_base: std::time::Duration) {
        self.last_rtp_ts = rtp_base;
        self.extended_ts = 0;
        self.timestamp_initialized = true;
        self.pts_base = pts_base;
    }

    fn pts_from_timestamp(&mut self, rtp_ts: u32) -> std::time::Duration {
        if self.clock_rate == 0 {
            return std::time::Duration::ZERO;
        }
        if !self.timestamp_initialized {
            // Preserve the old relative-to-zero behaviour for normal RTP
            // timestamps (tests/servers starting near zero). Random camera
            // timestamps are usually above i32::MAX; measuring them from zero
            // produced a negative delta and a saturated, constant PTS.
            self.last_rtp_ts = rtp_ts;
            self.extended_ts = if rtp_ts <= i32::MAX as u32 {
                i128::from(rtp_ts)
            } else {
                0
            };
            self.timestamp_initialized = true;
        } else {
            let raw_delta = rtp_ts.wrapping_sub(self.last_rtp_ts) as i32 as i64;
            self.extended_ts = self.extended_ts.saturating_add(raw_delta as i128);
            self.last_rtp_ts = rtp_ts;
        }
        let micros =
            self.extended_ts.saturating_mul(1_000_000) / i128::from(self.clock_rate).max(1);
        self.pts_base
            + std::time::Duration::from_micros(u64::try_from(micros.max(0)).unwrap_or(u64::MAX))
    }
    pub fn push(&mut self, p: RtpPacket) -> Result<Vec<AccessUnit>> {
        let timestamp = p.header.timestamp;
        let marker = p.header.marker;
        if self
            .expected_sequence
            .is_some_and(|e| e != p.header.sequence)
            && matches!(
                self.codec,
                Codec::H264
                    | Codec::H265
                    | Codec::Vp8
                    | Codec::Vp9
                    | Codec::Av1
                    | Codec::Jpeg
                    | Codec::H263
                    | Codec::Mpeg4
            )
        {
            let d = self.current_timestamp == Some(timestamp);
            self.assembling = None;
            self.assembling_is_keyframe = false;
            // VP8 fragments share one timestamp; a gap must discard the
            // in-flight frame too, otherwise fragments of two different
            // frames get spliced into one corrupted frame.
            self.vp_assembling = None;
            self.vp_is_keyframe = false;
            self.access_unit.clear();
            self.assembled_is_keyframe = false;
            self.current_timestamp = None;
            self.damaged_timestamp = d.then_some(timestamp);
        }
        self.expected_sequence = Some(p.header.sequence.wrapping_add(1));
        if self.damaged_timestamp.is_some_and(|d| d != timestamp) {
            self.damaged_timestamp = None;
        }
        if matches!(self.codec, Codec::Vp8 | Codec::Vp9) {
            return self.vp9_depacketize(&p);
        }
        if self.codec == Codec::Av1 {
            return self.av1_depacketize(p);
        }
        if matches!(self.codec, Codec::Jpeg | Codec::H263 | Codec::Mpeg4) {
            return self.opaque_video(p);
        }
        if matches!(self.codec, Codec::H264 | Codec::H265) {
            if self.current_timestamp.is_some_and(|v| v != timestamp) {
                self.access_unit.clear();
                self.access_unit_bytes = 0;
                self.assembled_is_keyframe = false;
                self.assembling = None;
                self.assembling_is_keyframe = false;
            }
            self.current_timestamp = Some(timestamp);
            let nal = match self.codec {
                Codec::H264 => self.h264(p.payload)?,
                Codec::H265 => self.h265(p.payload)?,
                other => {
                    return Err(Error::Media(format!(
                        "depacketization not implemented for {other:?}"
                    )));
                }
            };
            if let Some((nal, keyframe_nal)) = nal {
                if self.access_unit_bytes.saturating_add(nal.len()) > MAX_ASSEMBLY_BYTES {
                    self.access_unit.clear();
                    self.access_unit_bytes = 0;
                    self.assembled_is_keyframe = false;
                    self.assembling = None;
                    self.assembling_is_keyframe = false;
                    self.damaged_timestamp = Some(timestamp);
                    return Err(Error::Media(format!(
                        "access unit exceeds {MAX_ASSEMBLY_BYTES} bytes"
                    )));
                }
                // Keyframe classification happens inside h264/h265 while the
                // NAL headers are already being parsed — no second scan.
                self.assembled_is_keyframe |= keyframe_nal;
                self.access_unit_bytes += nal.len();
                self.access_unit.push(nal);
            }
            if !marker || self.assembling.is_some() || self.access_unit.is_empty() {
                return Ok(vec![]);
            }
            if self.damaged_timestamp == Some(timestamp) {
                self.access_unit.clear();
                self.access_unit_bytes = 0;
                self.assembled_is_keyframe = false;
                self.current_timestamp = None;
                self.damaged_timestamp = None;
                return Ok(vec![]);
            }
            let data = {
                // Pre-size the output: NAL lengths are known, so assemble
                // with a single allocation instead of geometric growth.
                let total: usize = self
                    .access_unit
                    .iter()
                    .map(|nal| nal.len().saturating_add(4))
                    .sum();
                let mut output = BytesMut::with_capacity(total);
                for nal in self.access_unit.drain(..) {
                    output.extend_from_slice(&[0, 0, 0, 1]);
                    output.extend_from_slice(&nal);
                }
                self.access_unit_bytes = 0;
                output.freeze()
            };
            let is_keyframe = self.assembled_is_keyframe;
            self.assembled_is_keyframe = false;
            self.current_timestamp = None;
            let pts = self.pts_from_timestamp(timestamp);
            return Ok(vec![AccessUnit {
                track_id: self.track_id,
                codec: self.codec,
                pts,
                dts: None,
                rtp_timestamp: timestamp,
                is_keyframe,
                data,
            }]);
        }
        self.audio(p)
    }

    fn av1_depacketize(&mut self, packet: RtpPacket) -> Result<Vec<AccessUnit>> {
        let timestamp = packet.header.timestamp;
        let Some((data, keyframe)) = self
            .av1
            .depacketize(&packet.payload, packet.header.marker)?
        else {
            return Ok(Vec::new());
        };
        Ok(vec![AccessUnit {
            track_id: self.track_id,
            codec: Codec::Av1,
            pts: self.pts_from_timestamp(timestamp),
            dts: None,
            rtp_timestamp: timestamp,
            is_keyframe: keyframe,
            data,
        }])
    }

    fn opaque_video(&mut self, packet: RtpPacket) -> Result<Vec<AccessUnit>> {
        let timestamp = packet.header.timestamp;
        if self
            .current_timestamp
            .is_some_and(|value| value != timestamp)
        {
            self.access_unit.clear();
            self.access_unit_bytes = 0;
            self.assembled_is_keyframe = false;
        }
        self.current_timestamp = Some(timestamp);
        let payload = packet.payload;
        let (data, keyframe) = match self.codec {
            Codec::Av1 => {
                if payload.is_empty() {
                    return Err(Error::Media("short AV1 payload".into()));
                }
                (payload.slice(1..), payload[0] & 0x40 != 0)
            }
            Codec::Jpeg => {
                if payload.len() < 8 {
                    return Err(Error::Media("short JPEG payload header".into()));
                }
                (payload.slice(8..), true)
            }
            Codec::H263 => {
                if payload.len() < 2 {
                    return Err(Error::Media("short H263 payload header".into()));
                }
                let header = u16::from_be_bytes([payload[0], payload[1]]);
                let startcode = header & 0x0400 != 0;
                let mut offset = 2usize;
                if header & 0x0200 != 0 {
                    offset += 1; // VRC byte
                }
                offset += ((header & 0x01f8) >> 3) as usize;
                if offset > payload.len() {
                    return Err(Error::Media("short H263 payload header".into()));
                }
                let data = payload.slice(offset..);
                if startcode {
                    let mut value = bytes::BytesMut::with_capacity(data.len() + 2);
                    value.extend_from_slice(&[0, 0]);
                    value.extend_from_slice(&data);
                    (value.freeze(), true)
                } else {
                    (data, true)
                }
            }
            Codec::Mpeg4 => {
                let keyframe =
                    payload.len() >= 5 && payload[..4] == [0, 0, 1, 0xb6] && payload[4] >> 6 == 0;
                (payload, keyframe)
            }
            _ => return Err(Error::Media("not an opaque video codec".into())),
        };
        if self.access_unit_bytes.saturating_add(data.len()) > MAX_ASSEMBLY_BYTES {
            self.access_unit.clear();
            self.access_unit_bytes = 0;
            self.assembled_is_keyframe = false;
            return Err(Error::Media(format!(
                "access unit exceeds {MAX_ASSEMBLY_BYTES} bytes"
            )));
        }
        self.assembled_is_keyframe |= keyframe;
        self.access_unit_bytes += data.len();
        self.access_unit.push(data);
        if !packet.header.marker {
            return Ok(Vec::new());
        }
        let mut output = BytesMut::with_capacity(self.access_unit_bytes);
        for chunk in self.access_unit.drain(..) {
            output.extend_from_slice(&chunk);
        }
        let is_keyframe = if self.codec == Codec::Mpeg4 {
            mpeg4_is_keyframe(&output)
        } else {
            self.assembled_is_keyframe
        };
        self.access_unit_bytes = 0;
        self.assembled_is_keyframe = false;
        self.current_timestamp = None;
        Ok(vec![AccessUnit {
            track_id: self.track_id,
            codec: self.codec,
            pts: self.pts_from_timestamp(timestamp),
            dts: None,
            rtp_timestamp: timestamp,
            is_keyframe,
            data: output.freeze(),
        }])
    }
}

/// True when the MPEG-4 Visual access unit contains an I-VOP start code.
fn mpeg4_is_keyframe(data: &[u8]) -> bool {
    let mut index = 0;
    while index + 5 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1, 0xb6]) {
            return data[index + 4] >> 6 == 0;
        }
        index += 1;
    }
    false
}
