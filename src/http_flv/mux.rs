use bytes::{BufMut, Bytes, BytesMut};

use crate::media::{AccessUnit, Codec, MediaDescription};

const FLV_HEADER_SIZE: usize = 9;
const FLV_TAG_HEADER_SIZE: usize = 11;

pub struct FlvMuxer {
    has_video: bool,
    has_audio: bool,
    video_codec: Codec,
    video_sequence_header_sent: bool,
    audio_sequence_header_sent: bool,
    tag_buffer: BytesMut,
    /// Cached video decoder configuration record (sequence header).
    video_sequence_header: Option<Bytes>,
    /// Cached AAC audio specific config (sequence header).
    aac_sequence_header: Option<Bytes>,
}

impl FlvMuxer {
    pub fn new(description: &MediaDescription) -> Self {
        let video_track = description.tracks.iter().find(|t| t.kind == crate::media::MediaKind::Video);
        let has_video = video_track.is_some();
        let video_codec = video_track.map(|t| t.codec).unwrap_or(Codec::H264);
        let has_audio = description.tracks.iter().any(|t| t.kind == crate::media::MediaKind::Audio);
        Self {
            has_video,
            has_audio,
            video_codec,
            video_sequence_header_sent: false,
            audio_sequence_header_sent: false,
            tag_buffer: BytesMut::with_capacity(4096),
            video_sequence_header: None,
            aac_sequence_header: None,
        }
    }

    /// Write FLV file header (9 bytes + 4 bytes previous tag size 0).
    pub fn flv_header(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(FLV_HEADER_SIZE + 4);
        buf.put_slice(b"FLV");         // signature
        buf.put_u8(1);                 // version
        let flags = (if self.has_video { 1u8 } else { 0 })
            | (if self.has_audio { 4u8 } else { 0 });
        buf.put_u8(flags);             // type flags
        buf.put_u32(FLV_HEADER_SIZE as u32); // data offset
        buf.put_u32(0);                // previous tag size 0
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

    /// Encode an AccessUnit into FLV tags.
    /// Returns one or more FLV tags (may include sequence headers).
    pub fn encode(&mut self, unit: &AccessUnit) -> Bytes {
        self.tag_buffer.clear();

        match unit.codec {
            Codec::H264 => {
                if !self.video_sequence_header_sent {
                    if let Some(ref seq) = self.video_sequence_header {
                        write_h264_video_tag(&mut self.tag_buffer, unit, seq, true);
                        self.video_sequence_header_sent = true;
                    }
                }
                let avcc_data = annex_b_to_avcc(&unit.data);
                write_h264_video_tag(&mut self.tag_buffer, unit, &avcc_data, false);
            }
            Codec::H265 => {
                if !self.video_sequence_header_sent {
                    if let Some(ref seq) = self.video_sequence_header {
                        write_h265_video_tag(&mut self.tag_buffer, unit, seq, true);
                        self.video_sequence_header_sent = true;
                    }
                }
                let hvcc_data = annex_b_to_avcc(&unit.data);
                write_h265_video_tag(&mut self.tag_buffer, unit, &hvcc_data, false);
            }
            Codec::Aac => {
                if !self.audio_sequence_header_sent {
                    if let Some(ref seq) = self.aac_sequence_header {
                        write_audio_tag(&mut self.tag_buffer, unit, seq, true);
                        self.audio_sequence_header_sent = true;
                    }
                }
                write_audio_tag(&mut self.tag_buffer, unit, &unit.data, false);
            }
            Codec::Pcma | Codec::Pcmu => {
                write_g711_audio_tag(&mut self.tag_buffer, unit);
            }
        }

        self.tag_buffer.split().freeze()
    }

    /// Build an AVCDecoderConfigurationRecord from Annex-B H.264 data (SPS + PPS).
    pub fn build_avc_sequence_header(annex_b_data: &Bytes) -> Option<Bytes> {
        let nals = parse_annex_b_nals(annex_b_data);
        let mut sps = None;
        let mut pps = None;
        for nal in &nals {
            if nal.is_empty() { continue; }
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
        buf.put_u8(1);                     // configurationVersion
        buf.put_u8(sps[1]);                // AVCProfileIndication
        buf.put_u8(sps[2]);                // profile_compatibility
        buf.put_u8(sps[3]);                // AVCLevelIndication
        buf.put_u8(0xff);                  // lengthSizeMinus1 = 3 (4 bytes)
        buf.put_u8(0xe1);                  // numOfSequenceParameterSets = 1
        buf.put_u16(sps.len() as u16);     // SPS length
        buf.put_slice(&sps);
        buf.put_u8(1);                     // numOfPictureParameterSets = 1
        buf.put_u16(pps.len() as u16);     // PPS length
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
            if nal.len() < 2 { continue; }
            // H.265 NAL unit type is in bits [1..6] of the first byte
            let nal_type = (nal[0] >> 1) & 0x3f;
            match nal_type {
                32 => vps = Some(nal.clone()),  // VPS
                33 => sps = Some(nal.clone()),  // SPS
                34 => pps = Some(nal.clone()),  // PPS
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
        // Then profile_tier_level() begins at offset 2.
        // profile_tier_level layout:
        //   [0]  general_profile_space(2) + general_tier_flag(1) + general_profile_idc(5)
        //   [1..4] general_profile_compatibility_flags (4 bytes)
        //   [5..10] general_constraint_indicator_flags (6 bytes)
        //   [11] general_level_idc

        let p = if sps.len() > 12 { &sps[2..] } else { return None };

        let general_profile_space_tier_idc = p[0];
        let general_profile_compatibility_flags = [p[1], p[2], p[3], p[4]];
        let general_constraint_indicator_flags = [p[5], p[6], p[7], p[8], p[9], p[10]];
        let general_level_idc = p[11];

        let mut buf = BytesMut::with_capacity(128);
        buf.put_u8(1);                       // configurationVersion
        buf.put_u8(general_profile_space_tier_idc);
        buf.put_slice(&general_profile_compatibility_flags);
        buf.put_slice(&general_constraint_indicator_flags);
        buf.put_u8(general_level_idc);
        buf.put_u16(0xf000);                 // min_spatial_segmentation_idc (reserved)
        buf.put_u8(0xfc);                    // parallelismType (reserved)
        buf.put_u8(0xfd);                    // chromaFormatIdc = 1 (4:2:0)
        buf.put_u8(0xf8);                    // bitDepthLumaMinus8 = 0
        buf.put_u8(0xf8);                    // bitDepthChromaMinus8 = 0
        buf.put_u16(0);                      // avgFrameRate
        buf.put_u8(0x0f);                    // constantFrameRate(0) + numTemporalLayers(1) + temporalIdNested(1) + lengthSizeMinus1(3)
        buf.put_u8(3);                       // numOfArrays (VPS + SPS + PPS)

        // VPS array
        buf.put_u8(0x20); // NAL_unit_type = 32 (VPS)
        buf.put_u16(1);
        buf.put_u16(vps.len() as u16);
        buf.put_slice(&vps);

        // SPS array
        buf.put_u8(0x21); // NAL_unit_type = 33 (SPS)
        buf.put_u16(1);
        buf.put_u16(sps.len() as u16);
        buf.put_slice(&sps);

        // PPS array
        buf.put_u8(0x22); // NAL_unit_type = 34 (PPS)
        buf.put_u16(1);
        buf.put_u16(pps.len() as u16);
        buf.put_slice(&pps);

        Some(buf.freeze())
    }
}

/// Write an H.264 video FLV tag (traditional format: codec_id = 7).
fn write_h264_video_tag(buf: &mut BytesMut, unit: &AccessUnit, data: &Bytes, is_sequence_header: bool) {
    write_flv_tag_header(buf, unit, data.len() + 5);

    // Frame type (4 bits) + Codec ID (4 bits)
    let frame_type = if unit.is_keyframe { 1u8 } else { 2u8 };
    buf.put_u8((frame_type << 4) | 7); // 7 = AVC

    // AVC packet type
    buf.put_u8(if is_sequence_header { 0 } else { 1 });

    // Composition time offset
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);

    buf.put_slice(data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + data.len() + 5;
    buf.put_u32(tag_total_size as u32);
}

/// Write an H.265 video FLV tag (Enhanced RTMP format).
fn write_h265_video_tag(buf: &mut BytesMut, unit: &AccessUnit, data: &Bytes, is_sequence_header: bool) {
    // Enhanced RTMP video tag body:
    //   byte 0: 0b1XYY0000
    //     bit 7: isExVideoHeader = 1
    //     bits 4-6: X = FrameType (0=sequence header, 1=keyframe, 2=inter)
    //   bytes 1-4: FourCC ("hvc1")
    //   byte 5: PacketType (0=SequenceStart, 1=CodedFrames)
    //   [if PacketType=1]: 3 bytes CompositionTime
    //   data: HVCC record (seq hdr) or length-prefixed NALUs (coded frames)

    let (frame_type, packet_type): (u8, u8) = if is_sequence_header {
        (0, 0) // FrameType=0 for sequence header in Enhanced RTMP
    } else if unit.is_keyframe {
        (1, 1)
    } else {
        (2, 1)
    };

    let body_size = if is_sequence_header {
        1 + 4 + 1 + data.len()
    } else {
        1 + 4 + 1 + 3 + data.len() // +3 for CompositionTime
    };

    write_flv_tag_header(buf, unit, body_size);

    // Enhanced RTMP header byte: isExVideoHeader=1 + frameType in bits 4-6
    buf.put_u8(0x80 | (frame_type << 4));

    // FourCC: "hvc1"
    buf.put_slice(b"hvc1");

    // Packet type
    buf.put_u8(packet_type);

    // CompositionTime (3 bytes, SI24, big-endian) — only for coded frames
    if !is_sequence_header {
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u8(0);
    }

    // Data
    buf.put_slice(data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + body_size;
    buf.put_u32(tag_total_size as u32);
}

/// Write the FLV tag header (common for all tag types).
fn write_flv_tag_header(buf: &mut BytesMut, unit: &AccessUnit, body_size: usize) {
    let timestamp = unit.pts.as_millis() as u32;
    let timestamp_ext = (timestamp >> 24) as u8;
    let timestamp_base = timestamp & 0x00FFFFFF;

    buf.put_u8(0x09);                    // tag type: video
    buf.put_u8((body_size >> 16) as u8);
    buf.put_u8((body_size >> 8) as u8);
    buf.put_u8(body_size as u8);
    buf.put_u8(timestamp_base as u8);
    buf.put_u8((timestamp_base >> 8) as u8);
    buf.put_u8((timestamp_base >> 16) as u8);
    buf.put_u8(timestamp_ext);
    buf.put_u8(0);                       // stream id (always 0)
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
    buf.put_u8(0x08);                    // tag type: audio
    buf.put_u8((data_size >> 16) as u8);
    buf.put_u8((data_size >> 8) as u8);
    buf.put_u8(data_size as u8);
    buf.put_u8(timestamp_base as u8);
    buf.put_u8((timestamp_base >> 8) as u8);
    buf.put_u8((timestamp_base >> 16) as u8);
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
    buf.put_u8(timestamp_base as u8);
    buf.put_u8((timestamp_base >> 8) as u8);
    buf.put_u8((timestamp_base >> 16) as u8);
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
        return data.clone();
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

        let nal_data_start = if nal_start + 3 < bytes.len()
            && bytes[nal_start + 2] == 0x01
        {
            nal_start + 3
        } else if nal_start + 4 < bytes.len()
            && bytes[nal_start + 3] == 0x01
        {
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
            tracks: vec![
                TrackDescription {
                    id: 0,
                    kind: MediaKind::Video,
                    codec: Codec::H265,
                    clock_rate: 90000,
                    channels: None,
                    payload_type: 96,
                    fmtp: None,
                    control: Arc::from("trackID=0"),
                    #[cfg(feature = "srtp")]
                    crypto: None,
                },
            ],
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
                id: 0, kind: MediaKind::Audio, codec: Codec::Aac,
                clock_rate: 44100, channels: Some(2), payload_type: 97,
                fmtp: None, control: Arc::from("trackID=0"),
                #[cfg(feature = "srtp")] crypto: None,
            }],
            #[cfg(feature = "multicast")] connection: None,
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

        // Minimal H.265 data with VPS/SPS/PPS
        let mut data = Vec::new();
        // VPS (nal_type=32, byte = 0x40)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x40, 0x01]);
        // SPS (nal_type=33, byte = 0x42)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x42, 0x01]);
        // PPS (nal_type=34, byte = 0x44)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x44, 0x01]);

        let bytes = Bytes::from(data);
        let seq = FlvMuxer::build_h265_sequence_header(&bytes);
        assert!(seq.is_some(), "should build H.265 sequence header from VPS/SPS/PPS");

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
        // Find the first video tag and verify Enhanced RTMP format
        let mut pos = 0;
        while pos + FLV_TAG_HEADER_SIZE + 6 < tags.len() {
            if tags[pos] == 0x09 {
                let body_start = pos + FLV_TAG_HEADER_SIZE;
                // Enhanced RTMP header byte: 0x80 | (frame_type << 4)
                // Sequence header: frame_type=0, so byte = 0x80
                assert_eq!(tags[body_start], 0x80, "seq hdr: enhanced flag + frame_type=0");
                assert_eq!(&tags[body_start + 1..body_start + 5], b"hvc1");
                assert_eq!(tags[body_start + 5], 0, "PacketType=0 for SequenceStart");
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
