use crate::{Error, Result, media::Codec};
#[cfg(any(feature = "mp4", feature = "mkv"))]
use base64::{Engine as _, engine::general_purpose::STANDARD};
#[cfg(any(feature = "mp4", feature = "mkv"))]
use bytes::{Bytes, BytesMut};
#[cfg(any(feature = "mp4", feature = "mkv"))]
use std::ops::Range;
use std::sync::Arc;

pub(crate) const START_CODE: &[u8] = &[0, 0, 0, 1];

#[derive(Clone, Debug, Default)]
#[cfg(any(feature = "mp4", feature = "mkv"))]
pub(crate) struct VideoConfig {
    pub length_size: usize,
    pub parameter_sets: Vec<Bytes>,
    pub fmtp: Option<Arc<str>>,
}

#[cfg(any(feature = "mp4", feature = "mkv"))]
pub(crate) fn avcc_config(data: &[u8]) -> Result<VideoConfig> {
    if data.len() < 7 || data[0] != 1 {
        return Err(Error::Media("invalid AVCDecoderConfigurationRecord".into()));
    }
    let length_size = usize::from((data[4] & 3) + 1);
    let mut position = 6;
    let sps_count = usize::from(data[5] & 0x1f);
    let mut parameter_sets = Vec::new();
    for _ in 0..sps_count {
        parameter_sets.push(read_config_nal(data, &mut position)?);
    }
    let pps_count = *data
        .get(position)
        .ok_or_else(|| Error::Media("truncated AVC configuration".into()))?;
    position += 1;
    for _ in 0..pps_count {
        parameter_sets.push(read_config_nal(data, &mut position)?);
    }
    let sps = parameter_sets
        .iter()
        .find(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 7));
    let pps = parameter_sets
        .iter()
        .find(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 8));
    let fmtp = match (sps, pps) {
        (Some(sps), Some(pps)) if sps.len() >= 4 => Some(Arc::from(format!(
            "packetization-mode=1;profile-level-id={:02X}{:02X}{:02X};sprop-parameter-sets={},{}",
            sps[1],
            sps[2],
            sps[3],
            STANDARD.encode(sps),
            STANDARD.encode(pps)
        ))),
        _ => None,
    };
    Ok(VideoConfig {
        length_size,
        parameter_sets,
        fmtp,
    })
}

#[cfg(any(feature = "mp4", feature = "mkv"))]
pub(crate) fn hvcc_config(data: &[u8]) -> Result<VideoConfig> {
    if data.len() < 23 || data[0] != 1 {
        return Err(Error::Media(
            "invalid HEVCDecoderConfigurationRecord".into(),
        ));
    }
    let length_size = usize::from((data[21] & 3) + 1);
    let array_count = usize::from(data[22]);
    let mut position = 23;
    let mut parameter_sets = Vec::new();
    for _ in 0..array_count {
        let nal_type = *data
            .get(position)
            .ok_or_else(|| Error::Media("truncated HEVC configuration".into()))?
            & 0x3f;
        position += 1;
        let count = read_u16(data, &mut position)?;
        for _ in 0..count {
            let nal = read_config_nal(data, &mut position)?;
            if matches!(nal_type, 32..=34) {
                parameter_sets.push(nal);
            }
        }
    }
    let values = [32u8, 33, 34].map(|kind| {
        parameter_sets
            .iter()
            .find(|nal| nal.first().is_some_and(|byte| (byte >> 1) & 0x3f == kind))
    });
    let fmtp = match values {
        [Some(vps), Some(sps), Some(pps)] => Some(Arc::from(format!(
            "sprop-vps={};sprop-sps={};sprop-pps={}",
            STANDARD.encode(vps),
            STANDARD.encode(sps),
            STANDARD.encode(pps)
        ))),
        _ => None,
    };
    Ok(VideoConfig {
        length_size,
        parameter_sets,
        fmtp,
    })
}

#[cfg(any(feature = "mp4", feature = "mkv"))]
fn read_config_nal(data: &[u8], position: &mut usize) -> Result<Bytes> {
    let length = read_u16(data, position)?;
    let end = position
        .checked_add(length)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| Error::Media("truncated codec parameter set".into()))?;
    let nal = Bytes::copy_from_slice(&data[*position..end]);
    *position = end;
    Ok(nal)
}

#[cfg(any(feature = "mp4", feature = "mkv"))]
fn read_u16(data: &[u8], position: &mut usize) -> Result<usize> {
    let end = position
        .checked_add(2)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| Error::Media("truncated codec configuration".into()))?;
    let value = u16::from_be_bytes([data[*position], data[*position + 1]]) as usize;
    *position = end;
    Ok(value)
}

#[cfg(any(feature = "mp4", feature = "mkv"))]
pub(crate) fn length_prefixed_to_annex_b(
    data: Bytes,
    length_size: usize,
    parameter_sets: &[Bytes],
    inject_parameter_sets: bool,
) -> Result<Bytes> {
    if !(1..=4).contains(&length_size) {
        return Err(Error::Media(
            "NAL length size must be between 1 and 4".into(),
        ));
    }
    let mut position = 0usize;
    let mut ranges: Vec<Range<usize>> = Vec::new();
    while position < data.len() {
        if data.len() - position < length_size {
            return Err(Error::Media("truncated length-prefixed NAL unit".into()));
        }
        let mut length = 0usize;
        for byte in &data[position..position + length_size] {
            length = (length << 8) | usize::from(*byte);
        }
        position += length_size;
        let end = position
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or_else(|| Error::Media("NAL length exceeds sample".into()))?;
        if length != 0 {
            ranges.push(position..end);
        }
        position = end;
    }
    if ranges.is_empty() {
        return Err(Error::Media("sample contains no NAL units".into()));
    }
    if !inject_parameter_sets && ranges.len() == 1 {
        return Ok(data.slice(ranges[0].clone()));
    }
    let capacity = ranges
        .iter()
        .map(|range| START_CODE.len() + range.len())
        .sum::<usize>()
        + if inject_parameter_sets {
            parameter_sets
                .iter()
                .map(|nal| START_CODE.len() + nal.len())
                .sum()
        } else {
            0
        };
    let mut output = BytesMut::with_capacity(capacity);
    if inject_parameter_sets {
        for nal in parameter_sets {
            output.extend_from_slice(START_CODE);
            output.extend_from_slice(nal);
        }
    }
    for range in ranges {
        output.extend_from_slice(START_CODE);
        output.extend_from_slice(&data[range]);
    }
    Ok(output.freeze())
}

pub(crate) fn is_keyframe(codec: Codec, data: &[u8]) -> bool {
    use crate::media::bitstream::{is_h264_idr, is_h265_random_access};
    let matches = |nal: &[u8]| match codec {
        Codec::H264 => is_h264_idr(nal),
        Codec::H265 => is_h265_random_access(nal),
        Codec::Aac
        | Codec::Pcma
        | Codec::Pcmu
        | Codec::Opus
        | Codec::Vp8
        | Codec::Vp9
        | Codec::Mp3
        | Codec::G726_16
        | Codec::G726_24
        | Codec::G726_32
        | Codec::G726_40
        | Codec::Av1
        | Codec::Jpeg
        | Codec::H263
        | Codec::Mpeg4 => true,
    };
    let mut nals = annex_b_nals(data).peekable();
    if nals.peek().is_none() {
        matches(data)
    } else {
        nals.any(matches)
    }
}

#[cfg(feature = "mkv")]
pub(crate) fn prepend_parameter_sets(data: Bytes, parameter_sets: &[Bytes]) -> Bytes {
    if parameter_sets.is_empty() {
        return data;
    }
    let has_start_code = data.starts_with(&[0, 0, 1]) || data.starts_with(START_CODE);
    let mut output = BytesMut::with_capacity(
        parameter_sets
            .iter()
            .map(|nal| START_CODE.len() + nal.len())
            .sum::<usize>()
            + START_CODE.len()
            + data.len(),
    );
    for nal in parameter_sets {
        output.extend_from_slice(START_CODE);
        output.extend_from_slice(nal);
    }
    if !has_start_code {
        output.extend_from_slice(START_CODE);
    }
    output.extend_from_slice(&data);
    output.freeze()
}

pub(crate) fn annex_b_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let marks = start_codes(data);
    let mut index = 0usize;
    std::iter::from_fn(move || {
        while index < marks.len() {
            let (offset, size) = marks[index];
            let start = offset + size;
            let end = marks.get(index + 1).map_or(data.len(), |mark| mark.0);
            index += 1;
            if start < end {
                return Some(&data[start..end]);
            }
        }
        None
    })
}

fn start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut result = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let size = if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else if index + 4 <= data.len() && data[index..].starts_with(START_CODE) {
            4
        } else {
            index += 1;
            continue;
        };
        result.push((index, size));
        index += size;
    }
    result
}

pub(crate) fn aac_audio_specific_config(data: &[u8]) -> Result<(u32, u8, Arc<str>)> {
    let mut bits = BitReader::new(data);
    let object_type = read_audio_object_type(&mut bits)?;
    let sample_rate = read_aac_frequency(&mut bits)?;
    let channel_configuration = bits.read(4)? as u8;
    let output_rate = if matches!(object_type, 5 | 29) {
        let extension_rate = read_aac_frequency(&mut bits)?;
        let _core_object_type = read_audio_object_type(&mut bits)?;
        extension_rate
    } else {
        sample_rate
    };
    let channels = match channel_configuration {
        1..=6 => channel_configuration,
        7 => 8,
        0 => {
            return Err(Error::Media(
                "AAC program-config-element channel layout is unsupported".into(),
            ));
        }
        _ => return Err(Error::Media("unsupported AAC channel configuration".into())),
    };
    let config = data
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    Ok((
        output_rate,
        channels,
        Arc::from(format!(
            "streamtype=5;profile-level-id=1;mode=AAC-hbr;config={config};SizeLength=13;IndexLength=3;IndexDeltaLength=3"
        )),
    ))
}

struct BitReader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn read(&mut self, count: usize) -> Result<u32> {
        if count > 32 || self.position.saturating_add(count) > self.data.len().saturating_mul(8) {
            return Err(Error::Media("truncated AAC AudioSpecificConfig".into()));
        }
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1)
                | u32::from((self.data[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }
}

fn read_audio_object_type(bits: &mut BitReader<'_>) -> Result<u8> {
    let value = bits.read(5)? as u8;
    if value == 31 {
        let extended = bits.read(6)? as u8;
        32u8.checked_add(extended)
            .ok_or(Error::Limit("AAC audio object type"))
    } else {
        Ok(value)
    }
}

fn read_aac_frequency(bits: &mut BitReader<'_>) -> Result<u32> {
    let index = bits.read(4)? as u8;
    if index == 15 {
        let rate = bits.read(24)?;
        if rate == 0 {
            return Err(Error::Media("AAC explicit sample rate is zero".into()));
        }
        Ok(rate)
    } else {
        aac_sample_rate(index).ok_or_else(|| Error::Media("unsupported AAC sample rate".into()))
    }
}

pub(crate) fn aac_sample_rate(index: u8) -> Option<u32> {
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    RATES.get(usize::from(index)).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_and_zero_copy_single_nal() {
        let config = avcc_config(&[
            1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x1f, 1, 0, 2, 0x68, 0xee,
        ])
        .unwrap();
        assert_eq!(config.length_size, 4);
        assert!(config.fmtp.unwrap().contains("profile-level-id=64001F"));
        let source = Bytes::from_static(&[0, 0, 0, 3, 0x65, 1, 2]);
        let converted = length_prefixed_to_annex_b(source.clone(), 4, &[], false).unwrap();
        assert_eq!(&converted[..], &[0x65, 1, 2]);
        assert_eq!(converted.as_ptr(), source[4..].as_ptr());
    }

    #[test]
    fn heaac_uses_extension_rate_and_full_config() {
        let parsed = aac_audio_specific_config(&[0x2b, 0x92, 0x08]).unwrap();
        assert_eq!(parsed.0, 44_100);
        assert_eq!(parsed.1, 2);
        assert!(parsed.2.contains("config=2B9208"));
    }

    #[test]
    fn hvcc_extracts_parameter_sets() {
        let mut config = vec![1; 23];
        config[21] = 3;
        config[22] = 1;
        config.extend_from_slice(&[0xa0, 0, 1, 0, 2, 0x40, 1]);
        let parsed = hvcc_config(&config).unwrap();
        assert_eq!(parsed.length_size, 4);
        assert_eq!(&parsed.parameter_sets[0][..], &[0x40, 1]);
    }
}
