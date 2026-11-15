use super::sps::{h264_dimensions_from_avcc, h265_dimensions_from_hvcc};
use super::util::codec_config_from_fmtp;
use crate::media::Codec;
use bytes::{BufMut, Bytes, BytesMut};

pub(super) fn write_video_entry(
    buf: &mut BytesMut,
    track: &crate::media::TrackDescription,
    codec_config: Option<&Bytes>,
) {
    let config_from_fmtp = codec_config_from_fmtp(track);
    let codec_config = codec_config.or(config_from_fmtp.as_ref());
    tracing::debug!(
        codec = ?track.codec,
        from_state = codec_config.is_some(),
        from_fmtp_fallback = config_from_fmtp.is_some(),
        config_len = codec_config.map(|c| c.len()).unwrap_or(0),
        "write_video_entry"
    );
    let entry_start = buf.len();
    buf.put_u32(0);
    match track.codec {
        Codec::H265 => buf.put_slice(b"hvc1"),
        Codec::Vp9 => buf.put_slice(b"vp09"),
        Codec::Av1 => buf.put_slice(b"av01"),
        _ => buf.put_slice(b"avc1"),
    }
    buf.put_u32(0);
    buf.put_u16(0);
    buf.put_u16(1);
    buf.put_u16(0);
    buf.put_u16(0);
    buf.put_u32(0);
    buf.put_u32(0);
    buf.put_u32(0);
    let dimensions = match track.codec {
        Codec::H265 => codec_config.and_then(|config| h265_dimensions_from_hvcc(config)),
        Codec::H264 => codec_config.and_then(|config| h264_dimensions_from_avcc(config)),
        Codec::Vp9 | Codec::Av1 => track
            .width
            .zip(track.height)
            .map(|(width, height)| (width as u16, height as u16)),
        _ => None,
    };
    buf.put_u16(dimensions.map_or(0, |(width, _)| width));
    buf.put_u16(dimensions.map_or(0, |(_, height)| height));
    buf.put_u32(0x00480000);
    buf.put_u32(0x00480000);
    buf.put_u32(0);
    buf.put_u16(1);
    let name = b"rtflow";
    buf.put_u8(name.len() as u8);
    buf.put_slice(name);
    for _ in 0..(31 - name.len()) {
        buf.put_u8(0);
    }
    buf.put_u16(0x0018);
    buf.put_i16(-1);

    let config_start = buf.len();
    buf.put_u32(0);
    match track.codec {
        Codec::H265 => buf.put_slice(b"hvcC"),
        Codec::Vp9 => buf.put_slice(b"vpcC"),
        Codec::Av1 => buf.put_slice(b"av1C"),
        _ => buf.put_slice(b"avcC"),
    }
    if let Some(config) = codec_config {
        buf.put_slice(config);
    } else if track.codec == Codec::Vp9 {
        // Minimal VPCodecConfigurationRecord: version 1, profile 0, level 0,
        // 8-bit 4:2:0 limited range, BT.709 colour.
        buf.put_slice(&[1, 0, 10, 0x82, 1, 1, 1, 0, 0]);
    } else if track.codec == Codec::Av1 {
        // Minimal AV1CodecConfigurationRecord: marker/version 1, profile 0,
        // level 0, 8-bit 4:2:0.
        buf.put_slice(&[0x81, 0x00, 0x00, 0x00]);
    }
    let config_size = (buf.len() - config_start) as u32;
    buf[config_start..config_start + 4].copy_from_slice(&config_size.to_be_bytes());

    // pasp box (pixel aspect ratio) - required by some players (Firefox)
    buf.put_u32(16);
    buf.put_slice(b"pasp");
    buf.put_u32(1); // hSpacing
    buf.put_u32(1); // vSpacing

    // btrt box (bitrate) - required by Firefox MSE for init segment validation
    buf.put_u32(20);
    buf.put_slice(b"btrt");
    buf.put_u32(0); // bufferSizeDB
    buf.put_u32(2_000_000); // maxBitrate
    buf.put_u32(2_000_000); // avgBitrate

    let entry_size = (buf.len() - entry_start) as u32;
    buf[entry_start..entry_start + 4].copy_from_slice(&entry_size.to_be_bytes());
}

pub(super) fn write_audio_entry(buf: &mut BytesMut, track: &crate::media::TrackDescription) {
    let entry_start = buf.len();
    buf.put_u32(0); // placeholder
    match track.codec {
        Codec::Aac => buf.put_slice(b"mp4a"),
        Codec::Pcma => buf.put_slice(b"alaw"),
        Codec::Pcmu => buf.put_slice(b"ulaw"),
        // RFC 7845: the Opus sample entry is box type "Opus" followed by
        // a dOps box.
        Codec::Opus => buf.put_slice(b"Opus"),
        _ => buf.put_slice(b"mp4a"),
    }
    buf.put_u32(0); // reserved
    buf.put_u16(0); // reserved
    buf.put_u16(1); // data_reference_index
    buf.put_u32(0); // reserved
    buf.put_u32(0); // reserved
    buf.put_u16(track.channels.unwrap_or(1) as u16); // channelcount
    buf.put_u16(16); // samplesize
    buf.put_u16(0); // pre-defined
    buf.put_u16(0); // reserved
    buf.put_u32(if track.clock_rate > 0xFFFF {
        // 16.16 fixed point cannot represent rates above 65535 Hz
        // (e.g. 96 kHz AAC); 0 lets decoders fall back to the esds config.
        0
    } else {
        track.clock_rate << 16
    }); // samplerate (fixed point)

    // esds box for AAC
    if track.codec == Codec::Aac {
        let config = aac_config_from_fmtp(track)
            .unwrap_or_else(|| default_aac_config(track.clock_rate, track.channels.unwrap_or(1)));
        write_esds(buf, &config);
    }

    // dOps box for Opus (RFC 7845 §5.2): pre_skip of 312 is the RFC
    // recommended default for 48 kHz streams.
    if track.codec == Codec::Opus {
        let dops_start = buf.len();
        buf.put_u32(0); // size placeholder
        buf.put_slice(b"dOps");
        buf.put_u8(0); // version
        buf.put_u8(track.channels.unwrap_or(1)); // channel count
        buf.put_u16(312); // pre_skip
        buf.put_u32(track.clock_rate); // input sample rate
        buf.put_u16(0); // output gain
        buf.put_u8(0); // channel mapping family
        let size = (buf.len() - dops_start) as u32;
        buf[dops_start..dops_start + 4].copy_from_slice(&size.to_be_bytes());
    }

    let entry_size = (buf.len() - entry_start) as u32;
    buf[entry_start..entry_start + 4].copy_from_slice(&entry_size.to_be_bytes());
}

pub(super) fn aac_config_from_fmtp(track: &crate::media::TrackDescription) -> Option<Bytes> {
    let value = track
        .fmtp
        .as_deref()?
        .split(';')
        .find(|part| part.trim().starts_with("config="))?
        .split_once('=')?
        .1
        .trim();
    if value.len() % 2 != 0 {
        return None;
    }
    // Slice by byte range via `get` so a multi-byte UTF-8 sequence in the
    // fmtp (which arrives unvalidated from ANNOUNCE SDP) cannot panic on a
    // non-char-boundary index; an invalid pair simply fails the whole parse.
    let bytes: Option<Vec<u8>> = (0..value.len())
        .step_by(2)
        .map(|index| {
            let pair = value.as_bytes().get(index..index + 2)?;
            u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()
        })
        .collect();
    bytes.filter(|bytes| !bytes.is_empty()).map(Bytes::from)
}

pub(super) fn default_aac_config(clock_rate: u32, channels: u8) -> Bytes {
    crate::media::bitstream::aac_audio_specific_config_bytes(clock_rate, channels)
}

pub(super) fn write_esds(buf: &mut BytesMut, config: &Bytes) {
    let esds_start = buf.len();
    buf.put_u32(0);
    buf.put_slice(b"esds");
    buf.put_u32(0);

    let decoder_config_size = 13 + 2 + config.len();
    let es_descriptor_size = 3 + 2 + decoder_config_size + 3;
    buf.put_u8(0x03);
    buf.put_u8(es_descriptor_size as u8);
    buf.put_u16(0);
    buf.put_u8(0);
    buf.put_u8(0x04);
    buf.put_u8(decoder_config_size as u8);
    buf.put_u8(0x40);
    buf.put_u8(0x15);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u8(0);
    buf.put_u32(128_000);
    buf.put_u32(128_000);
    buf.put_u8(0x05);
    buf.put_u8(config.len() as u8);
    buf.put_slice(config);
    buf.put_u8(0x06);
    buf.put_u8(1);
    buf.put_u8(2);

    let esds_size = (buf.len() - esds_start) as u32;
    buf[esds_start..esds_start + 4].copy_from_slice(&esds_size.to_be_bytes());
}
