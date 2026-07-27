//! FLV tag muxing and demuxing for RTMP audio/video messages.
//!
//! RTMP carries media as FLV tag bodies (without the 11-byte FLV tag header
//! when sent as RTMP messages). This module handles:
//! - Parsing AVC/AAC sequence headers and media data from FLV tag bodies
//! - Constructing FLV tag bodies from AccessUnits for play output
use bytes::{BufMut, Bytes, BytesMut};
use std::time::Duration;

use crate::media::{AccessUnit, Codec, MediaKind, TrackDescription};

/// FLV video codec IDs
const FLV_VIDEO_H264: u8 = 7;
const FLV_VIDEO_H265: u8 = 12; // Enhanced RTMP / non-standard extension

/// FLV audio codec IDs
const FLV_AUDIO_AAC: u8 = 10;

/// AVC packet types
const AVC_SEQUENCE_HEADER: u8 = 0;
const AVC_NALU: u8 = 1;

/// AAC packet types
const AAC_SEQUENCE_HEADER: u8 = 0;
const AAC_RAW: u8 = 1;

/// Video frame types
const FLV_KEYFRAME: u8 = 1;
const FLV_INTERFRAME: u8 = 2;

/// Parsed video data from an FLV video tag body.
#[derive(Debug)]
pub enum VideoData {
    /// AVC/H.264 sequence header (SPS/PPS in AVCC format)
    AvcSequenceHeader(Bytes),
    /// One or more H.264 NALUs (AVCC length-prefixed)
    AvcNalu {
        composition_time: i32,
        data: Bytes,
        is_keyframe: bool,
    },
    /// H.265 sequence header
    HevcSequenceHeader(Bytes),
    /// H.265 NALUs
    HevcNalu {
        composition_time: i32,
        data: Bytes,
        is_keyframe: bool,
    },
}

/// Parsed audio data from an FLV audio tag body.
#[derive(Debug)]
pub enum AudioData {
    /// AAC sequence header (AudioSpecificConfig)
    AacSequenceHeader(Bytes),
    /// Raw AAC frame
    AacRaw(Bytes),
}

/// Parse an RTMP video message payload (FLV video tag body).
pub fn parse_video_tag(data: &[u8]) -> Option<VideoData> {
    if data.len() < 5 {
        return None;
    }
    let frame_type = (data[0] >> 4) & 0x0F;
    let codec_id = data[0] & 0x0F;
    let is_keyframe = frame_type == FLV_KEYFRAME;

    match codec_id {
        FLV_VIDEO_H264 => {
            let avc_packet_type = data[1];
            let composition_time = ((data[2] as i32) << 16)
                | ((data[3] as i32) << 8)
                | (data[4] as i32);
            // Sign extend 24-bit
            let composition_time = if composition_time & 0x800000 != 0 {
                composition_time | !0xFFFFFF
            } else {
                composition_time
            };
            let body = Bytes::copy_from_slice(&data[5..]);
            match avc_packet_type {
                AVC_SEQUENCE_HEADER => Some(VideoData::AvcSequenceHeader(body)),
                AVC_NALU => Some(VideoData::AvcNalu {
                    composition_time,
                    data: body,
                    is_keyframe,
                }),
                _ => None,
            }
        }
        FLV_VIDEO_H265 => {
            let hevc_packet_type = data[1];
            let composition_time = ((data[2] as i32) << 16)
                | ((data[3] as i32) << 8)
                | (data[4] as i32);
            let composition_time = if composition_time & 0x800000 != 0 {
                composition_time | !0xFFFFFF
            } else {
                composition_time
            };
            let body = Bytes::copy_from_slice(&data[5..]);
            match hevc_packet_type {
                AVC_SEQUENCE_HEADER => Some(VideoData::HevcSequenceHeader(body)),
                AVC_NALU => Some(VideoData::HevcNalu {
                    composition_time,
                    data: body,
                    is_keyframe,
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Parse an RTMP audio message payload (FLV audio tag body).
pub fn parse_audio_tag(data: &[u8]) -> Option<AudioData> {
    if data.len() < 2 {
        return None;
    }
    let sound_format = (data[0] >> 4) & 0x0F;
    match sound_format {
        FLV_AUDIO_AAC => {
            let aac_packet_type = data[1];
            let body = Bytes::copy_from_slice(&data[2..]);
            match aac_packet_type {
                AAC_SEQUENCE_HEADER => Some(AudioData::AacSequenceHeader(body)),
                AAC_RAW => Some(AudioData::AacRaw(body)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Convert AVCC (length-prefixed) NALUs to Annex B format.
pub fn avcc_to_annexb(data: &[u8], sps_pps: Option<&[u8]>) -> Bytes {
    let mut out = BytesMut::with_capacity(data.len() + 64);
    let start_code = [0x00, 0x00, 0x00, 0x01];

    // If we have SPS/PPS from sequence header, prepend them for keyframes
    if let Some(config) = sps_pps {
        if let Some(params) = extract_sps_pps_from_avcc_config(config) {
            for param in &params {
                out.put_slice(&start_code);
                out.put_slice(param);
            }
        }
    }

    let mut pos = 0;
    while pos + 4 <= data.len() {
        let nalu_len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
                as usize;
        pos += 4;
        if pos + nalu_len > data.len() {
            break;
        }
        out.put_slice(&start_code);
        out.put_slice(&data[pos..pos + nalu_len]);
        pos += nalu_len;
    }
    out.freeze()
}

/// Extract SPS and PPS NALUs from an AVCC decoder configuration record.
fn extract_sps_pps_from_avcc_config(config: &[u8]) -> Option<Vec<Bytes>> {
    if config.len() < 7 {
        return None;
    }
    // AVCDecoderConfigurationRecord:
    // [0] configurationVersion
    // [1] AVCProfileIndication
    // [2] profile_compatibility
    // [3] AVCLevelIndication
    // [4] lengthSizeMinusOne (reserved + 2 bits)
    // [5] numOfSequenceParameterSets (reserved + 5 bits)
    let mut params = Vec::new();
    let num_sps = (config[5] & 0x1F) as usize;
    let mut pos = 6;

    for _ in 0..num_sps {
        if pos + 2 > config.len() {
            return None;
        }
        let len = u16::from_be_bytes([config[pos], config[pos + 1]]) as usize;
        pos += 2;
        if pos + len > config.len() {
            return None;
        }
        params.push(Bytes::copy_from_slice(&config[pos..pos + len]));
        pos += len;
    }

    if pos >= config.len() {
        return Some(params);
    }
    let num_pps = config[pos] as usize;
    pos += 1;

    for _ in 0..num_pps {
        if pos + 2 > config.len() {
            break;
        }
        let len = u16::from_be_bytes([config[pos], config[pos + 1]]) as usize;
        pos += 2;
        if pos + len > config.len() {
            break;
        }
        params.push(Bytes::copy_from_slice(&config[pos..pos + len]));
        pos += len;
    }

    Some(params)
}

/// Convert Annex B NALUs to AVCC (length-prefixed) format.
pub fn annexb_to_avcc(data: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(data.len());
    let nalus = split_annexb(data);
    for nalu in nalus {
        out.put_u32(nalu.len() as u32);
        out.put_slice(nalu);
    }
    out.freeze()
}

/// Split Annex B byte stream into individual NALUs.
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nalus = Vec::new();
    let mut pos = 0;
    let mut nalu_start = None;

    while pos < data.len() {
        // Look for start code (0x000001 or 0x00000001)
        if pos + 3 <= data.len() && data[pos] == 0 && data[pos + 1] == 0 {
            let (sc_len, found) = if data[pos + 2] == 1 {
                (3, true)
            } else if pos + 4 <= data.len() && data[pos + 2] == 0 && data[pos + 3] == 1 {
                (4, true)
            } else {
                (0, false)
            };

            if found {
                if let Some(start) = nalu_start {
                    // Trim trailing zeros from previous NALU
                    let mut end = pos;
                    while end > start && data[end - 1] == 0 {
                        end -= 1;
                    }
                    if end > start {
                        nalus.push(&data[start..end]);
                    }
                }
                pos += sc_len;
                nalu_start = Some(pos);
                continue;
            }
        }
        pos += 1;
    }

    if let Some(start) = nalu_start {
        if start < data.len() {
            nalus.push(&data[start..]);
        }
    }

    nalus
}

/// Build an FLV video tag body from an AccessUnit (for play output).
/// Output is in AVCC format.
pub fn build_video_tag(unit: &AccessUnit, is_sequence_header: bool) -> Bytes {
    let mut buf = BytesMut::with_capacity(unit.data.len() + 16);

    let frame_type = if unit.is_keyframe {
        FLV_KEYFRAME
    } else {
        FLV_INTERFRAME
    };

    match unit.codec {
        Codec::H264 => {
            buf.put_u8((frame_type << 4) | FLV_VIDEO_H264);
            if is_sequence_header {
                buf.put_u8(AVC_SEQUENCE_HEADER);
            } else {
                buf.put_u8(AVC_NALU);
            }
            // Composition time offset (0 for now, could be computed from PTS-DTS)
            let cts = if let (Some(dts), pts) = (unit.dts, unit.pts) {
                let diff = pts.as_millis() as i32 - dts.as_millis() as i32;
                diff.clamp(-0x800000, 0x7FFFFF)
            } else {
                0
            };
            buf.put_u8((cts >> 16) as u8);
            buf.put_u8((cts >> 8) as u8);
            buf.put_u8(cts as u8);
            buf.put_slice(&unit.data);
        }
        Codec::H265 => {
            buf.put_u8((frame_type << 4) | FLV_VIDEO_H265);
            if is_sequence_header {
                buf.put_u8(AVC_SEQUENCE_HEADER);
            } else {
                buf.put_u8(AVC_NALU);
            }
            let cts = 0i32;
            buf.put_u8((cts >> 16) as u8);
            buf.put_u8((cts >> 8) as u8);
            buf.put_u8(cts as u8);
            buf.put_slice(&unit.data);
        }
        _ => {}
    }

    buf.freeze()
}

/// Build an FLV audio tag body from an AccessUnit (for play output).
pub fn build_audio_tag(unit: &AccessUnit, is_sequence_header: bool) -> Bytes {
    let mut buf = BytesMut::with_capacity(unit.data.len() + 8);

    match unit.codec {
        Codec::Aac => {
            // Sound format AAC (10), rate 44kHz (3), 16-bit (1), stereo (1)
            buf.put_u8((FLV_AUDIO_AAC << 4) | 0x0F);
            if is_sequence_header {
                buf.put_u8(AAC_SEQUENCE_HEADER);
            } else {
                buf.put_u8(AAC_RAW);
            }
            buf.put_slice(&unit.data);
        }
        _ => {}
    }

    buf.freeze()
}

/// Build an AVCC decoder configuration record from SPS/PPS.
pub fn build_avc_sequence_header(sps: &[u8], pps: &[u8]) -> Bytes {
    let mut buf = BytesMut::with_capacity(sps.len() + pps.len() + 16);
    buf.put_u8(1); // configurationVersion
    if sps.len() >= 4 {
        buf.put_u8(sps[1]); // AVCProfileIndication
        buf.put_u8(sps[2]); // profile_compatibility
        buf.put_u8(sps[3]); // AVCLevelIndication
    } else {
        buf.put_u8(0x42); // baseline
        buf.put_u8(0x00);
        buf.put_u8(0x1E); // level 3.0
    }
    buf.put_u8(0xFF); // lengthSizeMinusOne = 3 (4 bytes)
    buf.put_u8(0xE1); // numOfSPS = 1
    buf.put_u16(sps.len() as u16);
    buf.put_slice(sps);
    buf.put_u8(1); // numOfPPS
    buf.put_u16(pps.len() as u16);
    buf.put_slice(pps);
    buf.freeze()
}

/// Build an AAC AudioSpecificConfig for the given sample rate and channels.
pub fn build_aac_sequence_header(sample_rate: u32, channels: u8) -> Bytes {
    let freq_index = match sample_rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        7350 => 12,
        _ => 4, // default 44100
    };
    // AudioSpecificConfig: 5 bits objectType (2=AAC-LC) + 4 bits freqIndex + 4 bits channels
    let byte0 = (2u8 << 3) | ((freq_index >> 1) & 0x07);
    let byte1 = ((freq_index & 0x01) << 7) | ((channels & 0x0F) << 3);
    Bytes::from(vec![byte0, byte1])
}

/// Build MediaDescription tracks from RTMP metadata + sequence headers.
pub fn tracks_from_rtmp(
    has_video: bool,
    has_audio: bool,
    video_codec: Option<Codec>,
    audio_config: Option<&[u8]>,
) -> Vec<TrackDescription> {
    let mut tracks = Vec::new();

    if has_video {
        let codec = video_codec.unwrap_or(Codec::H264);
        tracks.push(TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec,
            clock_rate: 90000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: "trackID=0".into(),
            #[cfg(feature = "srtp")]
            crypto: None,
        });
    }

    if has_audio {
        let (sample_rate, channels) = parse_aac_config(audio_config);
        tracks.push(TrackDescription {
            id: if has_video { 1 } else { 0 },
            kind: MediaKind::Audio,
            codec: Codec::Aac,
            clock_rate: sample_rate,
            channels: Some(channels),
            payload_type: 97,
            fmtp: Some(
                format!(
                    "profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config={:02x}{:02x}",
                    audio_config.map(|c| c[0]).unwrap_or(0x12),
                    audio_config.map(|c| c.get(1).copied()).flatten().unwrap_or(0x10),
                )
                .into(),
            ),
            control: if has_video { "trackID=1" } else { "trackID=0" }.into(),
            #[cfg(feature = "srtp")]
            crypto: None,
        });
    }

    tracks
}

fn parse_aac_config(config: Option<&[u8]>) -> (u32, u8) {
    match config {
        Some(c) if c.len() >= 2 => {
            let freq_index = ((c[0] & 0x07) << 1) | ((c[1] >> 7) & 0x01);
            let channels = (c[1] >> 3) & 0x0F;
            let sample_rate = match freq_index {
                0 => 96000,
                1 => 88200,
                2 => 64000,
                3 => 48000,
                4 => 44100,
                5 => 32000,
                6 => 24000,
                7 => 22050,
                8 => 16000,
                9 => 12000,
                10 => 11025,
                11 => 8000,
                12 => 7350,
                _ => 44100,
            };
            (sample_rate, channels.max(1))
        }
        _ => (44100, 2),
    }
}

/// Compute RTP timestamp from Duration and clock rate.
pub fn duration_to_rtp_timestamp(d: Duration, clock_rate: u32) -> u32 {
    (d.as_secs_f64() * clock_rate as f64) as u32
}

/// Compute Duration from RTMP timestamp (milliseconds) and clock rate.
pub fn rtmp_timestamp_to_duration(ts_ms: u32) -> Duration {
    Duration::from_millis(ts_ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_annexb_roundtrip() {
        // Create a fake AVCC stream with 2 NALUs
        let nalu1 = [0x65, 0x01, 0x02, 0x03]; // IDR
        let nalu2 = [0x41, 0x04, 0x05]; // non-IDR
        let mut avcc = BytesMut::new();
        avcc.put_u32(nalu1.len() as u32);
        avcc.put_slice(&nalu1);
        avcc.put_u32(nalu2.len() as u32);
        avcc.put_slice(&nalu2);

        let annexb = avcc_to_annexb(&avcc, None);
        let nalus = split_annexb(&annexb);
        assert_eq!(nalus.len(), 2);
        assert_eq!(nalus[0], &nalu1);
        assert_eq!(nalus[1], &nalu2);
    }

    #[test]
    fn build_and_parse_video_tag() {
        let unit = AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::from_millis(100),
            dts: Some(Duration::from_millis(100)),
            rtp_timestamp: 9000,
            is_keyframe: true,
            data: Bytes::from(vec![0x00, 0x00, 0x00, 0x04, 0x65, 0x01, 0x02, 0x03]),
        };
        let tag = build_video_tag(&unit, false);
        let parsed = parse_video_tag(&tag).unwrap();
        match parsed {
            VideoData::AvcNalu {
                is_keyframe, data, ..
            } => {
                assert!(is_keyframe);
                assert_eq!(data.len(), 8);
            }
            _ => panic!("expected AvcNalu"),
        }
    }

    #[test]
    fn aac_sequence_header() {
        let config = build_aac_sequence_header(44100, 2);
        assert_eq!(config.len(), 2);
        let (rate, ch) = parse_aac_config(Some(&config));
        assert_eq!(rate, 44100);
        assert_eq!(ch, 2);
    }
}
