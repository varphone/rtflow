use super::avcc::{annex_b_avcc_size, write_annex_b_avcc};
use super::tag::{FLV_TAG_HEADER_SIZE, write_composition_time, write_flv_tag_header};
use bytes::{BufMut, Bytes, BytesMut};
use rtflow_media::AccessUnit;

pub(super) fn write_h264_video_tag(
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

/// audio frames) would otherwise be mistaken for a 3-byte Annex-B start
/// code and corrupted by conversion.
pub(super) fn write_h264_video_tag_inplace(buf: &mut BytesMut, unit: &AccessUnit, data: &Bytes) {
    let converted_size = annex_b_avcc_size(data).unwrap_or(data.len());
    write_flv_tag_header(buf, unit, converted_size + 5);

    let frame_type = if unit.is_keyframe { 1u8 } else { 2u8 };
    buf.put_u8((frame_type << 4) | 7); // 7 = AVC
    buf.put_u8(1); // AVC packet type: NALU

    write_composition_time(buf, unit);

    write_annex_b_avcc(buf, data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + converted_size + 5;
    buf.put_u32(tag_total_size as u32);
}
