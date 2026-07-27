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
                let avcc_data = annex_b_to_avcc(&normalized.data);
                write_h264_video_tag(&mut self.tag_buffer, &normalized, &avcc_data, false);
            }
            Codec::H265 => {
                if !self.video_sequence_header_sent
                    && let Some(ref seq) = self.video_sequence_header
                {
                    write_h265_video_tag(&mut self.tag_buffer, &normalized, seq, true);
                    self.video_sequence_header_sent = true;
                }
                let hvcc_data = annex_b_to_avcc(&normalized.data);
                write_h265_video_tag(&mut self.tag_buffer, &normalized, &hvcc_data, false);
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
                write_g711_audio_tag(&mut self.tag_buffer, unit);
            }
            Codec::Opus | Codec::Vp8 | Codec::Vp9 => {
                // FLV does not support these codecs; skip silently.
            }
        }

        self.tag_buffer.split().freeze()
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

    /// Build an AVCDecoderConfigurationRecord from Annex-B H.264 data (SPS + PPS).
    pub fn build_avc_sequence_header(annex_b_data: &Bytes) -> Option<Bytes> {
        let nals = parse_annex_b_nals(annex_b_data);
        let mut sps = None;
        let mut pps = None;
        for nal in &nals {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1f {
                7 => sps = Some(nal.clone()),
                8 => pps = Some(nal.clone()),
                _ => {}
            }
        }
        let sps = sps?;
        let pps = pps?;

        let mut buf = BytesMut::new();
        // AVCDecoderConfigurationRecord
        buf.put_u8(1); // configurationVersion
        buf.put_u8(sps[1]); // AVCProfileIndication
        buf.put_u8(sps[2]); // profile_compatibility
        buf.put_u8(sps[3]); // AVCLevelIndication
        buf.put_u8(0xff); // lengthSizeMinus1 = 3 (4 bytes)
        buf.put_u8(0xe1); // numOfSequenceParameterSets = 1
        buf.put_u16(sps.len() as u16); // SPS length
        buf.put_slice(&sps);
        buf.put_u8(1); // numOfPictureParameterSets = 1
        buf.put_u16(pps.len() as u16); // PPS length
        buf.put_slice(&pps);
        Some(buf.freeze())
    }

    /// Build a HEVCDecoderConfigurationRecord from Annex-B H.265 data (VPS + SPS + PPS).
    pub fn build_h265_sequence_header(annex_b_data: &Bytes) -> Option<Bytes> {
        let nals = parse_annex_b_nals(annex_b_data);
        let mut vps = None;
        let mut sps = None;
        let mut pps = None;
        for nal in &nals {
            if nal.len() < 2 {
                continue;
            }
            // H.265 NAL unit type is in bits [1..6] of the first byte
            let nal_type = (nal[0] >> 1) & 0x3f;
            match nal_type {
                32 => vps = Some(nal.clone()), // VPS
                33 => sps = Some(nal.clone()), // SPS
                34 => pps = Some(nal.clone()), // PPS
                _ => {}
            }
        }
        let vps = vps?;
        let sps = sps?;
        let pps = pps?;

        // Extract profile_tier_level fields from SPS RBSP.
        // After the 2-byte NAL header, SPS RBSP starts with:
        //   sps_video_parameter_set_id (4 bits)
        //   sps_max_sub_layers_minus1 (3 bits)
        //   sps_temporal_id_nesting_flag (1 bit)
        // Then profile_tier_level() begins after that first RBSP byte.
        // profile_tier_level layout:
        //   [0]  general_profile_space(2) + general_tier_flag(1) + general_profile_idc(5)
        //   [1..4] general_profile_compatibility_flags (4 bytes)
        //   [5..10] general_constraint_indicator_flags (6 bytes)
        //   [11] general_level_idc

        let p = if sps.len() > 14 {
            &sps[2..]
        } else {
            return None;
        };

        // p[0] is the SPS header (vps_id, max_sub_layers_minus1 and
        // temporal_id_nesting_flag), not part of profile_tier_level().
        let general_profile_space_tier_idc = p[1];
        let general_profile_compatibility_flags = [p[2], p[3], p[4], p[5]];
        let general_constraint_indicator_flags = [p[6], p[7], p[8], p[9], p[10], p[11]];
        let general_level_idc = p[12];

        let mut buf = BytesMut::with_capacity(128);
        buf.put_u8(1); // configurationVersion
        buf.put_u8(general_profile_space_tier_idc);
        buf.put_slice(&general_profile_compatibility_flags);
        buf.put_slice(&general_constraint_indicator_flags);
        buf.put_u8(general_level_idc);
        buf.put_u16(0xf000); // min_spatial_segmentation_idc (reserved)
        buf.put_u8(0xfc); // parallelismType (reserved)
        buf.put_u8(0xfd); // chromaFormatIdc = 1 (4:2:0)
        buf.put_u8(0xf8); // bitDepthLumaMinus8 = 0
        buf.put_u8(0xf8); // bitDepthChromaMinus8 = 0
        buf.put_u16(0); // avgFrameRate
        buf.put_u8(0x0f); // constantFrameRate(0) + numTemporalLayers(1) + temporalIdNested(1) + lengthSizeMinus1(3)
        buf.put_u8(3); // numOfArrays (VPS + SPS + PPS)

        // VPS array: array_completeness=1, reserved=1, NAL_unit_type=32
        buf.put_u8(0xa0); // 0b10100000 = complete(1) + reserved(1) + type(32)
        buf.put_u16(1);
        buf.put_u16(vps.len() as u16);
        buf.put_slice(&vps);

        // SPS array: array_completeness=1, reserved=1, NAL_unit_type=33
        buf.put_u8(0xa1); // 0b10100001
        buf.put_u16(1);
        buf.put_u16(sps.len() as u16);
        buf.put_slice(&sps);

        // PPS array: array_completeness=1, reserved=1, NAL_unit_type=34
        buf.put_u8(0xa2); // 0b10100010
        buf.put_u16(1);
        buf.put_u16(pps.len() as u16);
        buf.put_slice(&pps);

        Some(buf.freeze())
    }

    /// Build an AVCDecoderConfigurationRecord from pre-extracted SPS/PPS NALs.
    pub fn build_avc_sequence_header_from_nals(sps: &[u8], pps: &[u8]) -> Option<Bytes> {
        if sps.len() < 4 || pps.is_empty() {
            return None;
        }
        let mut buf = BytesMut::new();
        buf.put_u8(1);
        buf.put_u8(sps[1]);
        buf.put_u8(sps[2]);
        buf.put_u8(sps[3]);
        buf.put_u8(0xff);
        buf.put_u8(0xe1);
        buf.put_u16(sps.len() as u16);
        buf.put_slice(sps);
        buf.put_u8(1);
        buf.put_u16(pps.len() as u16);
        buf.put_slice(pps);
        Some(buf.freeze())
    }

    /// Build a HEVCDecoderConfigurationRecord from pre-extracted VPS/SPS/PPS NALs.
    pub fn build_h265_sequence_header_from_nals(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
    ) -> Option<Bytes> {
        if vps.len() < 2 || sps.len() <= 14 || pps.len() < 2 {
            return None;
        }
        let p = &sps[2..];
        let mut buf = BytesMut::with_capacity(128);
        buf.put_u8(1);
        buf.put_u8(p[1]);
        buf.put_slice(&p[2..6]);
        buf.put_slice(&p[6..12]);
        buf.put_u8(p[12]);
        buf.put_u16(0xf000);
        buf.put_u8(0xfc);
        buf.put_u8(0xfd);
        buf.put_u8(0xf8);
        buf.put_u8(0xf8);
        buf.put_u16(0);
        buf.put_u8(0x0f);
        buf.put_u8(3);
        buf.put_u8(0xa0);
        buf.put_u16(1);
        buf.put_u16(vps.len() as u16);
        buf.put_slice(vps);
        buf.put_u8(0xa1);
        buf.put_u16(1);
        buf.put_u16(sps.len() as u16);
        buf.put_slice(sps);
        buf.put_u8(0xa2);
        buf.put_u16(1);
        buf.put_u16(pps.len() as u16);
        buf.put_slice(pps);
        Some(buf.freeze())
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

fn write_composition_time(buf: &mut BytesMut, unit: &AccessUnit) {
    let dts = unit.dts.unwrap_or(unit.pts);
    let offset_ms = if unit.pts >= dts {
        unit.pts.saturating_sub(dts).as_millis() as i64
    } else {
        -(dts.saturating_sub(unit.pts).as_millis() as i64)
    }
    .clamp(-0x80_0000, 0x7f_ffff) as i32;
    let encoded = (offset_ms as u32) & 0x00ff_ffff;
    buf.put_u8((encoded >> 16) as u8);
    buf.put_u8((encoded >> 8) as u8);
    buf.put_u8(encoded as u8);
}

/// Write the FLV tag header (common for all tag types).
fn write_flv_tag_header(buf: &mut BytesMut, unit: &AccessUnit, body_size: usize) {
    let timestamp = unit.dts.unwrap_or(unit.pts).as_millis() as u32;
    let timestamp_ext = (timestamp >> 24) as u8;
    let timestamp_base = timestamp & 0x00FFFFFF;

    buf.put_u8(0x09); // tag type: video
    buf.put_u8((body_size >> 16) as u8);
    buf.put_u8((body_size >> 8) as u8);
    buf.put_u8(body_size as u8);
    buf.put_u8((timestamp_base >> 16) as u8);
    buf.put_u8((timestamp_base >> 8) as u8);
    buf.put_u8(timestamp_base as u8);
    buf.put_u8(timestamp_ext);
    buf.put_u8(0); // stream id (always 0)
    buf.put_u8(0);
    buf.put_u8(0);
}

/// Write an audio FLV tag (AAC).
fn write_audio_tag(buf: &mut BytesMut, unit: &AccessUnit, data: &Bytes, is_sequence_header: bool) {
    let timestamp = unit.pts.as_millis() as u32;
    let timestamp_ext = (timestamp >> 24) as u8;
    let timestamp_base = timestamp & 0x00FFFFFF;

    let data_size = data.len() + 2; // +2 for audio tag header bytes

    // FLV tag header
    buf.put_u8(0x08); // tag type: audio
    buf.put_u8((data_size >> 16) as u8);
    buf.put_u8((data_size >> 8) as u8);
    buf.put_u8(data_size as u8);
    buf.put_u8((timestamp_base >> 16) as u8);
    buf.put_u8((timestamp_base >> 8) as u8);
    buf.put_u8(timestamp_base as u8);
    buf.put_u8(timestamp_ext);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);

    // AAC: sound_format=10, rate=3(44kHz), size=1(16-bit), type=1(stereo)
    buf.put_u8(0xAF);

    buf.put_u8(if is_sequence_header { 0 } else { 1 });

    buf.put_slice(data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + data_size;
    buf.put_u32(tag_total_size as u32);
}

/// Write a G.711 audio FLV tag.
fn write_g711_audio_tag(buf: &mut BytesMut, unit: &AccessUnit) {
    let timestamp = unit.pts.as_millis() as u32;
    let timestamp_ext = (timestamp >> 24) as u8;
    let timestamp_base = timestamp & 0x00FFFFFF;

    let data_size = unit.data.len() + 1; // +1 for audio tag header byte

    // FLV tag header
    buf.put_u8(0x08);
    buf.put_u8((data_size >> 16) as u8);
    buf.put_u8((data_size >> 8) as u8);
    buf.put_u8(data_size as u8);
    buf.put_u8((timestamp_base >> 16) as u8);
    buf.put_u8((timestamp_base >> 8) as u8);
    buf.put_u8(timestamp_base as u8);
    buf.put_u8(timestamp_ext);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);

    let sound_format = match unit.codec {
        Codec::Pcma => 7u8,
        Codec::Pcmu => 8u8,
        _ => unreachable!(),
    };
    buf.put_u8(sound_format << 4);

    buf.put_slice(&unit.data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + data_size;
    buf.put_u32(tag_total_size as u32);
}

/// Convert Annex-B NALUs (with start codes) to AVCC/HVCC format (4-byte length prefix).
fn annex_b_to_avcc(data: &Bytes) -> Bytes {
    let nals = parse_annex_b_nals(data);
    if nals.is_empty() {
        // No start code — treat whole buffer as a single NAL.
        let mut buf = BytesMut::with_capacity(4 + data.len());
        buf.put_u32(data.len() as u32);
        buf.put_slice(data);
        return buf.freeze();
    }

    let total_size: usize = nals.iter().map(|n| 4 + n.len()).sum();
    let mut buf = BytesMut::with_capacity(total_size);
    for nal in &nals {
        buf.put_u32(nal.len() as u32);
        buf.put_slice(nal);
    }
    buf.freeze()
}

/// Parse Annex-B byte stream into individual NAL units.
fn parse_annex_b_nals(data: &Bytes) -> Vec<Vec<u8>> {
    let bytes = data.as_ref();
    let mut nals = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let start = find_start_code(bytes, i);
        if start.is_none() {
            break;
        }
        let nal_start = start.unwrap();

        let nal_end = find_start_code(bytes, nal_start + 3).unwrap_or(bytes.len());

        let nal_data_start = if nal_start + 3 < bytes.len() && bytes[nal_start + 2] == 0x01 {
            nal_start + 3
        } else if nal_start + 4 < bytes.len() && bytes[nal_start + 3] == 0x01 {
            nal_start + 4
        } else {
            nal_start + 3
        };

        if nal_data_start < nal_end {
            nals.push(bytes[nal_data_start..nal_end].to_vec());
        }

        i = nal_end;
    }

    nals
}

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
                    clock_rate: 90000,
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
                clock_rate: 90000,
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
}
