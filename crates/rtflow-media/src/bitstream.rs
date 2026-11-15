//! Read-only bitstream helpers for hardware-encoder integration (HiSilicon
//! HiMPP VENC, V4L2, etc.): Annex-B NAL splitting, NAL classification and
//! H.264/H.265 parameter-set extraction for SDP fmtp generation.
//!
//! All functions are pure and allocation-light; they never parse beyond
//! what is needed for RTP packaging and SDP signaling.

use bytes::{BufMut, Bytes, BytesMut};
use std::sync::Arc;

/// Annex-B start codes, in descending length order.
pub const START_CODES: [&[u8]; 2] = [&[0, 0, 0, 1], &[0, 0, 1]];

/// Split Annex-B data into NAL units (without start codes).
///
/// If `data` contains no start code at all, an empty iterator is returned —
/// callers typically then treat the whole buffer as a single NAL (raw
/// length-prefixed or single-NAL payloads).
pub fn annex_b_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        let code = if data[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        let start = i + code;
        let mut j = start;
        while j < data.len() && !data[j..].starts_with(&[0, 0, 1]) {
            j += 1;
        }
        // A 4-byte code is also matched by the 3-byte probe above: the
        // probe lands on the third zero of 00 00 00 01, so back up one
        // byte when the code is actually four bytes long.
        if j >= 1 && data[j - 1..].starts_with(&[0, 0, 0, 1]) {
            j -= 1;
        }
        nals.push(&data[start..j]);
        i = j;
    }
    nals
}

/// H.264 NAL unit type (5 bits of the first byte).
pub fn nal_type_h264(nal: &[u8]) -> Option<u8> {
    nal.first().map(|byte| byte & 0x1f)
}

/// H.265 NAL unit type (6 bits spanning the first two bytes).
pub fn nal_type_h265(nal: &[u8]) -> Option<u8> {
    if nal.len() < 2 {
        return None;
    }
    Some((nal[0] >> 1) & 0x3f)
}

/// Convert an Annex-B byte stream to AVCC (length-prefixed) NALs.
///
/// Trailing zero bytes of each NAL (Annex-B `trailing_zero_8bits`
/// alignment padding) are trimmed so the length prefix matches exactly
/// what decoders expect from FLV/MP4.
pub fn annexb_to_avcc(data: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(data.len());
    for nalu in annex_b_nals(data) {
        // Trim trailing zero bytes (Annex-B `trailing_zero_8bits` padding)
        // and skip NALs that collapse to nothing (e.g. a stream ending in
        // a bare start code).
        let end = nalu.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        if end == 0 {
            continue;
        }
        out.put_u32(end as u32);
        out.put_slice(&nalu[..end]);
    }
    out.freeze()
}

/// True when the Annex-B access unit contains an H.264 IDR slice
/// (NAL type 5) or an H.265 IDR_W_RADL/IDR_N_LP (types 19/20). Parameter
/// sets may precede the IDR slice, so every NAL is scanned.
pub fn is_idr(codec: super::Codec, data: &[u8]) -> bool {
    for nal in annex_b_nals(data) {
        let idr = match codec {
            super::Codec::H264 => nal_type_h264(nal) == Some(5),
            super::Codec::H265 => matches!(nal_type_h265(nal), Some(19 | 20)),
            _ => false,
        };
        if idr {
            return true;
        }
    }
    false
}

/// Whether a NAL unit is an H.264 IDR slice (type 5).
pub fn is_h264_idr(nal: &[u8]) -> bool {
    nal_type_h264(nal) == Some(5)
}

/// Whether a NAL unit is an H.265 random access point: IDR (19/20),
/// CRA (21) or BLA (16–18). These are the slices a decoder can start
/// decoding from without prior pictures.
pub fn is_h265_random_access(nal: &[u8]) -> bool {
    matches!(nal_type_h265(nal), Some(16..=21))
}

/// Whether the Annex-B access unit contains a random access point:
/// H.264 IDR (type 5) or H.265 IDR/CRA/BLA (types 16–21). Audio and VP8/VP9
/// frames are always independently decodable.
pub fn is_random_access_point(codec: super::Codec, data: &[u8]) -> bool {
    match codec {
        super::Codec::H264 => annex_b_nals(data).iter().any(|nal| is_h264_idr(nal)),
        super::Codec::H265 => annex_b_nals(data)
            .iter()
            .any(|nal| is_h265_random_access(nal)),
        _ => true,
    }
}

/// Parameter sets found in an Annex-B buffer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParameterSets {
    /// H.264 SPS / H.265 VPS (H.265 NAL 32).
    pub vps: Option<Bytes>,
    /// H.264/H.265 SPS (NAL 33).
    pub sps: Option<Bytes>,
    /// H.264/H.265 PPS (NAL 34).
    pub pps: Option<Bytes>,
}

impl ParameterSets {
    /// Whether any parameter set was found.
    pub fn is_empty(&self) -> bool {
        self.vps.is_none() && self.sps.is_none() && self.pps.is_none()
    }
}

/// Extract H.264 SPS/PPS from an Annex-B buffer (NAL types 7 and 8).
pub fn h264_parameter_sets(data: &[u8]) -> ParameterSets {
    let mut sets = ParameterSets::default();
    for nal in annex_b_nals(data) {
        match nal_type_h264(nal) {
            Some(7) if sets.sps.is_none() => sets.sps = Some(Bytes::copy_from_slice(nal)),
            Some(8) if sets.pps.is_none() => sets.pps = Some(Bytes::copy_from_slice(nal)),
            _ => {}
        }
        if sets.sps.is_some() && sets.pps.is_some() {
            break;
        }
    }
    sets
}

/// Extract H.265 VPS/SPS/PPS from an Annex-B buffer (NAL types 32/33/34).
pub fn h265_parameter_sets(data: &[u8]) -> ParameterSets {
    let mut sets = ParameterSets::default();
    for nal in annex_b_nals(data) {
        match nal_type_h265(nal) {
            Some(32) if sets.vps.is_none() => sets.vps = Some(Bytes::copy_from_slice(nal)),
            Some(33) if sets.sps.is_none() => sets.sps = Some(Bytes::copy_from_slice(nal)),
            Some(34) if sets.pps.is_none() => sets.pps = Some(Bytes::copy_from_slice(nal)),
            _ => {}
        }
        if sets.vps.is_some() && sets.sps.is_some() && sets.pps.is_some() {
            break;
        }
    }
    sets
}

/// Base64-encode a NAL body (no start code) for `sprop-parameter-sets`.
pub fn base64_nal(nal: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(nal)
}

/// H.264 `profile-level-id` (6 hex chars) from an SPS NAL: bytes 1..4 of
/// an NAL-header-carrying SPS (or 0..3 of a headerless SPS) are
/// profile_idc, constraint flags and level_idc.
pub fn h264_profile_level_id(sps: &[u8]) -> Option<String> {
    let offset = if sps.first().is_some_and(|byte| byte & 0x1f == 7) {
        1 // NAL header present
    } else {
        0 // headerless body
    };
    let profile = sps.get(offset)?;
    let constraint = sps.get(offset + 1)?;
    let level = sps.get(offset + 2)?;
    Some(format!("{profile:02x}{constraint:02x}{level:02x}"))
}

/// Build a full H.264 SDP fmtp value:
/// `profile-level-id=..;sprop-parameter-sets=..;packetization-mode=1`.
pub fn h264_fmtp(sps: &[u8], pps: &[u8]) -> String {
    let profile = h264_profile_level_id(sps).unwrap_or_else(|| "42001f".into());
    format!(
        "profile-level-id={profile};sprop-parameter-sets={},{};packetization-mode=1",
        base64_nal(sps),
        base64_nal(pps)
    )
}

/// Build a full H.265 SDP fmtp value from VPS/SPS/PPS
/// (`sprop-vps=..;sprop-sps=..;sprop-pps=..`).
pub fn h265_fmtp(vps: &[u8], sps: &[u8], pps: &[u8]) -> String {
    format!(
        "sprop-vps={};sprop-sps={};sprop-pps={}",
        base64_nal(vps),
        base64_nal(sps),
        base64_nal(pps)
    )
}

/// Build an AVCDecoderConfigurationRecord from SPS + PPS NALs.
///
/// `None` when the SPS is too short to carry profile/compatibility/level
/// (the first three bytes beyond the NAL header).
pub fn build_avc_config_record(sps: &[u8], pps: &[u8]) -> Option<Bytes> {
    if sps.len() < 4 || pps.is_empty() {
        return None;
    }
    let mut buf = BytesMut::with_capacity(sps.len() + pps.len() + 16);
    buf.put_u8(1); // configurationVersion
    buf.put_u8(sps[1]); // AVCProfileIndication
    buf.put_u8(sps[2]); // profile_compatibility
    buf.put_u8(sps[3]); // AVCLevelIndication
    buf.put_u8(0xff); // lengthSizeMinusOne = 3 (4-byte lengths)
    buf.put_u8(0xe1); // numOfSequenceParameterSets = 1
    buf.put_u16(sps.len() as u16);
    buf.put_slice(sps);
    buf.put_u8(1); // numOfPictureParameterSets = 1
    buf.put_u16(pps.len() as u16);
    buf.put_slice(pps);
    Some(buf.freeze())
}

/// Build a HEVCDecoderConfigurationRecord from VPS + SPS + PPS NALs.
///
/// `None` when any parameter set is too short (SPS must be > 14 bytes to
/// carry the profile/tier/level fields).
pub fn build_hevc_config_record(vps: &[u8], sps: &[u8], pps: &[u8]) -> Option<Bytes> {
    if vps.len() < 2 || sps.len() <= 14 || pps.len() < 2 {
        return None;
    }
    let profile = &sps[2..];
    let mut buf = BytesMut::with_capacity(128);
    buf.put_u8(1);
    buf.put_u8(profile[1]);
    buf.put_slice(&profile[2..6]);
    buf.put_slice(&profile[6..12]);
    buf.put_u8(profile[12]);
    buf.put_u16(0xf000);
    buf.put_u8(0xfc);
    buf.put_u8(0xfd);
    buf.put_u8(0xf8);
    buf.put_u8(0xf8);
    buf.put_u16(0);
    buf.put_u8(0x0f);
    buf.put_u8(3);
    for (kind, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        buf.put_u8(0x80 | kind);
        buf.put_u16(1);
        buf.put_u16(nal.len() as u16);
        buf.put_slice(nal);
    }
    Some(buf.freeze())
}

/// AAC sampling-frequency table (AudioSpecificConfig 4-bit index),
/// index → Hz. Index 13–15 are reserved.
const AAC_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Sample rate (Hz) for an AAC sampling-frequency index (0–12).
/// Unknown/reserved indexes → `None` (callers choose a fallback).
pub fn aac_sample_rate_for_index(index: u8) -> Option<u32> {
    AAC_SAMPLE_RATES.get(index as usize).copied()
}

/// AAC sampling-frequency index for a sample rate (Hz). Unknown rates
/// default to index 4 (44 100 Hz) — the FLV/TS convention.
pub fn aac_sample_rate_index(sample_rate: u32) -> u8 {
    AAC_SAMPLE_RATES
        .iter()
        .position(|rate| *rate == sample_rate)
        .unwrap_or(4) as u8
}

/// 2-byte AudioSpecificConfig (AAC-LC, MPEG-4 Audio Object Type 2) for
/// FLV/MP4/TS sequence headers: 5 bits objectType + 4 bits frequency
/// index + 4 bits channel configuration.
pub fn aac_audio_specific_config_bytes(sample_rate: u32, channels: u8) -> Bytes {
    let freq_index = aac_sample_rate_index(sample_rate);
    let channel_config = channels.min(7);
    let byte0 = (2u8 << 3) | ((freq_index >> 1) & 0x07);
    let byte1 = ((freq_index & 0x01) << 7) | ((channel_config & 0x0F) << 3);
    Bytes::from(vec![byte0, byte1])
}

/// AudioSpecificConfig for AAC-LC (MPEG-4 Audio Object Type 2) with the
/// given sample rate and channel count, hex-encoded for SDP `config=`.
pub fn aac_audio_specific_config(sample_rate: u32, channels: u8) -> Option<String> {
    let sampling_frequency_index = match sample_rate {
        96_000 => 0,
        88_200 => 1,
        64_000 => 2,
        48_000 => 3,
        44_100 => 4,
        32_000 => 5,
        24_000 => 6,
        22_050 => 7,
        16_000 => 8,
        12_000 => 9,
        11_025 => 10,
        8_000 => 11,
        _ => return None,
    };
    let channel_configuration = channels.clamp(1, 7);
    let mut asc = 0u16;
    asc |= 2 << 11; // audioObjectType = 2 (AAC-LC)
    asc |= sampling_frequency_index << 7;
    asc |= u16::from(channel_configuration) << 3;
    Some(format!("{asc:04x}"))
}

/// Build an AAC SDP fmtp value (RFC 3640) from an AudioSpecificConfig hex
/// string (see [`aac_audio_specific_config`]).
pub fn aac_fmtp(config: &str) -> String {
    format!(
        "profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config={config}"
    )
}

/// Runtime-extracted fmtp payload (already formatted) — used by the mount
/// auto-injection path.
pub type FmtpValue = Arc<str>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annex_b_splitting_handles_3_and_4_byte_codes() {
        let data: &[u8] = &[
            0, 0, 0, 1, 0x65, 1, 2, 0, 0, 1, 0x41, 3, 0, 0, 0, 1, 0x67, 4,
        ];
        let nals = annex_b_nals(data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x65, 1, 2]);
        assert_eq!(nals[1], &[0x41, 3]);
        assert_eq!(nals[2], &[0x67, 4]);
    }

    #[test]
    fn empty_or_raw_data_yields_no_nals() {
        assert!(annex_b_nals(&[]).is_empty());
        assert!(annex_b_nals(&[0x65, 0x88, 0x84]).is_empty());
    }

    #[test]
    fn h264_sets_extraction_and_fmtp() {
        let idr = [
            0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f, 0x8c, 0x8d, 0x40, //
            0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, //
            0, 0, 0, 1, 0x65, 0x88, 0x84, //
        ];
        let sets = h264_parameter_sets(&idr);
        assert_eq!(
            sets.sps.as_deref(),
            Some(&[0x67, 0x42, 0x00, 0x1f, 0x8c, 0x8d, 0x40][..])
        );
        assert_eq!(sets.pps.as_deref(), Some(&[0x68, 0xce, 0x3c, 0x80][..]));
        let fmtp = h264_fmtp(sets.sps.as_deref().unwrap(), sets.pps.as_deref().unwrap());
        assert!(
            fmtp.starts_with("profile-level-id=42001f;sprop-parameter-sets="),
            "{fmtp}"
        );
        assert!(fmtp.ends_with(";packetization-mode=1"));
        assert!(is_idr(crate::Codec::H264, &idr));
    }

    #[test]
    fn h265_sets_extraction() {
        let idr = [
            0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00,
            0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x3c, 0x95, 0xc0, 0x90, 0xa1, //
            0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00,
            0x03, 0x00, 0x00, 0x03, 0x00, 0x3c, 0xa0, //
            0, 0, 0, 1, 0x44, 0x01, 0xc1, 0x72, 0xb4, 0x62, 0x40, //
            0, 0, 0, 1, 0x26, 0x01, 0xaf, 0x0a, //
        ];
        let sets = h265_parameter_sets(&idr);
        assert!(sets.vps.is_some() && sets.sps.is_some() && sets.pps.is_some());
        assert!(is_idr(crate::Codec::H265, &idr));
        let fmtp = h265_fmtp(
            sets.vps.as_deref().unwrap(),
            sets.sps.as_deref().unwrap(),
            sets.pps.as_deref().unwrap(),
        );
        assert!(fmtp.starts_with("sprop-vps="), "{fmtp}");
    }

    #[test]
    fn aac_config_generation() {
        // audioObjectType=2 << 11 | SFI << 7 | channelConfig << 3
        assert_eq!(
            aac_audio_specific_config(48_000, 2).as_deref(),
            Some("1190")
        );
        assert_eq!(
            aac_audio_specific_config(44_100, 2).as_deref(),
            Some("1210")
        );
        assert_eq!(aac_audio_specific_config(8_000, 1).as_deref(), Some("1588"));
        assert_eq!(
            aac_audio_specific_config(11_025, 2).as_deref(),
            Some("1510")
        );
        assert!(aac_audio_specific_config(12_345, 2).is_none());
    }
}
