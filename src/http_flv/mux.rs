use bytes::{BufMut, Bytes, BytesMut};

use crate::media::{AccessUnit, Codec, MediaDescription};

const FLV_HEADER_SIZE: usize = 9;
const FLV_TAG_HEADER_SIZE: usize = 11;

pub struct FlvMuxer {
    has_video: bool,
    has_audio: bool,
    sequence_header_sent: bool,
    audio_sequence_header_sent: bool,
    tag_buffer: BytesMut,
    /// Cached AVC decoder configuration record (sequence header).
    avc_sequence_header: Option<Bytes>,
    /// Cached AAC audio specific config (sequence header).
    aac_sequence_header: Option<Bytes>,
}

impl FlvMuxer {
    pub fn new(description: &MediaDescription) -> Self {
        let has_video = description.tracks.iter().any(|t| t.kind == crate::media::MediaKind::Video);
        let has_audio = description.tracks.iter().any(|t| t.kind == crate::media::MediaKind::Audio);
        Self {
            has_video,
            has_audio,
            sequence_header_sent: false,
            audio_sequence_header_sent: false,
            tag_buffer: BytesMut::with_capacity(4096),
            avc_sequence_header: None,
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

    /// Set the AVC sequence header (AVCDecoderConfigurationRecord).
    pub fn set_avc_sequence_header(&mut self, data: Bytes) {
        self.avc_sequence_header = Some(data);
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
            Codec::H264 | Codec::H265 => {
                if !self.sequence_header_sent {
                    if let Some(ref seq) = self.avc_sequence_header {
                        // Write video sequence header tag
                        write_video_tag(&mut self.tag_buffer, unit, seq, true);
                        self.sequence_header_sent = true;
                    }
                }
                // Write video data tag
                let avcc_data = annex_b_to_avcc(&unit.data);
                write_video_tag(&mut self.tag_buffer, unit, &avcc_data, false);
            }
            Codec::Aac => {
                if !self.audio_sequence_header_sent {
                    if let Some(ref seq) = self.aac_sequence_header {
                        // Write audio sequence header tag
                        write_audio_tag(&mut self.tag_buffer, unit, seq, true);
                        self.audio_sequence_header_sent = true;
                    }
                }
                // Write audio data tag
                write_audio_tag(&mut self.tag_buffer, unit, &unit.data, false);
            }
            Codec::Pcma | Codec::Pcmu => {
                write_g711_audio_tag(&mut self.tag_buffer, unit);
            }
        }

        self.tag_buffer.split().freeze()
    }

    /// Generate a video sequence header from SPS/PPS in Annex-B data.
    pub fn build_avc_sequence_header(annex_b_data: &Bytes) -> Option<Bytes> {
        let nals = parse_annex_b_nals(annex_b_data);
        let mut sps = None;
        let mut pps = None;
        for nal in &nals {
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
}

/// Write a video FLV tag.
fn write_video_tag(buf: &mut BytesMut, unit: &AccessUnit, data: &Bytes, is_sequence_header: bool) {
    let timestamp = unit.pts.as_millis() as u32;
    let timestamp_ext = (timestamp >> 24) as u8;
    let timestamp_base = timestamp & 0x00FFFFFF;

    // FLV tag header (11 bytes)
    buf.put_u8(0x09);                    // tag type: video
    let data_size = data.len() + 5;      // +5 for video tag header bytes
    buf.put_u8((data_size >> 16) as u8);
    buf.put_u8((data_size >> 8) as u8);
    buf.put_u8(data_size as u8);
    buf.put_u8(timestamp_base as u8);
    buf.put_u8((timestamp_base >> 8) as u8);
    buf.put_u8((timestamp_base >> 16) as u8);
    buf.put_u8(timestamp_ext);
    buf.put_u8(0);                       // stream id (always 0)
    buf.put_u8(0);
    buf.put_u8(0);

    // Video tag body
    // Frame type (4 bits) + Codec ID (4 bits)
    let frame_type = if unit.is_keyframe { 1u8 } else { 2u8 }; // 1=keyframe, 2=inter
    let codec_id = 7u8; // AVC
    buf.put_u8((frame_type << 4) | codec_id);

    // AVC packet type
    if is_sequence_header {
        buf.put_u8(0); // AVC sequence header
    } else {
        buf.put_u8(1); // AVC NALU
    }

    // Composition time offset (0 for simplicity)
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);

    // Data
    buf.put_slice(data);

    // Previous tag size
    let tag_total_size = FLV_TAG_HEADER_SIZE + data_size;
    buf.put_u32(tag_total_size as u32);
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

    // Audio tag body
    // Sound format (4 bits) + Rate (2 bits) + Size (1 bit) + Type (1 bit)
    // AAC = 10, Rate=3 (44kHz), Size=1 (16-bit), Type=1 (stereo)
    buf.put_u8(0xAF); // 1010 1111

    // AAC packet type
    if is_sequence_header {
        buf.put_u8(0); // AAC sequence header
    } else {
        buf.put_u8(1); // AAC raw
    }

    buf.put_slice(data);

    // Previous tag size
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

    // Sound format: PCMA=7, PCMU=8; Rate=0(5.5kHz mono), Size=0(8-bit), Type=0(mono)
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

/// Convert Annex-B NALUs (with start codes) to AVCC format (4-byte length prefix).
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
        // Find start code (0x00 0x00 0x01 or 0x00 0x00 0x00 0x01)
        let start = find_start_code(bytes, i);
        if start.is_none() {
            break;
        }
        let nal_start = start.unwrap();

        // Find next start code or end
        let nal_end = find_start_code(bytes, nal_start + 3).unwrap_or(bytes.len());

        // Skip the start code prefix
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

    fn make_description(has_video: bool, has_audio: bool) -> MediaDescription {
        let mut tracks = Vec::new();
        if has_video {
            tracks.push(TrackDescription {
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
            });
        }
        if has_audio {
            tracks.push(TrackDescription {
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
            });
        }
        MediaDescription {
            session_name: Arc::from("test"),
            tracks,
            #[cfg(feature = "multicast")]
            connection: None,
        }
    }

    #[test]
    fn flv_header_video_only() {
        let muxer = FlvMuxer::new(&make_description(true, false));
        let header = muxer.flv_header();
        assert_eq!(&header[..3], b"FLV");
        assert_eq!(header[3], 1); // version
        assert_eq!(header[4], 1); // video flag
    }

    #[test]
    fn flv_header_audio_only() {
        let muxer = FlvMuxer::new(&make_description(false, true));
        let header = muxer.flv_header();
        assert_eq!(header[4], 4); // audio flag
    }

    #[test]
    fn flv_header_both() {
        let muxer = FlvMuxer::new(&make_description(true, true));
        let header = muxer.flv_header();
        assert_eq!(header[4], 5); // video + audio flags
    }

    #[test]
    fn avc_sequence_header_from_sps_pps() {
        // Minimal Annex-B with SPS and PPS
        let mut data = Vec::new();
        // SPS
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x67, 0x42, 0x00, 0x0a, 0xe9, 0x40, 0x20]);
        // PPS
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x68, 0xce, 0x38, 0x80]);

        let bytes = Bytes::from(data);
        let seq = FlvMuxer::build_avc_sequence_header(&bytes).unwrap();
        assert_eq!(seq[0], 1); // configurationVersion
        assert_eq!(seq[4], 0xff); // lengthSizeMinus1 = 3
        assert_eq!(seq[5], 0xe1); // numOfSPS = 1
    }

    #[test]
    fn annex_b_to_avcc_conversion() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x65, 0x88, 0x00]); // IDR NAL
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x41, 0x9a, 0x00]); // non-IDR NAL

        let bytes = Bytes::from(data);
        let avcc = annex_b_to_avcc(&bytes);
        // First NAL: 4-byte length (3) + 3 bytes = 7
        // Second NAL: 4-byte length (3) + 3 bytes = 7
        assert_eq!(avcc.len(), 14);
        assert_eq!(&avcc[0..4], &[0x00, 0x00, 0x00, 0x03]); // length = 3
        assert_eq!(&avcc[4..7], &[0x65, 0x88, 0x00]);
        assert_eq!(&avcc[7..11], &[0x00, 0x00, 0x00, 0x03]);
        assert_eq!(&avcc[11..14], &[0x41, 0x9a, 0x00]);
    }
}
