use crate::media::{AccessUnit, Codec};
use bytes::{BufMut, Bytes, BytesMut};

pub(super) const FLV_HEADER_SIZE: usize = 9;
pub(super) const FLV_TAG_HEADER_SIZE: usize = 11;

pub(super) fn write_composition_time(buf: &mut BytesMut, unit: &AccessUnit) {
    let dts = unit.dts.unwrap_or(unit.pts);
    crate::media::flv::write_composition_time(
        buf,
        crate::media::flv::composition_time_ms(unit.pts, Some(dts)),
    );
}

/// Write the FLV video tag header.
pub(super) fn write_flv_tag_header(buf: &mut BytesMut, unit: &AccessUnit, body_size: usize) {
    let timestamp = unit.dts.unwrap_or(unit.pts).as_millis() as u32;
    crate::media::flv::write_tag_header(
        buf,
        crate::media::flv::TAG_TYPE_VIDEO,
        body_size,
        timestamp,
    );
}

/// Write an audio FLV tag (AAC).
pub(super) fn write_audio_tag(
    buf: &mut BytesMut,
    unit: &AccessUnit,
    data: &Bytes,
    is_sequence_header: bool,
) {
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
pub(super) fn write_g711_audio_tag(buf: &mut BytesMut, unit: &AccessUnit) {
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
