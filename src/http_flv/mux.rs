use base64::Engine;
use bytes::{BufMut, Bytes, BytesMut};
use std::time::Duration;

type H265Sprop = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

use crate::media::{AccessUnit, Codec, MediaDescription};

const FLV_HEADER_SIZE: usize = 9;
const FLV_TAG_HEADER_SIZE: usize = 11;

pub struct FlvMuxer {
    has_video: bool,
    has_audio: bool,
    video_sequence_header_sent: bool,
    audio_sequence_header_sent: bool,
    tag_buffer: BytesMut,
    /// Cached video decoder configuration record (sequence header).
    video_sequence_header: Option<Bytes>,
    /// Cached AAC audio specific config (sequence header).
    aac_sequence_header: Option<Bytes>,
    /// PTS of the first emitted access unit, used to normalize the shared FLV timeline.
    first_pts: Option<Duration>,
    /// Next decode timestamp for video. RTP delivers B-frames in decode order.
    next_video_dts: Option<Duration>,
    // Presentation timestamps are reordered by B-frames; their smallest observed
    // non-zero delta is the encoded frame duration.
    last_video_pts: Option<Duration>,
    video_frame_duration: Option<Duration>,
}

impl FlvMuxer {
    pub fn new(description: &MediaDescription) -> Self {
        let video_track = description
            .tracks
            .iter()
            .find(|t| t.kind == crate::media::MediaKind::Video);
        let has_video = video_track.is_some();
        let has_audio = description
            .tracks
            .iter()
            .any(|t| t.kind == crate::media::MediaKind::Audio);
        Self {
            has_video,
            has_audio,
            video_sequence_header_sent: false,
            audio_sequence_header_sent: false,
            tag_buffer: BytesMut::with_capacity(4096),
            video_sequence_header: None,
            aac_sequence_header: None,
            first_pts: None,
            next_video_dts: None,
            last_video_pts: None,
            video_frame_duration: None,
        }
    }

    /// Write FLV file header (9 bytes + 4 bytes previous tag size 0).
    pub fn flv_header(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(FLV_HEADER_SIZE + 4);
        buf.put_slice(b"FLV"); // signature
        buf.put_u8(1); // version
        let flags = (if self.has_video { 1u8 } else { 0 }) | (if self.has_audio { 4u8 } else { 0 });
        buf.put_u8(flags); // type flags
        buf.put_u32(FLV_HEADER_SIZE as u32); // data offset
        buf.put_u32(0); // previous tag size 0
        buf.freeze()
    }

    /// onMetaData script tag (width/height/framerate/duration). Some legacy
    /// Flash-era and Android players refuse to start without it. AMF0 is
    /// encoded inline so http-flv does not depend on the rtmp feature.
    pub fn metadata_tag(&self) -> Option<Bytes> {
        let mut payload = BytesMut::with_capacity(128);
        // "onMetaData" string
        payload.put_u8(0x02);
        payload.put_u16(10);
        payload.put_slice(b"onMetaData");
        // ECMA array
        let mut props: Vec<(&str, f64)> = vec![("server", 0.0), ("duration", 0.0)];
        let mut bool_props: Vec<(&str, bool)> = Vec::new();
        if self.has_video {
            props.push(("videocodecid", 7.0));
            props.push(("framerate", 30.0));
            props.push(("width", 0.0));
            props.push(("height", 0.0));
        }
        if self.has_audio {
            props.push(("audiocodecid", 10.0));
            props.push(("audiosamplerate", 44100.0));
            props.push(("audiosamplesize", 16.0));
            bool_props.push(("stereo", true));
        }
        let count = (props.len() + bool_props.len()) as u32;
        payload.put_u8(0x08);
        payload.put_u32(count);
        for (key, value) in props {
            payload.put_u16(key.len() as u16);
            payload.put_slice(key.as_bytes());
            payload.put_u8(0x00); // number
            payload.put_f64(value);
        }
        for (key, value) in bool_props {
            payload.put_u16(key.len() as u16);
            payload.put_slice(key.as_bytes());
            payload.put_u8(0x01); // boolean
            payload.put_u8(value as u8);
        }
        payload.put_u32(0x0000_0009); // ECMA array end marker
        let mut buf = BytesMut::with_capacity(payload.len() + 16);
        buf.put_u8(0x12); // AMF0 script tag
        let len = payload.len() as u32;
        buf.put_slice(&len.to_be_bytes()[1..]); // 24-bit
        buf.put_slice(&0u32.to_be_bytes()[1..]); // timestamp (3 bytes)
        buf.put_u8(0); // timestamp extension
        // FLV tag header is 11 bytes: type(1) + size(3) + ts(3) + ext(1)
        // + stream id(3). One extra byte here shifts every following tag.
        buf.put_slice(&0u32.to_be_bytes()[1..]); // stream id (3 bytes)
        buf.put_slice(&payload);
        buf.put_u32((payload.len() + 11) as u32); // previous tag size
        Some(buf.freeze())
    }

    /// Set the video sequence header (AVCDecoderConfigurationRecord or HEVCDecoderConfigurationRecord).
    pub fn set_video_sequence_header(&mut self, data: Bytes) {
        self.video_sequence_header = Some(data);
    }

    /// Set the AAC sequence header (AudioSpecificConfig).
    pub fn set_aac_sequence_header(&mut self, data: Bytes) {
        self.aac_sequence_header = Some(data);
    }

    /// Reset timestamp and codec-header emission state after a subscriber gap.
    pub fn reset_after_discontinuity(&mut self) {
        self.video_sequence_header_sent = false;
        self.audio_sequence_header_sent = false;
        self.first_pts = None;
        self.next_video_dts = None;
        self.last_video_pts = None;
        self.video_frame_duration = None;
    }

    /// Encode an AccessUnit into FLV tags.
    /// Returns one or more FLV tags (may include sequence headers).
    pub fn encode(&mut self, unit: &AccessUnit) -> Bytes {
        self.tag_buffer.clear();

        // Normalize the shared FLV timeline to start at 0.  Audio and video
        // must use the same base or a cached video keyframe can move DTS backward.
        let base = *self.first_pts.get_or_insert(unit.pts);
        let normalized_pts = unit.pts.saturating_sub(base);
        if unit.codec == Codec::Aac && tracing::enabled!(tracing::Level::DEBUG) {
            tracing::debug!(
                "mux: aac pts={:?} rtp_ts={} base={:?} norm={:?}",
                unit.pts,
                unit.rtp_timestamp,
                base,
                normalized_pts
            );
        }
        let mut normalized = unit.clone();
        normalized.pts = normalized_pts;
        if matches!(normalized.codec, Codec::H264 | Codec::H265) {
            // RTP access units arrive in decode order while their PTS can be
            // reordered by B-frames.  FLV timestamps carry DTS; retain PTS in
            // the signed CompositionTime field written by the video tag.
            let frame_duration = self.observe_video_frame_duration(normalized.pts);
            let dts = self.next_video_dts.unwrap_or(normalized.pts);
            self.next_video_dts = Some(dts.saturating_add(frame_duration));
            normalized.dts = Some(dts);
        }

        match normalized.codec {
            Codec::H264 => {
                if !self.video_sequence_header_sent
                    && let Some(ref seq) = self.video_sequence_header
                {
                    write_h264_video_tag(&mut self.tag_buffer, &normalized, seq, true);
                    self.video_sequence_header_sent = true;
                }
                write_h264_video_tag_inplace(&mut self.tag_buffer, &normalized, &normalized.data);
            }
            Codec::H265 => {
                if !self.video_sequence_header_sent
                    && let Some(ref seq) = self.video_sequence_header
                {
                    write_h265_video_tag(&mut self.tag_buffer, &normalized, seq, true);
                    self.video_sequence_header_sent = true;
                }
                write_h265_video_tag_inplace(&mut self.tag_buffer, &normalized, &normalized.data);
            }
            Codec::Aac => {
                if !self.audio_sequence_header_sent
                    && let Some(ref seq) = self.aac_sequence_header
                {
                    write_audio_tag(&mut self.tag_buffer, &normalized, seq, true);
                    self.audio_sequence_header_sent = true;
                }
                write_audio_tag(&mut self.tag_buffer, &normalized, &normalized.data, false);
            }
            Codec::Pcma | Codec::Pcmu => {
                write_g711_audio_tag(&mut self.tag_buffer, &normalized);
            }
            Codec::Opus | Codec::Vp8 | Codec::Vp9 => {
                // FLV does not support these codecs; skip silently.
            }
        }

        self.tag_buffer.split_to(self.tag_buffer.len()).freeze()
    }

    fn observe_video_frame_duration(&mut self, pts: Duration) -> Duration {
        if let Some(previous) = self.last_video_pts {
            let delta = pts.abs_diff(previous);
            if !delta.is_zero() && delta <= Duration::from_secs(1) {
                self.video_frame_duration = Some(
                    self.video_frame_duration
                        .map_or(delta, |current| current.min(delta)),
                );
            }
        }
        self.last_video_pts = Some(pts);
        self.video_frame_duration
            .unwrap_or(Duration::from_millis(40))
    }

    /// Build an AVCDecoderConfigurationRecord from Annex-B H.264 data
    /// (SPS + PPS extracted via [`crate::media::bitstream::h264_parameter_sets`]).
    pub fn build_avc_sequence_header(annex_b_data: &Bytes) -> Option<Bytes> {
        let sets = crate::media::bitstream::h264_parameter_sets(annex_b_data);
        crate::media::bitstream::build_avc_config_record(sets.sps.as_deref()?, sets.pps.as_deref()?)
    }

    /// Build a HEVCDecoderConfigurationRecord from Annex-B H.265 data
    /// (VPS/SPS/PPS extracted via [`crate::media::bitstream::h265_parameter_sets`]).
    pub fn build_h265_sequence_header(annex_b_data: &Bytes) -> Option<Bytes> {
        let sets = crate::media::bitstream::h265_parameter_sets(annex_b_data);
        crate::media::bitstream::build_hevc_config_record(
            sets.vps.as_deref()?,
            sets.sps.as_deref()?,
            sets.pps.as_deref()?,
        )
    }

    /// Build an AVCDecoderConfigurationRecord from pre-extracted SPS/PPS NALs.
    pub fn build_avc_sequence_header_from_nals(sps: &[u8], pps: &[u8]) -> Option<Bytes> {
        crate::media::bitstream::build_avc_config_record(sps, pps)
    }

    /// Build a HEVCDecoderConfigurationRecord from pre-extracted VPS/SPS/PPS NALs.
    pub fn build_h265_sequence_header_from_nals(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
    ) -> Option<Bytes> {
        crate::media::bitstream::build_hevc_config_record(vps, sps, pps)
    }

    /// Parse AAC AudioSpecificConfig from an fmtp string.
    pub fn parse_aac_config_from_fmtp(fmtp: &str) -> Option<Bytes> {
        let config_hex = fmtp
            .split(';')
            .find(|kv| kv.trim().starts_with("config="))
            .and_then(|kv| kv.split_once('=').map(|(_, value)| value))?
            .trim();
        let bytes: Option<Vec<u8>> = (0..config_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&config_hex[i..i + 2], 16).ok())
            .collect();
        let bytes = bytes?;
        if bytes.is_empty() {
            return None;
        }
        Some(Bytes::from(bytes))
    }

    /// Parse H.265 sprop-vps/sprop-sps/sprop-pps from an fmtp string.
    pub fn parse_h265_sprop_from_fmtp(fmtp: &str) -> H265Sprop {
        let b64_decode = |key: &str| -> Option<Vec<u8>> {
            let val = fmtp
                .split(';')
                .find(|kv| kv.trim().starts_with(key))
                .and_then(|kv| kv.split_once('=').map(|(_, value)| value))?
                .trim();
            base64::engine::general_purpose::STANDARD.decode(val).ok()
        };
        (
            b64_decode("sprop-vps"),
            b64_decode("sprop-sps"),
            b64_decode("sprop-pps"),
        )
    }

    /// Parse H.264 sprop-parameter-sets from an fmtp string.
    pub fn parse_h264_sprop_from_fmtp(fmtp: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let val = match fmtp
            .split(';')
            .find(|kv| kv.trim().starts_with("sprop-parameter-sets"))
            .and_then(|kv| kv.split_once('=').map(|(_, value)| value))
        {
            Some(v) => v.trim(),
            None => return (None, None),
        };
        let parts: Vec<&str> = val.split(',').collect();
        if parts.len() < 2 {
            return (None, None);
        }
        let decode = |b64: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .ok()
        };
        (decode(parts[0]), decode(parts[1]))
    }
}

/// Write an H.264 video FLV tag (traditional format: codec_id = 7).
fn write_h264_video_tag(
    buf: &mut BytesMut,
    unit: &AccessUnit,
    data: &Bytes,
    is_sequence_header: bool,
) {
    write_flv_tag_header(buf, unit, data.len() + 5);

    // Frame type (4 bits) + Codec ID (4 bits)
    let frame_type = if unit.is_keyframe { 1u8 } else { 2u8 };
    buf.put_u8((frame_type << 4) | 7); // 7 = AVC

    // AVC packet type
    buf.put_u8(if is_sequence_header { 0 } else { 1 });

    write_composition_time(buf, unit);

    buf.put_slice(data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + data.len() + 5;
    buf.put_u32(tag_total_size as u32);
}

/// audio frames) would otherwise be mistaken for a 3-byte Annex-B start
/// code and corrupted by conversion.
fn is_length_prefixed_chain(data: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if len == 0 {
            return false; // empty NAL: not a valid AVCC chain
        }
        pos += 4 + len;
        if pos == data.len() {
            return true;
        }
        if pos > data.len() {
            return false;
        }
    }
    false
}

/// Converted AVCC/HVCC (length-prefixed) size of `data`, or `None` when
/// `data` is already length-prefixed (AVCC/HVCC, e.g. from RTMP ingest).
///
/// Detection: length-prefixed chains pass through untouched; payloads
/// starting with an Annex-B start code are converted (single scan, no
/// allocation); anything else (raw single NAL without framing) also
/// passes through, matching the pre-optimization behaviour.
fn annex_b_avcc_size(data: &Bytes) -> Option<usize> {
    let bytes = data.as_ref();
    if is_length_prefixed_chain(bytes) {
        return None;
    }
    let starts_with_start_code = bytes.len() >= 4
        && ((bytes[0] == 0 && bytes[1] == 0 && bytes[2] == 1)
            || (bytes[0] == 0 && bytes[1] == 0 && bytes[2] == 0 && bytes[3] == 1));
    if !starts_with_start_code {
        return None;
    }
    let mut total = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        let Some(start) = find_start_code(bytes, i) else {
            break;
        };
        let nal_end = find_start_code(bytes, start + 3).unwrap_or(bytes.len());
        let nal_data_start = if start + 3 < bytes.len() && bytes[start + 2] == 0x01 {
            start + 3
        } else if start + 4 < bytes.len() && bytes[start + 3] == 0x01 {
            start + 4
        } else {
            start + 3
        };
        if nal_data_start < nal_end {
            total += 4 + (nal_end - nal_data_start);
        }
        i = nal_end;
    }
    Some(total)
}

/// Append `data` to `buf` as length-prefixed NALs (Annex-B → AVCC).
/// Already length-prefixed payloads are copied verbatim. No allocation.
fn write_annex_b_avcc(buf: &mut BytesMut, data: &Bytes) {
    if annex_b_avcc_size(data).is_none() {
        buf.put_slice(data);
        return;
    }
    let bytes = data.as_ref();
    let mut i = 0;
    while i < bytes.len() {
        let Some(start) = find_start_code(bytes, i) else {
            break;
        };
        let nal_end = find_start_code(bytes, start + 3).unwrap_or(bytes.len());
        let nal_data_start = if start + 3 < bytes.len() && bytes[start + 2] == 0x01 {
            start + 3
        } else if start + 4 < bytes.len() && bytes[start + 3] == 0x01 {
            start + 4
        } else {
            start + 3
        };
        if nal_data_start < nal_end {
            buf.put_u32((nal_end - nal_data_start) as u32);
            buf.put_slice(&bytes[nal_data_start..nal_end]);
        }
        i = nal_end;
    }
}

/// H.264 video tag with Annex-B → AVCC conversion performed directly into
/// `buf`: the tag header carries the converted size, then the
/// length-prefixed NALs are written in place. Single copy, no intermediate
/// allocation (a ~2× reduction of the per-frame copy/alloc cost that
/// matters on low-end IPC CPUs). Length-prefixed payloads (RTMP ingest)
/// are copied as-is.
fn write_h264_video_tag_inplace(buf: &mut BytesMut, unit: &AccessUnit, data: &Bytes) {
    let converted_size = annex_b_avcc_size(data).unwrap_or(data.len());
    write_flv_tag_header(buf, unit, converted_size + 5);

    let frame_type = if unit.is_keyframe { 1u8 } else { 2u8 };
    buf.put_u8((frame_type << 4) | 7); // 7 = AVC
    buf.put_u8(1); // AVC packet type: NALU

    write_composition_time(buf, unit);

    write_annex_b_avcc(buf, data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + converted_size + 5;
    buf.put_u32(tag_total_size as u32);
}

/// Write an H.265 video FLV tag (Enhanced RTMP / Veovera format).
///
/// Format (Veovera Enhanced RTMP spec):
///   byte 0: IsExVideoHeader(1) | VideoFrameType(3) | VideoPacketType(4)
///     bit 7:   isExVideoHeader = 1
///     bits 4-6: FrameType (1=keyframe, 2=inter, 5=command)
///     bits 0-3: PacketType (0=SequenceStart, 1=CodedFrames, 3=CodedFramesX)
///   bytes 1-4: FourCC ("hvc1")
///   data: HVCC record (SequenceStart) or length-prefixed NALUs (CodedFramesX).
///
/// CodedFramesX omits CompositionTime.  FFmpeg's enhanced-FLV HEVC
/// demuxer treats CompositionTime as unsigned, which is incompatible with
/// the negative offsets produced by B-frames.
fn write_h265_video_tag(
    buf: &mut BytesMut,
    unit: &AccessUnit,
    data: &Bytes,
    is_sequence_header: bool,
) {
    let (frame_type, packet_type): (u8, u8) = if is_sequence_header {
        (1, 0) // keyframe + SequenceStart
    } else if unit.is_keyframe {
        (1, 3) // keyframe + CodedFramesX
    } else {
        (2, 3) // inter + CodedFramesX
    };

    let body_size = 1 + 4 + data.len(); // header + FourCC + payload

    write_flv_tag_header(buf, unit, body_size);

    // Enhanced RTMP header byte: PacketType is embedded in bits 0-3
    buf.put_u8(0x80 | (frame_type << 4) | packet_type);

    // FourCC: "hvc1"
    buf.put_slice(b"hvc1");

    // Data: HVCC (SequenceStart) or AVCC NALUs (CodedFramesX)
    buf.put_slice(data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + body_size;
    buf.put_u32(tag_total_size as u32);
}

/// H.265 video tag with Annex-B → length-prefixed conversion performed
/// directly into `buf` (see [`write_h264_video_tag_inplace`]).
fn write_h265_video_tag_inplace(buf: &mut BytesMut, unit: &AccessUnit, data: &Bytes) {
    let converted_size = annex_b_avcc_size(data).unwrap_or(data.len());
    let body_size = 1 + 4 + converted_size;

    write_flv_tag_header(buf, unit, body_size);

    let frame_type = if unit.is_keyframe { 1u8 } else { 2u8 };
    // Enhanced RTMP header byte: keyframe + CodedFramesX (no CompositionTime)
    buf.put_u8(0x80 | (frame_type << 4) | 3);

    buf.put_slice(b"hvc1");

    write_annex_b_avcc(buf, data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + body_size;
    buf.put_u32(tag_total_size as u32);
}

fn write_composition_time(buf: &mut BytesMut, unit: &AccessUnit) {
    let dts = unit.dts.unwrap_or(unit.pts);
    crate::media::flv::write_composition_time(
        buf,
        crate::media::flv::composition_time_ms(unit.pts, Some(dts)),
    );
}

/// Write the FLV video tag header.
fn write_flv_tag_header(buf: &mut BytesMut, unit: &AccessUnit, body_size: usize) {
    let timestamp = unit.dts.unwrap_or(unit.pts).as_millis() as u32;
    crate::media::flv::write_tag_header(
        buf,
        crate::media::flv::TAG_TYPE_VIDEO,
        body_size,
        timestamp,
    );
}

/// Write an audio FLV tag (AAC).
fn write_audio_tag(buf: &mut BytesMut, unit: &AccessUnit, data: &Bytes, is_sequence_header: bool) {
    let timestamp = unit.pts.as_millis() as u32;
    let data_size = data.len() + 2; // +2 for audio tag header bytes
    crate::media::flv::write_tag_header(
        buf,
        crate::media::flv::TAG_TYPE_AUDIO,
        data_size,
        timestamp,
    );

    // AAC: sound_format=10, rate=3 (44 kHz), size=1 (16-bit), type=1 (stereo).
    buf.put_u8(crate::media::flv::AAC_SOUND_HEADER);
    buf.put_u8(if is_sequence_header { 0 } else { 1 });
    buf.put_slice(data);

    let tag_total_size = crate::media::flv::FLV_TAG_HEADER_SIZE + data_size;
    buf.put_u32(tag_total_size as u32);
}

/// Write a G.711 audio FLV tag.
fn write_g711_audio_tag(buf: &mut BytesMut, unit: &AccessUnit) {
    let timestamp = unit.pts.as_millis() as u32;
    let data_size = unit.data.len() + 1; // +1 for audio tag header byte
    crate::media::flv::write_tag_header(
        buf,
        crate::media::flv::TAG_TYPE_AUDIO,
        data_size,
        timestamp,
    );

    let sound_format = match unit.codec {
        Codec::Pcma => crate::media::flv::SOUND_FORMAT_ALAW,
        Codec::Pcmu => crate::media::flv::SOUND_FORMAT_ULAW,
        _ => unreachable!(),
    };
    // Sound rate/size/type bits: 44kHz (3), 16-bit (1), stereo (1).
    buf.put_u8((sound_format << 4) | 0x0F);
    buf.put_slice(&unit.data);

    let tag_total_size = crate::media::flv::FLV_TAG_HEADER_SIZE + data_size;
    buf.put_u32(tag_total_size as u32);
}

/// Reference implementation kept for tests: Annex-B → AVCC conversion
/// returning a standalone buffer.
#[cfg(test)]
fn annex_b_to_avcc(data: &Bytes) -> Bytes {
    // Fast path: the payload is already AVCC/HVCC (length-prefixed), which
    // is what RTMP ingest delivers. Detecting it avoids a full-frame scan
    // and rebuild; the Bytes clone is a reference-count bump only.
    if data.len() >= 4
        && u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize == data.len() - 4
    {
        return data.clone();
    }
    let ranges = annex_b_nal_ranges(data);
    if ranges.is_empty() {
        // No start code — treat whole buffer as a single NAL.
        let mut buf = BytesMut::with_capacity(4 + data.len());
        buf.put_u32(data.len() as u32);
        buf.put_slice(data);
        return buf.freeze();
    }

    let total_size: usize = ranges.iter().map(|(start, end)| 4 + (end - start)).sum();
    let mut buf = BytesMut::with_capacity(total_size);
    for (start, end) in &ranges {
        buf.put_u32((end - start) as u32);
        buf.put_slice(&data[*start..*end]);
    }
    buf.freeze()
}

/// Locate Annex-B NAL boundaries as offset ranges into the buffer, without
/// copying the NAL data (test helper for the reference conversion).
#[cfg(test)]
fn annex_b_nal_ranges(data: &Bytes) -> Vec<(usize, usize)> {
    let bytes = data.as_ref();
    let mut nals = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let Some(start) = find_start_code(bytes, i) else {
            break;
        };
        let nal_end = find_start_code(bytes, start + 3).unwrap_or(bytes.len());

        let nal_data_start = if start + 3 < bytes.len() && bytes[start + 2] == 0x01 {
            start + 3
        } else if start + 4 < bytes.len() && bytes[start + 3] == 0x01 {
            start + 4
        } else {
            start + 3
        };

        if nal_data_start < nal_end {
            nals.push((nal_data_start, nal_end));
        }

        i = nal_end;
    }

    nals
}

/// Locate the next Annex-B start code (00 00 01 or 00 00 00 01) at or
/// after `start`.
fn find_start_code(data: &[u8], start: usize) -> Option<usize> {
    let len = data.len();
    if start + 2 >= len {
        return None;
    }
    for i in start..len - 2 {
        if data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                return Some(i);
            }
            if i + 3 < len && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some(i);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{MediaDescription, MediaKind, TrackDescription};
    use std::sync::Arc;
    use std::time::Duration;

    fn make_h264_description() -> MediaDescription {
        MediaDescription {
            session_name: Arc::from("test"),
            tracks: vec![
                TrackDescription {
                    id: 0,
                    kind: MediaKind::Video,
                    codec: Codec::H264,
                    clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
                    channels: None,
                    payload_type: 96,
                    fmtp: None,
                    control: Arc::from("trackID=0"),
                    width: None,
                    height: None,
                    frame_rate: None,
                    #[cfg(feature = "srtp")]
                    crypto: None,
                },
                TrackDescription {
                    id: 1,
                    kind: MediaKind::Audio,
                    codec: Codec::Aac,
                    clock_rate: 44100,
                    channels: Some(2),
                    payload_type: 97,
                    fmtp: None,
                    control: Arc::from("trackID=1"),
                    width: None,
                    height: None,
                    frame_rate: None,
                    #[cfg(feature = "srtp")]
                    crypto: None,
                },
            ],
            #[cfg(feature = "multicast")]
            connection: None,
        }
    }

    fn make_h265_description() -> MediaDescription {
        MediaDescription {
            session_name: Arc::from("test"),
            tracks: vec![TrackDescription {
                id: 0,
                kind: MediaKind::Video,
                codec: Codec::H265,
                clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
                channels: None,
                payload_type: 96,
                fmtp: None,
                control: Arc::from("trackID=0"),
                width: None,
                height: None,
                frame_rate: None,
                #[cfg(feature = "srtp")]
                crypto: None,
            }],
            #[cfg(feature = "multicast")]
            connection: None,
        }
    }

    #[test]
    fn flv_header_video_and_audio() {
        let muxer = FlvMuxer::new(&make_h264_description());
        let header = muxer.flv_header();
        assert_eq!(&header[..3], b"FLV");
        assert_eq!(header[4], 5); // video + audio flags
    }

    #[test]
    fn flv_header_audio_only() {
        let desc = MediaDescription {
            session_name: Arc::from("test"),
            tracks: vec![TrackDescription {
                id: 0,
                kind: MediaKind::Audio,
                codec: Codec::Aac,
                clock_rate: 44100,
                channels: Some(2),
                payload_type: 97,
                fmtp: None,
                control: Arc::from("trackID=0"),
                width: None,
                height: None,
                frame_rate: None,
                #[cfg(feature = "srtp")]
                crypto: None,
            }],
            #[cfg(feature = "multicast")]
            connection: None,
        };
        let muxer = FlvMuxer::new(&desc);
        let header = muxer.flv_header();
        assert_eq!(header[4], 4); // audio flag
    }

    #[test]
    fn avc_sequence_header_from_sps_pps() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x67, 0x42, 0x00, 0x0a, 0xe9, 0x40, 0x20]);
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x68, 0xce, 0x38, 0x80]);

        let bytes = Bytes::from(data);
        let seq = FlvMuxer::build_avc_sequence_header(&bytes).unwrap();
        assert_eq!(seq[0], 1);
        assert_eq!(seq[4], 0xff);
        assert_eq!(seq[5], 0xe1);
    }

    #[test]
    fn h265_enhanced_rtmp_tag() {
        let desc = make_h265_description();
        let mut muxer = FlvMuxer::new(&desc);

        // Minimal H.265 data with VPS/SPS/PPS.
        // SPS must be long enough for build_h265_sequence_header to parse
        // profile_tier_level (needs >=13 bytes after the 2-byte NAL header + 1-byte SPS header).
        let mut data = Vec::new();
        // VPS (nal_type=32, first byte = 0x40)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x40, 0x01]);
        // SPS (nal_type=33, first byte = 0x42)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        // NAL header: [0x42, 0x01], then SPS RBSP:
        //   [0]: sps_video_parameter_set_id(4)+sps_max_sub_layers_minus1(3)+sps_temporal_id_nesting_flag(1)
        //   [1..12]: profile_tier_level (12 bytes minimum)
        data.extend_from_slice(&[
            0x42, 0x01, // NAL header
            0x00, // sps params
            0x01, // general_profile_idc = 1 (Main)
            0x00, 0x00, 0x00, 0x00, // general_profile_compatibility_flags
            0x00, 0x00, 0x00, 0x00, 0x00, 0x60, // general_constraint_indicator_flags
            0x78, // general_level_idc = 120 (level 3.0)
        ]);
        // PPS (nal_type=34, first byte = 0x44)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x44, 0x01]);

        let bytes = Bytes::from(data);
        let seq = FlvMuxer::build_h265_sequence_header(&bytes);
        assert!(
            seq.is_some(),
            "should build H.265 sequence header from VPS/SPS/PPS"
        );

        let seq = seq.unwrap();
        muxer.set_video_sequence_header(seq);

        let unit = AccessUnit {
            track_id: 0,
            codec: Codec::H265,
            pts: Duration::from_millis(0),
            dts: None,
            rtp_timestamp: 0,
            is_keyframe: true,
            data: bytes,
        };

        let tags = muxer.encode(&unit);
        assert!(!tags.is_empty());
        // Find the first video tag and verify Enhanced RTMP format (Veovera spec)
        // Byte 0: IsExVideoHeader(1) | VideoFrameType(3) | VideoPacketType(4)
        let mut pos = 0;
        while pos + FLV_TAG_HEADER_SIZE + 5 < tags.len() {
            if tags[pos] == 0x09 {
                let body_start = pos + FLV_TAG_HEADER_SIZE;
                // Sequence header: IsEx=1, FrameType=1(keyframe), PacketType=0
                // = 0x80 | 0x10 | 0x00 = 0x90
                assert_eq!(
                    tags[body_start], 0x90,
                    "seq hdr: IsEx=1, FrameType=1, PacketType=0"
                );
                assert_eq!(&tags[body_start + 1..body_start + 5], b"hvc1");
                // No separate PacketType byte — it's embedded in byte 0
                break;
            }
            pos += 1;
        }
    }

    #[test]
    fn annex_b_to_avcc_conversion() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x65, 0x88, 0x00]);
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x41, 0x9a, 0x00]);

        let bytes = Bytes::from(data);
        let avcc = annex_b_to_avcc(&bytes);
        assert_eq!(avcc.len(), 14);
    }

    /// The in-place conversion used by `encode` must be byte-identical to
    /// the reference `annex_b_to_avcc`, including mixed 3/4-byte start
    /// codes and the already-length-prefixed fast path.
    #[test]
    fn inplace_conversion_matches_reference() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42]); // 4-byte start code
        data.extend_from_slice(&[0x00, 0x00, 0x01, 0x68]); // 3-byte start code
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80, 0x40]);
        let bytes = Bytes::from(data);

        let expected = annex_b_to_avcc(&bytes);
        let mut buf = BytesMut::new();
        write_annex_b_avcc(&mut buf, &bytes);
        assert_eq!(buf.freeze(), expected, "Annex-B in-place conversion");

        // Fast path: length-prefixed payload (single- or multi-NAL) must
        // pass through byte-identical.
        let mut buf = BytesMut::new();
        write_annex_b_avcc(&mut buf, &expected);
        assert_eq!(buf.freeze(), expected, "length-prefixed passthrough");
        assert!(annex_b_avcc_size(&expected).is_none());
        // Multi-NAL AVCC (RTMP ingest shape) is not mistaken for Annex-B.
        let mut multi = Vec::new();
        multi.extend_from_slice(&(3u32).to_be_bytes());
        multi.extend_from_slice(&[0x67, 0x42, 0x00]);
        multi.extend_from_slice(&(2u32).to_be_bytes());
        multi.extend_from_slice(&[0x65, 0x88]);
        let multi = Bytes::from(multi);
        assert!(
            annex_b_avcc_size(&multi).is_none(),
            "multi-NAL AVCC is AVCC"
        );
        let mut buf = BytesMut::new();
        write_annex_b_avcc(&mut buf, &multi);
        assert_eq!(buf.freeze(), multi, "multi-NAL AVCC passthrough");

        // An AVCC NAL whose length field is `00 00 01 XX` (256–511-byte
        // NAL, e.g. an AAC frame) looks like a 3-byte start code — it must
        // still be recognized as length-prefixed and pass through.
        let mut audio = Vec::new();
        audio.extend_from_slice(&0x0101u32.to_be_bytes()); // 257-byte NAL
        audio.extend_from_slice(&[0x21, 0x10, 0x00, 0x01]);
        audio.resize(4 + 257, 0xab);
        let audio = Bytes::from(audio);
        assert!(
            annex_b_avcc_size(&audio).is_none(),
            "00 00 01 XX AVCC NAL is not Annex-B"
        );
        let mut buf = BytesMut::new();
        write_annex_b_avcc(&mut buf, &audio);
        assert_eq!(buf.freeze(), audio, "00 00 01 XX AVCC passthrough");
    }

    /// `encode` of a multi-NAL Annex-B keyframe must produce a video tag
    /// whose body is exactly frame-type + packet-type + CTS + AVCC NALs.
    #[test]
    fn encode_h264_tag_payload_is_avcc() {
        let desc = MediaDescription {
            session_name: Arc::from("t"),
            tracks: vec![TrackDescription {
                id: 0,
                kind: MediaKind::Video,
                codec: Codec::H264,
                clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
                channels: None,
                payload_type: 96,
                fmtp: None,
                control: Arc::from("trackID=0"),
                #[cfg(feature = "srtp")]
                crypto: None,
                width: None,
                height: None,
                frame_rate: None,
            }],
            #[cfg(feature = "multicast")]
            connection: None,
        };
        let mut muxer = FlvMuxer::new(&desc);
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x0a]);
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x38, 0x80]);
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80]);
        let unit = AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(40),
            dts: None,
            rtp_timestamp: 3600,
            is_keyframe: true,
            data: Bytes::from(data),
        };
        if let Some(seq) = FlvMuxer::build_avc_sequence_header(&unit.data) {
            muxer.set_video_sequence_header(seq);
        }
        let tags = muxer.encode(&unit);
        // Walk the FLV tags and locate the coded-frames video tag.
        let mut pos = 0;
        let mut frame_body: Option<&[u8]> = None;
        while pos + 15 <= tags.len() {
            let tag_type = tags[pos];
            let body_size =
                u32::from_be_bytes([0, tags[pos + 1], tags[pos + 2], tags[pos + 3]]) as usize;
            let body = &tags[pos + 11..pos + 11 + body_size];
            if tag_type == 9 && body.get(1) == Some(&1) {
                frame_body = Some(body);
            }
            pos += 11 + body_size + 4; // header + body + previous-tag-size
        }
        let frame_body = frame_body.expect("coded-frames video tag");
        assert_eq!(frame_body[0] & 0x0f, 7, "codec id AVC");
        assert_eq!(frame_body[1], 1, "AVC packet type NALU");
        // Body after 5 bytes of video-tag header: length-prefixed NALs.
        assert_eq!(
            &frame_body[5..],
            annex_b_to_avcc(&unit.data).as_ref(),
            "frame payload is AVCC"
        );
    }
    /// G.711 audio tags must use the normalized PTS (0-based), not the raw
    /// unit PTS — a regression guard for the FLV timestamp base.
    #[test]
    fn g711_tag_pts_normalized_to_zero() {
        let desc = MediaDescription {
            session_name: Arc::from("t"),
            tracks: vec![TrackDescription {
                id: 0,
                kind: MediaKind::Audio,
                codec: Codec::Pcma,
                clock_rate: 8000,
                channels: Some(1),
                payload_type: 8,
                fmtp: None,
                control: Arc::from("trackID=0"),
                #[cfg(feature = "srtp")]
                crypto: None,
                width: None,
                height: None,
                frame_rate: None,
            }],
            #[cfg(feature = "multicast")]
            connection: None,
        };
        let mut muxer = FlvMuxer::new(&desc);
        let first = AccessUnit {
            track_id: 0,
            codec: Codec::Pcma,
            pts: Duration::from_millis(500),
            dts: None,
            rtp_timestamp: 4000,
            is_keyframe: true,
            data: Bytes::from_static(&[0x55; 40]),
        };
        let second = AccessUnit {
            pts: Duration::from_millis(520),
            rtp_timestamp: 4160,
            ..first.clone()
        };
        let tag1 = muxer.encode(&first);
        let tag2 = muxer.encode(&second);
        // FLV tag timestamp: 3 bytes + 1 extension byte at offset 4..8.
        let ts =
            |tag: &Bytes| (u32::from(tag[4]) << 16) | (u32::from(tag[5]) << 8) | u32::from(tag[6]);
        assert_eq!(ts(&tag1), 0, "first tag timestamp normalized to 0");
        assert_eq!(ts(&tag2), 20, "second tag timestamp is base-relative (ms)");
        // Sound format bits: A-law (7) << 4 | 0x0F.
        assert_eq!(tag1[11] >> 4, 7, "sound format A-law");
    }
}
