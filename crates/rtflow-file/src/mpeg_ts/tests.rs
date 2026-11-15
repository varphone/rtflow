use super::adts::split_adts;
use super::pes::{parse_pts, unwrap_timestamp};
use super::{PTS_MODULUS, TrackState};
use rtflow_media::Codec;
use std::collections::VecDeque;

#[test]
fn timestamp_unwraps_across_33_bit_boundary() {
    let previous = PTS_MODULUS - 5;
    assert_eq!(unwrap_timestamp(3, Some(previous)), PTS_MODULUS + 3);
}

#[test]
fn aac_frame_can_cross_pes_boundaries() {
    let frame = [0xff, 0xf1, 0x4c, 0x80, 0x01, 0x3f, 0xfc, 0xaa, 0xbb];
    let mut track = TrackState {
        pid: 256,
        track_id: 0,
        codec: Codec::Aac,
        clock_rate: 48_000,
        channels: Some(2),
        last_pts: None,
        last_dts: None,
        aac_buffer: Vec::new(),
        aac_next_tick: None,
    };
    let mut output = VecDeque::new();
    split_adts(&frame[..8], 0, &mut track, 1024, &mut output).unwrap();
    assert!(output.is_empty());
    split_adts(&frame[8..], 90_000, &mut track, 1024, &mut output).unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(&output[0].unit.data[..], &[0xaa, 0xbb]);
    assert!(track.aac_buffer.is_empty());
}

#[test]
fn parses_pts() {
    let value = 90_000u64;
    let encoded = [
        0x20 | (((value >> 30) as u8 & 7) << 1) | 1,
        (value >> 22) as u8,
        (((value >> 15) as u8 & 0x7f) << 1) | 1,
        (value >> 7) as u8,
        ((value as u8 & 0x7f) << 1) | 1,
    ];
    assert_eq!(parse_pts(&encoded).unwrap(), value);
}
