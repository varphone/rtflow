use super::avcc::{annex_b_avcc_size, write_annex_b_avcc};
use super::tag::{FLV_TAG_HEADER_SIZE, write_flv_tag_header};
use crate::media::AccessUnit;
use bytes::{BufMut, Bytes, BytesMut};

pub(super) fn write_h265_video_tag(
    buf: &mut BytesMut,
    unit: &AccessUnit,
    data: &Bytes,
    is_sequence_header: bool,
) {
    let (frame_type, packet_type): (u8, u8) = if is_sequence_header {
        (1, 0) // keyframe + SequenceStart
    } else if unit.is_keyframe {
        (1, 3) // keyframe + CodedFramesX
    } else {
        (2, 3) // inter + CodedFramesX
    };

    let body_size = 1 + 4 + data.len(); // header + FourCC + payload

    write_flv_tag_header(buf, unit, body_size);

    // Enhanced RTMP header byte: PacketType is embedded in bits 0-3
    buf.put_u8(0x80 | (frame_type << 4) | packet_type);

    // FourCC: "hvc1"
    buf.put_slice(b"hvc1");

    // Data: HVCC (SequenceStart) or AVCC NALUs (CodedFramesX)
    buf.put_slice(data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + body_size;
    buf.put_u32(tag_total_size as u32);
}

/// H.265 video tag with Annex-B → length-prefixed conversion performed
/// directly into `buf` (see [`write_h264_video_tag_inplace`]).
pub(super) fn write_h265_video_tag_inplace(buf: &mut BytesMut, unit: &AccessUnit, data: &Bytes) {
    let converted_size = annex_b_avcc_size(data).unwrap_or(data.len());
    let body_size = 1 + 4 + converted_size;

    write_flv_tag_header(buf, unit, body_size);

    let frame_type = if unit.is_keyframe { 1u8 } else { 2u8 };
    // Enhanced RTMP header byte: keyframe + CodedFramesX (no CompositionTime)
    buf.put_u8(0x80 | (frame_type << 4) | 3);

    buf.put_slice(b"hvc1");

    write_annex_b_avcc(buf, data);

    let tag_total_size = FLV_TAG_HEADER_SIZE + body_size;
    buf.put_u32(tag_total_size as u32);
}
