use crate::rtp::{RtpHeader, RtpPacket};
use crate::{
    Error, Result,
    media::{AccessUnit, Codec},
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
    width: u16,
    height: u16,
    av1_sequence_sent: bool,
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
            width: 0,
            height: 0,
            av1_sequence_sent: false,
        })
    }
    /// Set the picture dimensions used by RFC 2435 JPEG headers.
    pub fn with_dimensions(mut self, width: u16, height: u16) -> Self {
        self.width = width;
        self.height = height;
        self
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
            Codec::Mp3 => self.mpa(frame),
            Codec::G726_16 | Codec::G726_24 | Codec::G726_32 | Codec::G726_40 => {
                let frame_bytes = match self.codec {
                    Codec::G726_16 => 40,
                    Codec::G726_24 => 60,
                    Codec::G726_32 => 80,
                    Codec::G726_40 => 100,
                    _ => unreachable!(),
                };
                if frame.data.is_empty() || !frame.data.len().is_multiple_of(frame_bytes) {
                    return Err(Error::Media("invalid G.726 access unit length".into()));
                }
                Ok(vec![self.packet(
                    frame.data.clone(),
                    frame.rtp_timestamp,
                    true,
                )])
            }
            Codec::Vp8 => self.vp8(frame),
            Codec::Vp9 => self.vp9(frame),
            Codec::Av1 => self.av1(frame),
            Codec::Jpeg => self.jpeg(frame),
            Codec::H263 => self.h263(frame),
            Codec::Mpeg4 => self.mpeg4(frame),
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
        let mut buffer = BytesMut::with_capacity(f.data.len() + 3 * total);
        let mut out = Vec::with_capacity(total);
        for (index, chunk) in f.data.chunks(fragment_size).enumerate() {
            let first = index == 0;
            let last = index + 1 == total;
            // I=1 (picture ID present), M=1 (15-bit picture ID), B/E flags.
            // Always using the 15-bit form keeps the descriptor 3 bytes
            // regardless of the picture-ID value, which every conformant
            // decoder (libwebrtc, ffmpeg) accepts.
            // I=1 (picture ID), B=0x08 starts a frame, E=0x04 ends it.
            buffer.put_u8(0x80 | ((first as u8) << 3) | ((last as u8) << 2));
            buffer.put_u8(0x80 | ((picture_id >> 8) & 0x7f) as u8);
            buffer.put_u8((picture_id & 0xff) as u8);
            buffer.extend_from_slice(chunk);
            let length = buffer.len();
            let payload = buffer.split_to(length).freeze();
            out.push(self.packet(payload, f.rtp_timestamp, last));
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
        let mut buffer = BytesMut::with_capacity(f.data.len() + total);
        let mut out = Vec::with_capacity(total);
        for (index, chunk) in f.data.chunks(fragment_size).enumerate() {
            let start = index == 0;
            buffer.put_u8(((start as u8) << 4) | (index & 0x0f) as u8);
            buffer.extend_from_slice(chunk);
            let length = buffer.len();
            let payload = buffer.split_to(length).freeze();
            out.push(self.packet(payload, f.rtp_timestamp, index + 1 == total));
        }
        Ok(out)
    }
    fn h264(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if f.data.is_empty() {
            return Err(Error::Media("empty H264 access unit".into()));
        }
        let max = self.mtu.saturating_sub(12);
        if max <= 2 {
            return Err(Error::Media("MTU too small for H264".into()));
        }
        let nals = annex_b_nals(&f.data);
        let nals = if nals.is_empty() {
            vec![f.data.clone()]
        } else {
            nals
        };
        let total = nals.len();
        let fragments: usize = nals
            .iter()
            .map(|nal| {
                if nal.len() <= max {
                    1
                } else {
                    (nal.len() - 1).div_ceil(max - 2)
                }
            })
            .sum();
        // One backing allocation for the whole access unit: every payload is
        // a slice of this buffer, so fragmentation does not allocate per
        // packet (the wire encoder reuses its own buffer as well).
        let mut buffer = BytesMut::with_capacity(f.data.len() + 2 * fragments + 2);
        let mut out = Vec::with_capacity(fragments);
        for (i, nal) in nals.into_iter().enumerate() {
            if nal.len() <= max {
                let length = nal.len();
                buffer.extend_from_slice(&nal);
                let payload = buffer.split_to(length).freeze();
                out.push(self.packet(payload, f.rtp_timestamp, i + 1 == total));
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
                buffer.extend_from_slice(&[
                    (hdr & 0xe0) | 28,
                    ((pos == 1) as u8) << 7 | ((end == nal.len()) as u8) << 6 | (hdr & 0x1f),
                ]);
                buffer.extend_from_slice(&nal[pos..end]);
                let length = buffer.len();
                let payload = buffer.split_to(length).freeze();
                out.push(self.packet(payload, f.rtp_timestamp, end == nal.len() && i + 1 == total));
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
        if max <= 3 {
            return Err(Error::Media("MTU too small for H265".into()));
        }
        let nals = annex_b_nals(&f.data);
        let nals = if nals.is_empty() {
            vec![f.data.clone()]
        } else {
            nals
        };
        let total = nals.len();
        let fragments: usize = nals
            .iter()
            .map(|nal| {
                if nal.len() <= max {
                    1
                } else {
                    (nal.len().saturating_sub(2)).div_ceil(max - 3)
                }
            })
            .sum();
        let mut buffer = BytesMut::with_capacity(f.data.len() + 3 * fragments + 3);
        let mut out = Vec::with_capacity(fragments);
        for (index, nal) in nals.into_iter().enumerate() {
            if nal.len() < 2 {
                return Err(Error::Media("short H265 NAL".into()));
            }
            if nal.len() <= max {
                let length = nal.len();
                buffer.extend_from_slice(&nal);
                let payload = buffer.split_to(length).freeze();
                out.push(self.packet(payload, f.rtp_timestamp, index + 1 == total));
                continue;
            }
            let h0 = nal[0];
            let h1 = nal[1];
            let size = max - 3;
            let mut pos = 2;
            while pos < nal.len() {
                let end = (pos + size).min(nal.len());
                buffer.extend_from_slice(&[
                    (h0 & 0x81) | (49 << 1),
                    h1,
                    ((pos == 2) as u8) << 7 | ((end == nal.len()) as u8) << 6 | ((h0 >> 1) & 0x3f),
                ]);
                buffer.extend_from_slice(&nal[pos..end]);
                let length = buffer.len();
                let payload = buffer.split_to(length).freeze();
                out.push(self.packet(
                    payload,
                    f.rtp_timestamp,
                    end == nal.len() && index + 1 == total,
                ));
                pos = end;
            }
        }
        Ok(out)
    }
    fn av1(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        let max = self.mtu.saturating_sub(12);
        let mut sent = self.av1_sequence_sent;
        let payloads = super::av1::packetize(&f.data, max, f.is_keyframe, &mut sent)?;
        self.av1_sequence_sent = sent;
        let mut out = Vec::with_capacity(payloads.len());
        for (payload, marker) in payloads {
            out.push(self.packet(payload, f.rtp_timestamp, marker));
        }
        Ok(out)
    }

    /// RFC 2250 MPEG audio: every payload starts with a 4-byte fragment
    /// header; FFmpeg and GStreamer strip it before the MP3 parser.
    fn mpa(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if f.data.is_empty() {
            return Err(Error::Media("empty MPEG audio frame".into()));
        }
        let max = self.mtu.saturating_sub(12);
        if max <= 4 {
            return Err(Error::Media("MTU too small for MPEG audio".into()));
        }
        let chunk = max - 4;
        let total = f.data.len().div_ceil(chunk);
        let mut out = Vec::with_capacity(total);
        for (index, data) in f.data.chunks(chunk).enumerate() {
            let offset = (index * chunk) as u16;
            let mut payload = BytesMut::with_capacity(4 + data.len());
            payload.extend_from_slice(&offset.to_be_bytes());
            payload.extend_from_slice(&[0, 0]);
            payload.extend_from_slice(data);
            out.push(self.packet(payload.freeze(), f.rtp_timestamp, index + 1 == total));
        }
        Ok(out)
    }

    fn jpeg(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if f.data.is_empty() {
            return Err(Error::Media("empty JPEG access unit".into()));
        }
        let scan = jpeg_scan(&f.data);
        if scan.is_empty() {
            return Err(Error::Media("empty JPEG scan".into()));
        }
        let max = self.mtu.saturating_sub(12);
        if max <= 8 {
            return Err(Error::Media("MTU too small for JPEG".into()));
        }
        let chunk = max - 8;
        let total = scan.len().div_ceil(chunk);
        let mut buffer = BytesMut::with_capacity(scan.len() + 8 * total);
        let mut out = Vec::with_capacity(total);
        for (index, data) in scan.chunks(chunk).enumerate() {
            let offset = (index * chunk) as u32;
            buffer.extend_from_slice(&[
                1,
                ((offset >> 16) & 0xff) as u8,
                ((offset >> 8) & 0xff) as u8,
                (offset & 0xff) as u8,
                1,
                80,
                self.width.div_ceil(8).min(255) as u8,
                self.height.div_ceil(8).min(255) as u8,
            ]);
            buffer.extend_from_slice(data);
            let length = buffer.len();
            let payload = buffer.split_to(length).freeze();
            out.push(self.packet(payload, f.rtp_timestamp, index + 1 == total));
        }
        Ok(out)
    }

    fn h263(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if f.data.is_empty() {
            return Err(Error::Media("empty H263 access unit".into()));
        }
        let max = self.mtu.saturating_sub(12);
        if max <= 4 {
            return Err(Error::Media("MTU too small for H263".into()));
        }
        // RFC 2190 mode A: 4-byte header, picture start code kept in-band.
        // Both FFmpeg (H263/PT34) and GStreamer rtph263depay implement this
        // format; H263-1998 (RFC 4629) is only used on the input side.
        let chunk = max - 4;
        let total = f.data.len().div_ceil(chunk);
        let mut out = Vec::with_capacity(total);
        for (index, data) in f.data.chunks(chunk).enumerate() {
            let start = index == 0;
            let header = [
                if start { 0x40 } else { 0x00 },
                if start { 0x10 } else { 0x00 },
                0,
                0,
            ];
            let mut payload = BytesMut::with_capacity(4 + data.len());
            payload.extend_from_slice(&header);
            payload.extend_from_slice(data);
            out.push(self.packet(payload.freeze(), f.rtp_timestamp, index + 1 == total));
        }
        Ok(out)
    }

    fn mpeg4(&mut self, f: &AccessUnit) -> Result<Vec<RtpPacket>> {
        if f.data.is_empty() {
            return Err(Error::Media("empty MPEG-4 access unit".into()));
        }
        let max = self.mtu.saturating_sub(12);
        if max == 0 {
            return Err(Error::Media("MTU too small for MPEG-4".into()));
        }
        let total = f.data.len().div_ceil(max);
        let mut out = Vec::with_capacity(total);
        for (index, data) in f.data.chunks(max).enumerate() {
            out.push(self.packet(
                Bytes::copy_from_slice(data),
                f.rtp_timestamp,
                index + 1 == total,
            ));
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
pub(super) fn annex_b_nals(data: &Bytes) -> Vec<Bytes> {
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

/// Return the entropy-coded scan of a complete JPEG frame (after SOS), with
/// the trailing EOI removed. Inputs that are already scan data pass through.
fn jpeg_scan(data: &[u8]) -> &[u8] {
    if data.len() < 4 || data[0] != 0xff || data[1] != 0xd8 {
        return data;
    }
    let mut index = 2usize;
    while index + 3 < data.len() {
        if data[index] != 0xff {
            break;
        }
        let marker = data[index + 1];
        if marker == 0xd9 {
            break;
        }
        if (0xd0..=0xd7).contains(&marker) || marker == 0x01 {
            index += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([data[index + 2], data[index + 3]]));
        if length < 2 {
            break;
        }
        if marker == 0xda {
            let start = index + 2 + length;
            if start >= data.len() {
                break;
            }
            let mut end = data.len();
            if end >= 2 && data[end - 2] == 0xff && data[end - 1] == 0xd9 {
                end -= 2;
            }
            return &data[start..end];
        }
        index += 2 + length;
    }
    data
}
