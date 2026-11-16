use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::{BufMut, Bytes, BytesMut};
use rtflow_media::Codec;
use std::time::Duration;

pub(super) fn duration_to_ticks(duration: Duration, timescale: u32) -> u64 {
    // Round to nearest tick instead of truncating: repeated truncation of
    // µs-resolution durations drifts the fMP4 timeline (tfdt) relative to
    // the RTP timeline over long streams.
    rtflow_media::clock::duration_to_ticks_rounded(duration, timescale)
}

pub(super) fn composition_offset_ticks(pts: Duration, dts: Duration, timescale: u32) -> i32 {
    let ticks = if pts >= dts {
        duration_to_ticks(pts - dts, timescale) as i64
    } else {
        -(duration_to_ticks(dts - pts, timescale) as i64)
    };
    ticks.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}
pub(super) fn annex_b_to_length_prefixed(codec: Codec, data: &Bytes) -> Bytes {
    // Fast path: already length-prefixed (RTMP ingest delivers AVCC/HVCC).
    // Reference-count clone only; no full-frame scan or rebuild.
    if data.len() >= 4
        && u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize == data.len() - 4
    {
        return data.clone();
    }
    let nals = rtflow_media::bitstream::annex_b_nals(data);
    if nals.is_empty() {
        return data.clone();
    }
    let mut output = BytesMut::with_capacity(data.len());
    for nal in nals {
        if nal.is_empty() {
            // Consecutive Annex-B start codes from a publisher can yield an
            // empty NAL. Writing a zero length prefix makes every decoder
            // reject the following samples ("Invalid NAL unit size 0").
            continue;
        }
        let parameter_set = match codec {
            Codec::H264 => nal.first().is_some_and(|byte| matches!(byte & 0x1f, 7 | 8)),
            Codec::H265 => nal
                .first()
                .is_some_and(|byte| matches!((byte >> 1) & 0x3f, 32..=34)),
            _ => false,
        };
        if parameter_set {
            continue;
        }
        output.put_u32(nal.len() as u32);
        output.put_slice(nal);
    }
    output.freeze()
}
pub(super) fn codec_config_from_annex_b(codec: Codec, data: &Bytes) -> Option<Bytes> {
    let nals = rtflow_media::bitstream::annex_b_nals(data);
    match codec {
        Codec::H264 => {
            let sps = nals
                .iter()
                .find(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 7))?;
            let pps = nals
                .iter()
                .find(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 8))?;
            build_avcc(sps, pps)
        }
        Codec::H265 => {
            let find = |kind| {
                nals.iter()
                    .find(|nal| nal.first().is_some_and(|byte| (byte >> 1) & 0x3f == kind))
            };
            build_hvcc(find(32)?, find(33)?, find(34)?)
        }
        _ => None,
    }
}
pub(super) fn codec_config_from_fmtp(track: &rtflow_media::TrackDescription) -> Option<Bytes> {
    let fmtp = track.fmtp.as_deref()?;
    match track.codec {
        Codec::H264 => {
            let values = fmtp
                .split(';')
                .find(|value| value.trim().starts_with("sprop-parameter-sets"))?
                .split_once('=')?
                .1;
            let mut nals = values
                .split(',')
                .filter_map(|value| STANDARD.decode(value.trim()).ok());
            build_avcc(&nals.next()?, &nals.next()?)
        }
        Codec::H265 => {
            let value = |key: &str| {
                fmtp.split(';')
                    .find(|part| part.trim().starts_with(key))
                    .and_then(|part| {
                        part.split_once('=')
                            .map(|(_, value)| STANDARD.decode(value.trim()).ok())
                    })
                    .flatten()
            };
            build_hvcc(
                &value("sprop-vps")?,
                &value("sprop-sps")?,
                &value("sprop-pps")?,
            )
        }
        Codec::Vp9 | Codec::Av1 => {
            let value = fmtp
                .split(';')
                .find(|part| part.trim().starts_with("config="))?
                .split_once('=')?
                .1
                .trim();
            let bytes: Option<Vec<u8>> = (0..value.len())
                .step_by(2)
                .map(|index| {
                    let pair = value.as_bytes().get(index..index + 2)?;
                    u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()
                })
                .collect();
            bytes.filter(|bytes| !bytes.is_empty()).map(Bytes::from)
        }
        _ => None,
    }
}
pub(super) fn build_avcc(sps: &[u8], pps: &[u8]) -> Option<Bytes> {
    rtflow_media::bitstream::build_avc_config_record(sps, pps)
}

pub(super) fn build_hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Option<Bytes> {
    rtflow_media::bitstream::build_hevc_config_record(vps, sps, pps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segmenter::Codec;
    use bytes::Bytes;

    #[test]
    fn skips_empty_annex_b_nal_units() {
        let data = Bytes::from_static(&[0, 0, 0, 1, 0, 0, 0, 1, 0x65, 0x88, 0x84]);
        let output = annex_b_to_length_prefixed(Codec::H264, &data);
        assert_eq!(&output[..4], &[0, 0, 0, 3]);
        assert_eq!(&output[4..], &[0x65, 0x88, 0x84]);
    }
}
