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

/// Enhanced RTMP: bit 7 of first byte set indicates enhanced format
const ENHANCED_RTMP_FLAG: u8 = 0x80;

/// Enhanced RTMP packet types (bits 3-0 when enhanced flag is set)
const ENHANCED_PACKET_SEQUENCE_START: u8 = 0;
const ENHANCED_PACKET_CODED_FRAMES: u8 = 1;
// Sequence-end packet type documented for completeness; not emitted yet.
#[allow(dead_code)]
const ENHANCED_PACKET_SEQUENCE_END: u8 = 2;
const ENHANCED_PACKET_CODED_FRAMES_X: u8 = 3; // no composition time

/// Enhanced RTMP FourCC codes
const FOURCC_HEVC: &[u8; 4] = b"hvc1";
// AV1/VP9 FourCCs documented for the enhanced-RTMP mapping; HEVC is the
// only codec actually muxed today.
#[allow(dead_code)]
const FOURCC_AV1: &[u8; 4] = b"av01";
#[allow(dead_code)]
const FOURCC_VP9: &[u8; 4] = b"vp09";

/// FLV audio codec IDs
const FLV_AUDIO_AAC: u8 = 10;
// FLV sound formats: 7 = A-law, 8 = μ-law.
const FLV_AUDIO_ALAW: u8 = 7;
const FLV_AUDIO_ULAW: u8 = 8;

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
pub fn parse_video_tag(data: &Bytes) -> Option<VideoData> {
    if data.len() < 5 {
        return None;
    }

    // Check for Enhanced RTMP format (bit 7 set)
    if data[0] & ENHANCED_RTMP_FLAG != 0 {
        return parse_enhanced_video_tag(data);
    }

    let frame_type = (data[0] >> 4) & 0x0F;
    let codec_id = data[0] & 0x0F;
    let is_keyframe = frame_type == FLV_KEYFRAME;

    match codec_id {
        FLV_VIDEO_H264 => {
            let avc_packet_type = data[1];
            let composition_time =
                ((data[2] as i32) << 16) | ((data[3] as i32) << 8) | (data[4] as i32);
            // Sign extend 24-bit
            let composition_time = if composition_time & 0x800000 != 0 {
                composition_time | !0xFFFFFF
            } else {
                composition_time
            };
            let body = data.slice(5..);
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
            let composition_time =
                ((data[2] as i32) << 16) | ((data[3] as i32) << 8) | (data[4] as i32);
            let composition_time = if composition_time & 0x800000 != 0 {
                composition_time | !0xFFFFFF
            } else {
                composition_time
            };
            let body = data.slice(5..);
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

/// Parse Enhanced RTMP video tag (OBS 29+, FFmpeg 6+).
///
/// Format: [enhanced_byte][FourCC(4)][packet_data...]
/// enhanced_byte: bit7=1, bits6-4=frame_type, bits3-0=packet_type
fn parse_enhanced_video_tag(data: &Bytes) -> Option<VideoData> {
    if data.len() < 5 {
        return None;
    }

    let frame_type = (data[0] >> 4) & 0x07;
    let packet_type = data[0] & 0x0F;
    let is_keyframe = frame_type == FLV_KEYFRAME;
    let fourcc = &data[1..5];

    if fourcc == FOURCC_HEVC {
        match packet_type {
            ENHANCED_PACKET_SEQUENCE_START => {
                // HEVC decoder configuration record
                if data.len() > 5 {
                    Some(VideoData::HevcSequenceHeader(data.slice(5..)))
                } else {
                    None
                }
            }
            ENHANCED_PACKET_CODED_FRAMES => {
                // 3 bytes composition time + coded data
                if data.len() < 8 {
                    return None;
                }
                let composition_time =
                    ((data[5] as i32) << 16) | ((data[6] as i32) << 8) | (data[7] as i32);
                let composition_time = if composition_time & 0x800000 != 0 {
                    composition_time | !0xFFFFFF
                } else {
                    composition_time
                };
                Some(VideoData::HevcNalu {
                    composition_time,
                    data: data.slice(8..),
                    is_keyframe,
                })
            }
            ENHANCED_PACKET_CODED_FRAMES_X => {
                // No composition time (CTS=0)
                Some(VideoData::HevcNalu {
                    composition_time: 0,
                    data: data.slice(5..),
                    is_keyframe,
                })
            }
            _ => None,
        }
    } else {
        None // AV1/VP9 not yet supported
    }
}

/// Parse an RTMP audio message payload (FLV audio tag body).
pub fn parse_audio_tag(data: &Bytes) -> Option<AudioData> {
    if data.len() < 2 {
        return None;
    }
    let sound_format = (data[0] >> 4) & 0x0F;
    match sound_format {
        FLV_AUDIO_AAC => {
            let aac_packet_type = data[1];
            let body = data.slice(2..);
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
    if let Some(config) = sps_pps
        && let Some(params) = extract_sps_pps_from_avcc_config(config)
    {
        for param in &params {
            out.put_slice(&start_code);
            out.put_slice(param);
        }
    }

    let mut pos = 0;
    while pos + 4 <= data.len() {
        let nalu_len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
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
            crate::media::flv::write_composition_time(
                &mut buf,
                crate::media::flv::composition_time_ms(unit.pts, unit.dts),
            );
            buf.put_slice(&unit.data);
        }
        Codec::H265 => {
            buf.put_u8((frame_type << 4) | FLV_VIDEO_H265);
            if is_sequence_header {
                buf.put_u8(AVC_SEQUENCE_HEADER);
            } else {
                buf.put_u8(AVC_NALU);
            }
            // Same composition-time (PTS-DTS) computation as H.264:
            // enhanced-RTMP players expect B-frame offset here too.
            crate::media::flv::write_composition_time(
                &mut buf,
                crate::media::flv::composition_time_ms(unit.pts, unit.dts),
            );
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
        // FLV sound formats 7 (A-law) and 8 (μ-law), one byte of payload
        // header: rate bits 0x0F keep ffmpeg's demuxer happy (it rejects a
        // zero rate, same convention as the HTTP-FLV muxer).
        Codec::Pcma => buf.put_u8((FLV_AUDIO_ALAW << 4) | 0x0F),
        Codec::Pcmu => buf.put_u8((FLV_AUDIO_ULAW << 4) | 0x0F),
        Codec::Opus
        | Codec::Vp8
        | Codec::Vp9
        | Codec::H264
        | Codec::H265
        | Codec::Mp3
        | Codec::G726_16
        | Codec::G726_24
        | Codec::G726_32
        | Codec::G726_40
        | Codec::Av1
        | Codec::Jpeg
        | Codec::H263
        | Codec::Mpeg4 => {
            // FLV does not carry these in the RTMP play path today.
            return Bytes::new();
        }
    }
    // G.711 has no sequence-header concept; the payload follows the
    // one-byte audio header directly (AAC already appended its payload).
    if !matches!(unit.codec, Codec::Aac) {
        buf.put_slice(&unit.data);
    }

    buf.freeze()
}

/// Build an AVCC decoder configuration record from SPS/PPS.
pub fn build_avc_sequence_header(sps: &[u8], pps: &[u8]) -> Bytes {
    crate::media::bitstream::build_avc_config_record(sps, pps).unwrap_or_else(|| {
        // Degenerate SPS: fall back to baseline profile / level 3.0 so the
        // RTMP pipeline still produces a parseable record.
        let mut buf = BytesMut::with_capacity(sps.len() + pps.len() + 16);
        buf.put_u8(1); // configurationVersion
        buf.put_u8(0x42); // AVCProfileIndication (baseline)
        buf.put_u8(0x00); // profile_compatibility
        buf.put_u8(0x1E); // AVCLevelIndication (3.0)
        buf.put_u8(0xFF); // lengthSizeMinusOne = 3 (4 bytes)
        buf.put_u8(0xE1); // numOfSPS = 1
        buf.put_u16(sps.len() as u16);
        buf.put_slice(sps);
        buf.put_u8(1); // numOfPPS
        buf.put_u16(pps.len() as u16);
        buf.put_slice(pps);
        buf.freeze()
    })
}

/// Build an AAC AudioSpecificConfig for the given sample rate and channels.
pub fn build_aac_sequence_header(sample_rate: u32, channels: u8) -> Bytes {
    crate::media::bitstream::aac_audio_specific_config_bytes(sample_rate, channels)
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
            clock_rate: crate::media::clock::VIDEO_CLOCK_RATE,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: "trackID=0".into(),
            width: None,
            height: None,
            frame_rate: None,
            #[cfg(feature = "srtp")]
            crypto: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
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
                    audio_config.and_then(|c| c.get(1).copied()).unwrap_or(0x10),
                )
                .into(),
            ),
            control: if has_video { "trackID=1" } else { "trackID=0" }.into(),
            width: None,
            height: None,
            frame_rate: None,
            #[cfg(feature = "srtp")]
            crypto: None,
                    direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
});
    }

    tracks
}

fn parse_aac_config(config: Option<&[u8]>) -> (u32, u8) {
    use crate::media::bitstream::aac_sample_rate_for_index;
    match config {
        Some(c) if c.len() >= 2 => {
            let freq_index = ((c[0] & 0x07) << 1) | ((c[1] >> 7) & 0x01);
            let channels = (c[1] >> 3) & 0x0F;
            let sample_rate = aac_sample_rate_for_index(freq_index).unwrap_or(44_100);
            (sample_rate, channels.max(1))
        }
        _ => (44_100, 2),
    }
}

/// Compute RTP timestamp from Duration and clock rate.
pub fn duration_to_rtp_timestamp(d: Duration, clock_rate: u32) -> u32 {
    crate::media::clock::duration_to_ticks(d, clock_rate) as u32
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
        let nalus = crate::media::bitstream::annex_b_nals(&annexb);
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
