use super::adts::read_adts_header;
use super::annexb::{first_mb_in_slice_is_zero, nal_properties};
use super::*;

#[test]
fn h264_slice_boundary_detection() {
    assert_eq!(
        nal_properties(Codec::H264, &[0x65, 0x80]),
        (true, true, false, true)
    );
    assert!(!first_mb_in_slice_is_zero(&[0x40]));
}

#[test]
fn adts_header_fields() {
    let bytes = [0xff, 0xf1, 0x4c, 0x80, 0x01, 0x3f, 0xfc];
    let header = read_adts_header(&mut &bytes[..]).unwrap().unwrap();
    assert_eq!(header.sample_rate, 48_000);
    assert_eq!(header.channels, 2);
    assert_eq!(header.frame_length, 9);
}
