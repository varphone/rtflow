//! RFC 6184, RFC 7798, RFC 3640 and G.711 payload packetization.
use super::{RtpHeader, RtpPacket};
use crate::{
    Error, Result,
    media::{AccessUnit, Codec, TrackId},
};
use bytes::{BufMut, Bytes, BytesMut};

#[derive(Clone, Debug)]
pub struct Packetizer {
    pub codec: Codec,
    pub payload_type: u8,
    pub ssrc: u32,
    pub next_sequence: u16,
    pub mtu: usize,
    /// Per-frame picture ID for the VP9 payload descriptor
    /// (draft-ietf-payload-vp9), incremented and wrapped per frame.
    vp9_picture_id: u16,
}
impl Packetizer {
    pub fn new(codec: Codec, payload_type: u8, ssrc: u32, mtu: usize) -> Result<Self> {
        if payload_type > 127 {
            return Err(Error::Media("RTP payload type must be <= 127".into()));
        }
        if mtu <= 16 {
            return Err(Error::Media(
                "MTU must exceed RTP and fragment headers".into(),
            ));
        }
        Ok(Self {
            codec,
            payload_type,
            ssrc,
            next_sequence: 0,
            mtu,
            vp9_picture_id: 0,
        })
    }
    fn packet(&mut self, payload: Bytes, timestamp: u32, marker: bool) -> RtpPacket {
        let p = RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: self.payload_type,
                sequence: self.next_sequence,
                timestamp,
                ssrc: self.ssrc,
            },
            payload,
        };
        self.next_sequence = self.next_sequence.wrapping_add(1);
        p
    }
    pub fn packetize(&mut self, frame: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if frame.codec != self.codec {
            return Err(Error::Media("frame codec does not match packetizer".into()));
        }
        match self.codec {
            Codec::H264 => self.h264(frame),
            Codec::H265 => self.h265(frame),
            Codec::Aac => self.aac(frame),
            Codec::Pcma | Codec::Pcmu | Codec::Opus => Ok(vec![self.packet(
                frame.data.clone(),
                frame.rtp_timestamp,
                true,
            )]),
            Codec::Vp8 => self.vp8(frame),
            Codec::Vp9 => self.vp9(frame),
        }
    }
    /// VP9 payload descriptor (draft-ietf-payload-vp9, the format used by
    /// WebRTC and ffmpeg): octet 0 = `I P L F B E V Z`, with I=1 followed
    /// by a 16-bit picture ID. B marks the first fragment of a frame, E
    /// the last; the picture ID increments per frame.
    fn vp9(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if f.data.is_empty() {
            return Err(Error::Media("empty VP9 access unit".into()));
        }
        let max = self.mtu.saturating_sub(12);
        let fragment_size = max.saturating_sub(3); // descriptor + picture ID
        if fragment_size == 0 {
            return Err(Error::Media("MTU too small for VP9".into()));
        }
        let total = f.data.len().div_ceil(fragment_size);
        let picture_id = self.vp9_picture_id;
        self.vp9_picture_id = self.vp9_picture_id.wrapping_add(1);
        let mut out = Vec::with_capacity(total);
        for (index, chunk) in f.data.chunks(fragment_size).enumerate() {
            let mut payload = BytesMut::with_capacity(3 + chunk.len());
            let first = index == 0;
            let last = index + 1 == total;
            // I=1 (picture ID present), M=1 (15-bit picture ID), B/E flags.
            // Always using the 15-bit form keeps the descriptor 3 bytes
            // regardless of the picture-ID value, which every conformant
            // decoder (libwebrtc, ffmpeg) accepts.
            payload.put_u8(0x80 | ((first as u8) << 2) | ((last as u8) << 1));
            payload.put_u8(0x80 | ((picture_id >> 8) & 0x7f) as u8);
            payload.put_u8((picture_id & 0xff) as u8);
            payload.extend_from_slice(chunk);
            out.push(self.packet(payload.freeze(), f.rtp_timestamp, last));
        }
        Ok(out)
    }
    /// RFC 7741 VP8 packetization: 1-byte payload descriptor
    /// (X=0 R=0 N=0, S on the first fragment, PID increments per
    /// fragment, wrapping at 16). Frames larger than the MTU are
    /// fragmented; marker is set on the last fragment.
    fn vp8(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if f.data.is_empty() {
            return Err(Error::Media("empty VP8 access unit".into()));
        }
        let max = self.mtu.saturating_sub(12);
        let fragment_size = max.saturating_sub(1); // payload descriptor
        if fragment_size == 0 {
            return Err(Error::Media("MTU too small for VP8".into()));
        }
        let total = f.data.len().div_ceil(fragment_size);
        let mut out = Vec::with_capacity(total);
        for (index, chunk) in f.data.chunks(fragment_size).enumerate() {
            let mut payload = BytesMut::with_capacity(1 + chunk.len());
            let start = index == 0;
            payload.put_u8(((start as u8) << 4) | (index & 0x0f) as u8);
            payload.extend_from_slice(chunk);
            out.push(self.packet(payload.freeze(), f.rtp_timestamp, index + 1 == total));
        }
        Ok(out)
    }
    fn h264(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if f.data.is_empty() {
            return Err(Error::Media("empty H264 access unit".into()));
        }
        let max = self.mtu.saturating_sub(12);
        let nals = annex_b_nals(&f.data);
        let nals = if nals.is_empty() {
            vec![f.data.clone()]
        } else {
            nals
        };
        let total = nals.len();
        let mut out = Vec::new();
        for (i, nal) in nals.into_iter().enumerate() {
            if nal.len() <= max {
                out.push(self.packet(nal, f.rtp_timestamp, i + 1 == total));
                continue;
            }
            if nal.len() < 2 {
                return Err(Error::Media("short H264 NAL".into()));
            }
            let hdr = nal[0];
            let size = max - 2;
            let mut pos = 1;
            while pos < nal.len() {
                let end = (pos + size).min(nal.len());
                let mut v = BytesMut::with_capacity(2 + end - pos);
                v.extend_from_slice(&[
                    (hdr & 0xe0) | 28,
                    ((pos == 1) as u8) << 7 | ((end == nal.len()) as u8) << 6 | (hdr & 0x1f),
                ]);
                v.extend_from_slice(&nal[pos..end]);
                out.push(self.packet(
                    v.freeze(),
                    f.rtp_timestamp,
                    end == nal.len() && i + 1 == total,
                ));
                pos = end;
            }
        }
        Ok(out)
    }
    fn h265(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if f.data.len() < 2 {
            return Err(Error::Media("short H265 access unit".into()));
        }
        let max = self.mtu.saturating_sub(12);
        let nals = annex_b_nals(&f.data);
        let nals = if nals.is_empty() {
            vec![f.data.clone()]
        } else {
            nals
        };
        let total = nals.len();
        let mut out = Vec::new();
        for (index, nal) in nals.into_iter().enumerate() {
            if nal.len() < 2 {
                return Err(Error::Media("short H265 NAL".into()));
            }
            if nal.len() <= max {
                out.push(self.packet(nal, f.rtp_timestamp, index + 1 == total));
                continue;
            }
            let h0 = nal[0];
            let h1 = nal[1];
            let size = max - 3;
            let mut pos = 2;
            while pos < nal.len() {
                let end = (pos + size).min(nal.len());
                let mut v = BytesMut::with_capacity(3 + end - pos);
                v.extend_from_slice(&[
                    (h0 & 0x81) | (49 << 1),
                    h1,
                    ((pos == 2) as u8) << 7 | ((end == nal.len()) as u8) << 6 | ((h0 >> 1) & 0x3f),
                ]);
                v.extend_from_slice(&nal[pos..end]);
                out.push(self.packet(
                    v.freeze(),
                    f.rtp_timestamp,
                    end == nal.len() && index + 1 == total,
                ));
                pos = end;
            }
        }
        Ok(out)
    }
    fn aac(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        let size_bytes = f.data.len();
        if size_bytes > 0x1fff {
            return Err(Error::Media(
                "AAC AU exceeds the 13-bit AU-size field".into(),
            ));
        }
        if f.data.len() + 4 > self.mtu.saturating_sub(12) {
            return Err(Error::Media(
                "AAC AU exceeds MTU; fragmentation is unsupported".into(),
            ));
        }
        let au_header = (size_bytes as u16) << 3;
        let mut v = BytesMut::with_capacity(f.data.len() + 4);
        v.extend_from_slice(&16u16.to_be_bytes());
        v.extend_from_slice(&au_header.to_be_bytes());
        v.extend_from_slice(&f.data);
        Ok(vec![self.packet(v.freeze(), f.rtp_timestamp, true)])
    }
}
fn annex_b_nals(data: &Bytes) -> Vec<Bytes> {
    let mut marks = Vec::new();
    let b = &data[..];
    let mut i = 0;
    while i + 3 <= b.len() {
        let l = if b[i..].starts_with(&[0, 0, 1]) {
            3
        } else if i + 4 <= b.len() && b[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else {
            i += 1;
            continue;
        };
        marks.push((i, l));
        i += l;
    }
    let mut n = Vec::new();
    for ix in 0..marks.len() {
        let start = marks[ix].0 + marks[ix].1;
        let end = marks.get(ix + 1).map(|x| x.0).unwrap_or(b.len());
        if start < end {
            n.push(data.slice(start..end))
        }
    }
    n
}

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
    extended_ts: u64,
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
        }
    }

    /// PTS from the RTP timestamp, extended across the 32-bit wrap:
    /// without extension the PTS resets to zero every ~13.1h at 90kHz.
    /// The delta is interpreted as signed so out-of-order timestamps and
    /// wrap-arounds both extend correctly.
    fn pts_from_timestamp(&mut self, rtp_ts: u32) -> std::time::Duration {
        if self.clock_rate == 0 {
            return std::time::Duration::ZERO;
        }
        let raw_delta = rtp_ts.wrapping_sub(self.last_rtp_ts) as i32 as i64;
        self.extended_ts = self.extended_ts.wrapping_add(raw_delta as u64);
        self.last_rtp_ts = rtp_ts;
        std::time::Duration::from_micros(
            self.extended_ts.saturating_mul(1_000_000) / u64::from(self.clock_rate),
        )
    }
    pub fn push(&mut self, p: RtpPacket) -> Result<Vec<AccessUnit>> {
        let timestamp = p.header.timestamp;
        let marker = p.header.marker;
        if self
            .expected_sequence
            .is_some_and(|e| e != p.header.sequence)
            && matches!(self.codec, Codec::H264 | Codec::H265 | Codec::Vp8 | Codec::Vp9)
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
        match self.codec {
            Codec::Aac => {
                if p.payload.len() < 4 {
                    return Err(Error::Media("short AAC AU header".into()));
                }
                let header_bits = u16::from_be_bytes([p.payload[0], p.payload[1]]) as usize;
                if !header_bits.is_multiple_of(16) || header_bits == 0 {
                    return Err(Error::Media("invalid AAC AU-headers-length".into()));
                }
                let au_count = header_bits / 16;
                let header_end = 2 + au_count * 2;
                if p.payload.len() < header_end {
                    return Err(Error::Media("AAC AU headers exceed payload".into()));
                }
                let mut units = Vec::with_capacity(au_count);
                let mut data_offset = header_end;
                for i in 0..au_count {
                    let ah = u16::from_be_bytes([p.payload[2 + i * 2], p.payload[2 + i * 2 + 1]]);
                    let size = (ah >> 3) as usize;
                    if data_offset + size > p.payload.len() {
                        return Err(Error::Media("AAC AU-size exceeds payload".into()));
                    }
                    let pts = self.pts_from_timestamp(timestamp);
                    units.push(AccessUnit {
                        track_id: self.track_id,
                        codec: self.codec,
                        pts,
                        dts: None,
                        rtp_timestamp: timestamp,
                        is_keyframe: true,
                        data: p.payload.slice(data_offset..data_offset + size),
                    });
                    data_offset += size;
                }
                Ok(units)
            }
            Codec::Opus | Codec::Pcma | Codec::Pcmu => {
                let pts = self.pts_from_timestamp(timestamp);
                Ok(vec![AccessUnit {
                    track_id: self.track_id,
                    codec: self.codec,
                    pts,
                    dts: None,
                    rtp_timestamp: timestamp,
                    is_keyframe: true,
                    data: p.payload,
                }])
            }
            // Never panic on an unhandled codec: a future SDP/feature
            // extension must fail this session cleanly instead of killing
            // the tokio task (and with it the whole stream).
            other => Err(Error::Media(format!(
                "depacketization not implemented for {other:?}"
            ))),
        }
    }
    fn h264(&mut self, p: Bytes) -> Result<Option<(Bytes, bool)>> {
        if p.is_empty() {
            return Err(Error::Media("empty H264 RTP payload".into()));
        }
        let typ = p[0] & 0x1f;
        if typ == 24 {
            let mut offset = 1;
            let mut output = BytesMut::new();
            let mut keyframe = false;
            while offset + 2 <= p.len() {
                let size = u16::from_be_bytes([p[offset], p[offset + 1]]) as usize;
                offset += 2;
                if size == 0 || offset + size > p.len() {
                    return Err(Error::Media("invalid H264 STAP-A".into()));
                }
                keyframe |= size > 0 && (p[offset] & 0x1F) == 5;
                output.extend_from_slice(&[0, 0, 0, 1]);
                output.extend_from_slice(&p[offset..offset + size]);
                offset += size;
            }
            if offset != p.len() {
                return Err(Error::Media("trailing H264 STAP-A bytes".into()));
            }
            return Ok(Some((output.freeze(), keyframe)));
        }
        if typ != 28 {
            // Single NAL: its header byte is the classification.
            let keyframe = (p[0] & 0x1F) == 5;
            return Ok(Some((p, keyframe)));
        }
        if p.len() < 2 {
            return Err(Error::Media("short H264 FU-A".into()));
        }
        let start = p[1] & 0x80 != 0;
        let end = p[1] & 0x40 != 0;
        if start {
            let mut b = BytesMut::with_capacity(p.len());
            b.extend_from_slice(&[(p[0] & 0xe0) | (p[1] & 0x1f)]);
            b.extend_from_slice(&p[2..]);
            let keyframe = (p[1] & 0x1f) == 5;
            if end {
                return Ok(Some((b.freeze(), keyframe)));
            }
            // Latch the keyframe flag: FU continuation packets do not
            // carry the NAL type, so the flag must survive until the
            // FU end packet completes the NAL.
            self.assembling_is_keyframe = keyframe;
            self.assembling = Some(b);
            return Ok(None);
        }
        let b = self
            .assembling
            .as_mut()
            .ok_or_else(|| Error::Media("H264 FU continuation without start".into()))?;
        if b.len().saturating_add(p.len() - 2) > MAX_ASSEMBLY_BYTES {
            self.assembling = None;
            self.assembling_is_keyframe = false;
            return Err(Error::Media(format!(
                "H264 FU assembly exceeds {MAX_ASSEMBLY_BYTES} bytes"
            )));
        }
        b.extend_from_slice(&p[2..]);
        if end {
            let keyframe = std::mem::take(&mut self.assembling_is_keyframe);
            Ok(self.assembling.take().map(|b| (b.freeze(), keyframe)))
        } else {
            Ok(None)
        }
    }
    fn h265(&mut self, p: Bytes) -> Result<Option<(Bytes, bool)>> {
        if p.len() < 2 {
            return Err(Error::Media("short H265 RTP payload".into()));
        }
        let typ = (p[0] >> 1) & 0x3f;
        if typ == 48 {
            let mut offset = 2;
            let mut output = BytesMut::new();
            let mut keyframe = false;
            while offset + 2 <= p.len() {
                let size = u16::from_be_bytes([p[offset], p[offset + 1]]) as usize;
                offset += 2;
                if size < 2 || offset + size > p.len() {
                    return Err(Error::Media("invalid H265 AP".into()));
                }
                keyframe |= size >= 2 && crate::media::bitstream::is_h265_random_access(&p[offset..offset + size]);
                output.extend_from_slice(&[0, 0, 0, 1]);
                output.extend_from_slice(&p[offset..offset + size]);
                offset += size;
            }
            if offset != p.len() {
                return Err(Error::Media("trailing H265 AP bytes".into()));
            }
            return Ok(Some((output.freeze(), keyframe)));
        }
        if typ != 49 {
            let keyframe = crate::media::bitstream::is_h265_random_access(&p[..2.min(p.len())]);
            return Ok(Some((p, keyframe)));
        }
        if p.len() < 3 {
            return Err(Error::Media("short H265 FU".into()));
        }
        let start = p[2] & 0x80 != 0;
        let end = p[2] & 0x40 != 0;
        if start {
            let mut b = BytesMut::with_capacity(p.len());
            b.extend_from_slice(&[(p[0] & 0x81) | ((p[2] & 0x3f) << 1), p[1]]);
            b.extend_from_slice(&p[3..]);
            let keyframe = matches!(p[2] & 0x3f, 16..=21);
            if end {
                return Ok(Some((b.freeze(), keyframe)));
            }
            // Latch the keyframe flag (see H.264 FU-A handling).
            self.assembling_is_keyframe = keyframe;
            self.assembling = Some(b);
            return Ok(None);
        }
        let b = self
            .assembling
            .as_mut()
            .ok_or_else(|| Error::Media("H265 FU continuation without start".into()))?;
        if b.len().saturating_add(p.len() - 3) > MAX_ASSEMBLY_BYTES {
            self.assembling = None;
            self.assembling_is_keyframe = false;
            return Err(Error::Media(format!(
                "H265 FU assembly exceeds {MAX_ASSEMBLY_BYTES} bytes"
            )));
        }
        b.extend_from_slice(&p[3..]);
        if end {
            let keyframe = std::mem::take(&mut self.assembling_is_keyframe);
            Ok(self.assembling.take().map(|b| (b.freeze(), keyframe)))
        } else {
            Ok(None)
        }
    }

    /// VP8/VP9 depacketization: strips the payload descriptor and
    /// reassembles fragments (same timestamp, B/S bit starts a frame,
    /// E bit or marker ends it). Returns the frame directly — VP8/VP9
    /// frames are single access units, they never aggregate.
    ///
    /// - VP8 (RFC 7741): 1-byte descriptor `X R N S PID`; keyframe when
    ///   the VP8 frame-header P bit (byte 0 of the frame) is 0.
    /// - VP9 (draft-ietf-payload-vp9): octet 0 `I P L F B E V Z`, with
    ///   I=1 followed by a 16-bit picture ID; keyframe when the VP9
    ///   uncompressed-header frame_type bit (byte 0, bit 4) is 0.
    fn vp9_depacketize(&mut self, p: &RtpPacket) -> Result<Vec<AccessUnit>> {
        let payload = &p.payload;
        if payload.is_empty() {
            return Err(Error::Media("empty VP8/VP9 packet".into()));
        }
        struct VpFragment<'a> {
            start_bit: bool,
            end_bit: bool,
            data: &'a [u8],
            keyframe: fn(&[u8]) -> bool,
        }
        let VpFragment {
            start_bit,
            end_bit,
            data,
            keyframe: keyframe_fn,
        } = if self.codec == Codec::Vp8 {
                let first = payload[0];
                VpFragment {
                    start_bit: first & 0x10 != 0,
                    end_bit: false,
                    data: &payload[1..],
                    keyframe: |data| {
                        data.first().is_some_and(|byte| byte & 0x01 == 0)
                    },
                }
            } else {
                let first = payload[0];
                // I=1: picture ID follows. The M bit (octet 1, bit 7)
                // selects a 7-bit (2-byte descriptor, ffmpeg style) or
                // 15-bit (3-byte) picture ID.
                let offset = if first & 0x80 != 0 {
                    if payload.len() < 2 {
                        return Err(Error::Media("short VP9 payload descriptor".into()));
                    }
                    if payload[1] & 0x80 != 0 { 3 } else { 2 }
                } else {
                    1
                };
                if payload.len() < offset {
                    return Err(Error::Media("short VP9 payload descriptor".into()));
                }
                VpFragment {
                    start_bit: first & 0x04 != 0,
                    end_bit: first & 0x02 != 0,
                    data: &payload[offset..],
                    // frame_type bit of the VP9 uncompressed header:
                    // bit 4 of the first byte, 0 = keyframe.
                    keyframe: |data| data.first().is_some_and(|byte| byte & 0x10 == 0),
                }
            };
        if start_bit {
            // A new frame begins: drop any truncated previous frame.
            if let Some(prev) = self.vp_assembling.take() {
                tracing::debug!(bytes = prev.len(), "VP frame dropped (truncated)");
            }
            if data.len() > MAX_ASSEMBLY_BYTES {
                return Err(Error::Media(format!(
                    "VP frame exceeds {MAX_ASSEMBLY_BYTES} bytes"
                )));
            }
            self.vp_is_keyframe = keyframe_fn(data);
            self.vp_assembling = Some(BytesMut::from(data));
        } else if let Some(buffer) = &mut self.vp_assembling {
            if buffer.len().saturating_add(data.len()) > MAX_ASSEMBLY_BYTES {
                self.vp_assembling = None;
                self.vp_is_keyframe = false;
                return Err(Error::Media(format!(
                    "VP frame exceeds {MAX_ASSEMBLY_BYTES} bytes"
                )));
            }
            buffer.extend_from_slice(data);
        } else {
            // Continuation without a start (first fragment lost): drop
            // until the next start fragment.
            return Ok(vec![]);
        }
        let frame_complete = if self.codec == Codec::Vp8 {
            p.header.marker
        } else {
            // VP9: E bit or RTP marker ends the frame.
            p.header.marker || end_bit
        };
        if frame_complete
            && let Some(frame) = self.vp_assembling.take()
        {
            let is_keyframe = std::mem::take(&mut self.vp_is_keyframe);
            return Ok(vec![AccessUnit {
                track_id: self.track_id,
                codec: self.codec,
                pts: self.pts_from_timestamp(p.header.timestamp),
                dts: None,
                rtp_timestamp: p.header.timestamp,
                is_keyframe,
                data: frame.freeze(),
            }]);
        }
        Ok(vec![])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        media::AccessUnit,
        rtp::{JitterBuffer, JitterBufferConfig},
    };
    use std::time::Duration;
    fn frame(c: Codec, data: &[u8]) -> AccessUnit {
        AccessUnit {
            track_id: 0,
            codec: c,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 7,
            is_keyframe: true,
            data: Bytes::copy_from_slice(data),
        }
    }
    #[test]
    fn packet_view() {
        let p = RtpPacket {
            header: RtpHeader {
                marker: true,
                payload_type: 96,
                sequence: 1,
                timestamp: 2,
                ssrc: 3,
            },
            payload: Bytes::from_static(b"x"),
        };
        assert_eq!(RtpPacket::parse(p.encode()).unwrap().payload, b"x"[..]);
    }
    #[test]
    fn single_packet_fus_emit_an_access_unit() {
        let packet = |payload: &'static [u8]| RtpPacket {
            header: RtpHeader {
                marker: true,
                payload_type: 96,
                sequence: 1,
                timestamp: 2,
                ssrc: 3,
            },
            payload: Bytes::from_static(payload),
        };
        let mut h264 = Depacketizer::new(Codec::H264, 0, 90_000);
        let h264_out = h264.push(packet(&[28, 0xc5, 1, 2, 3])).unwrap();
        assert_eq!(h264_out.len(), 1);
        assert_eq!(
            h264_out[0].data,
            Bytes::from_static(&[0, 0, 0, 1, 0x05, 1, 2, 3])
        );

        let mut h265 = Depacketizer::new(Codec::H265, 0, 90_000);
        let h265_out = h265.push(packet(&[49 << 1, 1, 0xc2, 3, 4])).unwrap();
        assert_eq!(h265_out.len(), 1);
        assert_eq!(
            h265_out[0].data,
            Bytes::from_static(&[0, 0, 0, 1, 4, 1, 3, 4])
        );
    }

    #[test]
    fn h264_fu_roundtrip() {
        let f = frame(Codec::H264, &[0x65; 300]);
        let mut p = Packetizer::new(Codec::H264, 96, 1, 80).unwrap();
        let mut d = Depacketizer::new(Codec::H264, 0, 90000);
        let packets = p.packetize(&f).unwrap();
        let mut out = None;
        for x in packets {
            out = d.push(x).unwrap().into_iter().last().or(out)
        }
        let expected: Vec<u8> = [&[0u8, 0, 0, 1][..], &[0x65u8; 300][..]].concat();
        assert_eq!(out.unwrap().data, Bytes::from(expected))
    }
    #[test]
    fn video_access_units_preserve_multiple_nals() {
        for (codec, input) in [
            (Codec::H264, &[0, 0, 1, 0x61, 1, 0, 0, 1, 0x65, 2][..]),
            (
                Codec::H265,
                &[0, 0, 1, 2 << 1, 1, 1, 0, 0, 1, 19 << 1, 1, 2][..],
            ),
        ] {
            let f = frame(codec, input);
            let mut packetizer = Packetizer::new(codec, 96, 1, 1200).unwrap();
            let mut depacketizer = Depacketizer::new(
                codec,
                0,
                if matches!(codec, Codec::H264 | Codec::H265) {
                    90000
                } else if codec == Codec::Aac {
                    48000
                } else {
                    8000
                },
            );
            let mut output = None;
            for packet in packetizer.packetize(&f).unwrap() {
                output = depacketizer
                    .push(packet)
                    .unwrap()
                    .into_iter()
                    .last()
                    .or(output);
            }
            let output = output.unwrap();
            assert_eq!(annex_b_nals(&output.data).len(), 2);
            assert!(output.is_keyframe);
        }
    }
    #[test]
    fn h264_stap_a_and_h265_ap() {
        let packet = |payload: &'static [u8]| RtpPacket {
            header: RtpHeader {
                marker: true,
                payload_type: 96,
                sequence: 1,
                timestamp: 2,
                ssrc: 3,
            },
            payload: Bytes::from_static(payload),
        };
        let mut h264 = Depacketizer::new(Codec::H264, 0, 90000);
        let out = h264
            .push(packet(&[24, 0, 2, 0x61, 1, 0, 2, 0x65, 2]))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(annex_b_nals(&out.data).len(), 2);
        assert!(out.is_keyframe);
        let mut h265 = Depacketizer::new(Codec::H265, 0, 90000);
        let out = h265
            .push(packet(&[
                48 << 1,
                1,
                0,
                3,
                2 << 1,
                1,
                1,
                0,
                3,
                19 << 1,
                1,
                2,
            ]))
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(annex_b_nals(&out.data).len(), 2);
        assert!(out.is_keyframe);
    }
    #[test]
    fn audio_roundtrips() {
        for codec in [Codec::Aac, Codec::Pcma, Codec::Pcmu] {
            let f = frame(codec, &[1, 2, 3, 4]);
            let mut packetizer = Packetizer::new(codec, 96, 1, 1200).unwrap();
            let mut depacketizer = Depacketizer::new(
                codec,
                0,
                if matches!(codec, Codec::H264 | Codec::H265) {
                    90000
                } else if codec == Codec::Aac {
                    48000
                } else {
                    8000
                },
            );
            let packet = packetizer.packetize(&f).unwrap().remove(0);
            if codec == Codec::Aac {
                assert_eq!(&packet.payload[..4], &[0, 16, 0, 32]);
            }
            assert_eq!(
                depacketizer
                    .push(packet)
                    .unwrap()
                    .into_iter()
                    .next()
                    .unwrap()
                    .data,
                f.data
            );
        }
    }
    #[test]
    fn jitter_timer_releases_after_a_gap_without_a_new_packet() {
        let packet = |sequence| RtpPacket {
            header: RtpHeader {
                marker: true,
                payload_type: 96,
                sequence,
                timestamp: sequence as u32,
                ssrc: 1,
            },
            payload: Bytes::from_static(b"x"),
        };
        let mut jitter = JitterBuffer::new(JitterBufferConfig {
            max_delay: Duration::from_millis(1),
            ..JitterBufferConfig::default()
        });
        assert_eq!(jitter.push(packet(10)).len(), 1);
        assert!(jitter.push(packet(12)).is_empty());
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(jitter.drain_expired()[0].header.sequence, 12);
    }

    #[test]
    fn video_access_unit_is_dropped_after_sequence_loss() {
        let packet = |sequence, timestamp, marker, payload| RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: 96,
                sequence,
                timestamp,
                ssrc: 1,
            },
            payload,
        };
        let mut depacketizer = Depacketizer::new(Codec::H264, 0, 90000);
        assert!(
            depacketizer
                .push(packet(1, 10, false, Bytes::from_static(&[0x61, 1])))
                .unwrap()
                .is_empty()
        );
        assert!(
            depacketizer
                .push(packet(3, 10, true, Bytes::from_static(&[0x65, 2])))
                .unwrap()
                .is_empty()
        );
        assert!(
            !depacketizer
                .push(packet(4, 11, true, Bytes::from_static(&[0x65, 3])))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn jitter_reorders_deduplicates_and_wraps() {
        let packet = |sequence| RtpPacket {
            header: RtpHeader {
                marker: true,
                payload_type: 96,
                sequence,
                timestamp: sequence as u32,
                ssrc: 1,
            },
            payload: Bytes::from_static(b"x"),
        };
        let mut jitter = JitterBuffer::new(JitterBufferConfig::default());
        assert_eq!(jitter.push(packet(10))[0].header.sequence, 10);
        assert!(jitter.push(packet(12)).is_empty());
        let ready = jitter.push(packet(11));
        assert_eq!(
            ready.iter().map(|p| p.header.sequence).collect::<Vec<_>>(),
            vec![11, 12]
        );
        assert!(jitter.push(packet(11)).is_empty());
        jitter.reset();
        assert_eq!(jitter.push(packet(u16::MAX))[0].header.sequence, u16::MAX);
        assert_eq!(jitter.push(packet(0))[0].header.sequence, 0);
    }
}

#[cfg(test)]
mod fu_keyframe_tests {
    use super::*;

    fn packet(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: 96,
                sequence: seq,
                timestamp: ts,
                ssrc: 1,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    /// A multi-packet H.264 IDR (FU-A: 3 fragments) must assemble with
    /// `is_keyframe = true` — the FU start packet carries the NAL type,
    /// which must survive to the FU end packet.
    #[test]
    fn h264_fu_a_multi_fragment_idr_is_keyframe() {
        let mut d = Depacketizer::new(Codec::H264, 0, 90_000);
        let ts = 1000u32;
        // FU-A start: NAL type 5 (IDR), S=1 E=0.
        let fu_start = [0x7c, 0x85, 0x01, 0x02, 0x03];
        let units = d.push(packet(10, ts, false, &fu_start)).unwrap();
        assert!(units.is_empty());
        // FU-A continuation: S=0 E=0.
        let fu_mid = [0x7c, 0x05, 0x04, 0x05];
        assert!(d.push(packet(11, ts, false, &fu_mid)).unwrap().is_empty());
        // FU-A end: S=0 E=1, marker set.
        let fu_end = [0x7c, 0x45, 0x06];
        let units = d.push(packet(12, ts, true, &fu_end)).unwrap();
        assert_eq!(units.len(), 1, "one access unit");
        assert!(units[0].is_keyframe, "FU-A IDR must be a keyframe");
        // Annex-B output: 00 00 00 01 65 ...
        assert_eq!(&units[0].data[..5], &[0, 0, 0, 1, 0x65]);
    }

    /// A multi-packet H.264 P-frame (FU-A, NAL type 1) must NOT be marked
    /// as a keyframe.
    #[test]
    fn h264_fu_a_multi_fragment_delta_is_not_keyframe() {
        let mut d = Depacketizer::new(Codec::H264, 0, 90_000);
        let ts = 2000u32;
        let fu_start = [0x7c, 0x81, 0x01];
        assert!(d.push(packet(20, ts, false, &fu_start)).unwrap().is_empty());
        let fu_end = [0x7c, 0x41, 0x02];
        let units = d.push(packet(21, ts, true, &fu_end)).unwrap();
        assert_eq!(units.len(), 1);
        assert!(!units[0].is_keyframe, "P-frame FU-A must not be a keyframe");
    }

    /// H.265 FU (type 49) IDR (NAL type 19) across fragments.
    #[test]
    fn h265_fu_multi_fragment_idr_is_keyframe() {
        let mut d = Depacketizer::new(Codec::H265, 0, 90_000);
        let ts = 3000u32;
        // FU header byte 0: type 49 << 1 | layer; byte 2: S=1 E=0, type 19.
        let fu_start = [0x62, 0x01, 0x93, 0xaa, 0xbb];
        assert!(d.push(packet(30, ts, false, &fu_start)).unwrap().is_empty());
        let fu_end = [0x62, 0x01, 0x53, 0xcc];
        let units = d.push(packet(31, ts, true, &fu_end)).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].is_keyframe, "H265 FU IDR must be a keyframe");
    }
}

#[cfg(test)]
mod audit_tests {
    use super::*;

    fn opus_packet(payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker: true,
                payload_type: 120,
                sequence: 1,
                timestamp: 1,
                ssrc: 0,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn opus_depacketizes_single_packet() {
        let mut dep = Depacketizer::new(Codec::Opus, 0, 48_000);
        let unit = dep.push(opus_packet(&[0xf8, 0xff, 0xfe, 0x00])).unwrap();
        assert_eq!(unit.len(), 1);
        assert_eq!(unit[0].data.as_ref(), &[0xf8, 0xff, 0xfe, 0x00]);
        assert!(unit[0].is_keyframe);
    }

    /// Unsupported codecs must surface a typed error, never a panic.
    #[test]
    fn unsupported_codec_returns_error_not_panic() {
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let err = dep.push(opus_packet(&[0x80])).unwrap_err();
        assert!(
            matches!(err, Error::Media(_)),
            "typed error for unsupported codec: {err:?}"
        );
    }
}

#[cfg(test)]
mod vp8_tests {
    use super::*;
    use std::time::Duration;

    fn vp8_keyframe() -> Bytes {
        // VP8 frame header: P bit (bit 0) = 0 → keyframe.
        Bytes::from(vec![
            0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x00, 0x00, 0x00, 0x00,
        ])
    }

    fn packet(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: 96,
                sequence: seq,
                timestamp: ts,
                ssrc: 1,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn vp8_roundtrip_single_packet() {
        let frame = vp8_keyframe();
        let mut pack = Packetizer::new(Codec::Vp8, 0, 90_000, 1200).unwrap();
        let packets = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp8,
                pts: Duration::from_millis(0),
                dts: None,
                rtp_timestamp: 1000,
                is_keyframe: true,
                data: frame.clone(),
            })
            .unwrap();
        assert_eq!(packets.len(), 1);
        // Descriptor: S=1, PID=0 → 0x10; marker set.
        assert!(packets[0].header.marker);
        assert_eq!(packets[0].payload[0], 0x10);

        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        let units = dep.push(packets[0].clone()).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, frame);
        assert!(units[0].is_keyframe, "P bit 0 = keyframe");
    }

    #[test]
    fn vp8_roundtrip_fragmented() {
        // Frame larger than MTU → multiple fragments with S/PID/marker.
        let frame: Bytes = {
            let mut v = vp8_keyframe().to_vec();
            v.resize(3000, 0xab);
            Bytes::from(v)
        };
        let mut pack = Packetizer::new(Codec::Vp8, 0, 90_000, 1200).unwrap();
        let packets = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp8,
                pts: Duration::from_millis(0),
                dts: None,
                rtp_timestamp: 2000,
                is_keyframe: true,
                data: frame.clone(),
            })
            .unwrap();
        assert!(packets.len() >= 2, "large frame is fragmented");
        // Descriptor flags: first S=1, last marker=1, PID increments.
        assert_eq!(packets[0].payload[0] & 0x10, 0x10, "S on first fragment");
        assert_eq!(packets[0].payload[0] & 0x0f, 0, "PID 0");
        assert!(packets.last().unwrap().header.marker, "marker on last");

        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        let mut units = Vec::new();
        for p in &packets {
            units.extend(dep.push(p.clone()).unwrap());
        }
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, frame, "fragments reassembled");
        assert!(units[0].is_keyframe);
    }

    #[test]
    fn vp8_continuation_without_start_is_dropped() {
        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        // First fragment lost: a continuation packet must not panic and
        // must not emit a frame.
        let units = dep
            .push(packet(5, 3000, false, &[0x01, 0x02, 0x03]))
            .unwrap();
        assert!(units.is_empty());
        // The next complete frame recovers normally.
        let p = packet(6, 3000, true, &[0x10, 0x00, 0x02]);
        let units = dep.push(p).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].is_keyframe);
    }

    #[test]
    fn vp8_interframe_is_not_keyframe() {
        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        let units = dep
            .push(packet(1, 4000, true, &[0x10, 0x01, 0x02]))
            .unwrap();
        assert!(!units[0].is_keyframe, "P bit 1 = interframe");
    }
}

#[cfg(test)]
mod vp8_gap_tests {
    use super::*;

    fn packet(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: 96,
                sequence: seq,
                timestamp: ts,
                ssrc: 1,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    /// A sequence gap mid-frame must discard the in-flight fragments:
    /// splicing fragments of two frames would emit a corrupted frame.
    #[test]
    fn vp8_sequence_gap_discards_inflight_frame() {
        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        // Frame 1: fragments seq 1 (S) ... seq 2.
        let _ = dep.push(packet(1, 100, false, &[0x10, 0x00, 0x01])).unwrap();
        // Gap: seq 4 (not 3) — starts a new frame with S=1.
        let units = dep.push(packet(4, 200, true, &[0x10, 0x00, 0x02])).unwrap();
        assert_eq!(units.len(), 1, "new frame still delivered");
        assert_eq!(units[0].data.as_ref(), &[0x00, 0x02]);
        // The spliced buffer must NOT contain bytes from the abandoned frame.
        assert_ne!(units[0].data.as_ref(), &[0x00, 0x01, 0x00, 0x02]);
    }
}

#[cfg(test)]
mod vp9_tests {
    use super::*;
    use std::time::Duration;

    fn vp9_keyframe() -> Vec<u8> {
        // VP9 uncompressed header (profile 0): frame_type bit (0x10)
        // = 0 → keyframe; a few more header bytes to be realistic.
        vec![0x82, 0x49, 0x83, 0x42, 0x00, 0x01, 0x02, 0x03]
    }

    fn packet(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: 98,
                sequence: seq,
                timestamp: ts,
                ssrc: 1,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn vp9_single_packet_round_trip() {
        let frame_data = vp9_keyframe();
        let mut pack = Packetizer::new(Codec::Vp9, 98, 7, 1400).unwrap();
        let packets = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp9,
                pts: Duration::from_millis(33000),
                dts: None,
                rtp_timestamp: 33_000,
                is_keyframe: true,
                data: Bytes::from(frame_data.clone()),
            })
            .unwrap();
        assert_eq!(packets.len(), 1);
        // Descriptor: I=1, B=1, E=1.
        assert_eq!(packets[0].payload[0], 0x80 | 0x04 | 0x02);
        assert_eq!(packets[0].payload[1] & 0x80, 0x80, "M=1 (15-bit picture ID)");
        assert_eq!(
            u16::from_be_bytes([packets[0].payload[1] & 0x7f, packets[0].payload[2]]),
            0
        );
        assert_eq!(&packets[0].payload[3..], &frame_data[..]);
        assert!(packets[0].header.marker);

        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let units = dep.push(packets[0].clone()).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].is_keyframe, "VP9 frame_type bit 0 = keyframe");
        assert_eq!(units[0].data.as_ref(), &frame_data[..]);
        assert_eq!(units[0].rtp_timestamp, 33_000);
    }

    #[test]
    fn vp9_fragmented_round_trip_and_picture_id() {
        let frame_data: Vec<u8> = {
            let mut v = vp9_keyframe();
            v.extend(std::iter::repeat_n(0x5a, 3000));
            v
        };
        let mut pack = Packetizer::new(Codec::Vp9, 98, 7, 1200).unwrap();
        let f1 = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp9,
                pts: Duration::ZERO,
                dts: None,
                rtp_timestamp: 0,
                is_keyframe: true,
                data: Bytes::from(frame_data.clone()),
            })
            .unwrap();
        assert!(f1.len() >= 3, "fragmented into {}", f1.len());
        // First fragment: B=1, E=0; last: B=0, E=1; picture ID constant.
        assert_eq!(f1[0].payload[0] & 0x04, 0x04);
        assert_eq!(f1[0].payload[0] & 0x02, 0);
        let last = f1.last().unwrap();
        assert_eq!(last.payload[0] & 0x02, 0x02);
        assert!(last.header.marker);
        let pid0 = u16::from_be_bytes([f1[0].payload[1] & 0x7f, f1[0].payload[2]]);
        let pid_last = u16::from_be_bytes([last.payload[1] & 0x7f, last.payload[2]]);
        assert_eq!(pid0, pid_last, "picture ID constant within a frame");

        // Second frame: picture ID increments.
        let f2 = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp9,
                pts: Duration::from_millis(3000),
                dts: None,
                rtp_timestamp: 3000,
                is_keyframe: false,
                data: Bytes::from(vec![0x11, 0x02, 0x03]),
            })
            .unwrap();
        let pid1 = u16::from_be_bytes([f2[0].payload[1] & 0x7f, f2[0].payload[2]]);
        assert_eq!(pid1, pid0.wrapping_add(1));

        // Reassemble: feed every fragment of both frames.
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let mut out = Vec::new();
        let mut seq = 0u16;
        for p in f1.iter().chain(f2.iter()) {
            seq = seq.wrapping_add(1);
            out.extend(dep.push(packet(seq, p.header.timestamp, p.header.marker, &p.payload)).unwrap());
        }
        assert_eq!(out.len(), 2, "both frames reassembled");
        assert!(out[0].is_keyframe);
        assert!(!out[1].is_keyframe);
        assert_eq!(out[0].data.as_ref(), &frame_data[..]);
        assert_eq!(out[1].data.as_ref(), &[0x11, 0x02, 0x03]);
    }

    #[test]
    fn vp9_seven_bit_picture_id_descriptor() {
        // ffmpeg/libwebrtc may send I=1 with M=0: a 7-bit picture ID in
        // octet 1, so the descriptor is 2 bytes total. The depacketizer
        // must find the payload at offset 2, not 3.
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let units = dep
            .push(packet(7, 900, true, &[0x86, 0x2a, 0xde, 0xad, 0xbe]))
            .unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data.as_ref(), &[0xde, 0xad, 0xbe]);
    }

    #[test]
    fn vp9_sequence_gap_discards_inflight_frame() {
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        // Fragment 1 of a frame (I=1, M=1, B=1, E=0).
        let _ = dep
            .push(packet(1, 100, false, &[0x84, 0x80, 0x01, 0xaa, 0xbb]))
            .unwrap();
        // Gap (seq 3), new frame I=1 M=1 B=1 E=1.
        let units = dep
            .push(packet(3, 200, true, &[0x86, 0x80, 0x02, 0xcc]))
            .unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data.as_ref(), &[0xcc], "abandoned fragments dropped");
    }

    #[test]
    fn vp9_end_bit_without_marker_completes_frame() {
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let units = dep
            .push(packet(1, 500, false, &[0x86, 0x80, 0x05, 0xde, 0xad]))
            .unwrap();
        assert_eq!(units.len(), 1, "E bit ends the frame even without marker");
        assert_eq!(units[0].data.as_ref(), &[0xde, 0xad]);
    }
}
