use super::packetizer::annex_b_nals;
use super::{Depacketizer, Packetizer};
use crate::{
    Error,
    media::Codec,
    rtp::{RtpHeader, RtpPacket},
};
use bytes::Bytes;

use crate::{
    media::AccessUnit,
    rtp::{JitterBuffer, JitterBufferConfig},
};
use std::time::Duration;
fn frame(c: Codec, data: &[u8]) -> AccessUnit {
    AccessUnit {
        track_id: 0,
        codec: c,
        pts: Duration::ZERO,
        dts: None,
        rtp_timestamp: 7,
        is_keyframe: true,
        data: Bytes::copy_from_slice(data),
    }
}
#[test]
fn packet_view() {
    let p = RtpPacket {
        header: RtpHeader {
            marker: true,
            payload_type: 96,
            sequence: 1,
            timestamp: 2,
            ssrc: 3,
        },
        payload: Bytes::from_static(b"x"),
    };
    assert_eq!(RtpPacket::parse(p.encode()).unwrap().payload, b"x"[..]);
}
#[test]
fn single_packet_fus_emit_an_access_unit() {
    let packet = |payload: &'static [u8]| RtpPacket {
        header: RtpHeader {
            marker: true,
            payload_type: 96,
            sequence: 1,
            timestamp: 2,
            ssrc: 3,
        },
        payload: Bytes::from_static(payload),
    };
    let mut h264 = Depacketizer::new(Codec::H264, 0, 90_000);
    let h264_out = h264.push(packet(&[28, 0xc5, 1, 2, 3])).unwrap();
    assert_eq!(h264_out.len(), 1);
    assert_eq!(
        h264_out[0].data,
        Bytes::from_static(&[0, 0, 0, 1, 0x05, 1, 2, 3])
    );

    let mut h265 = Depacketizer::new(Codec::H265, 0, 90_000);
    let h265_out = h265.push(packet(&[49 << 1, 1, 0xc2, 3, 4])).unwrap();
    assert_eq!(h265_out.len(), 1);
    assert_eq!(
        h265_out[0].data,
        Bytes::from_static(&[0, 0, 0, 1, 4, 1, 3, 4])
    );
}

#[test]
fn h264_fu_roundtrip() {
    let f = frame(Codec::H264, &[0x65; 300]);
    let mut p = Packetizer::new(Codec::H264, 96, 1, 80).unwrap();
    let mut d = Depacketizer::new(Codec::H264, 0, 90000);
    let packets = p.packetize(&f).unwrap();
    let mut out = None;
    for x in packets {
        out = d.push(x).unwrap().into_iter().last().or(out)
    }
    let expected: Vec<u8> = [&[0u8, 0, 0, 1][..], &[0x65u8; 300][..]].concat();
    assert_eq!(out.unwrap().data, Bytes::from(expected))
}
#[test]
fn video_access_units_preserve_multiple_nals() {
    for (codec, input) in [
        (Codec::H264, &[0, 0, 1, 0x61, 1, 0, 0, 1, 0x65, 2][..]),
        (
            Codec::H265,
            &[0, 0, 1, 2 << 1, 1, 1, 0, 0, 1, 19 << 1, 1, 2][..],
        ),
    ] {
        let f = frame(codec, input);
        let mut packetizer = Packetizer::new(codec, 96, 1, 1200).unwrap();
        let mut depacketizer = Depacketizer::new(
            codec,
            0,
            if matches!(codec, Codec::H264 | Codec::H265) {
                90000
            } else if codec == Codec::Aac {
                48000
            } else {
                8000
            },
        );
        let mut output = None;
        for packet in packetizer.packetize(&f).unwrap() {
            output = depacketizer
                .push(packet)
                .unwrap()
                .into_iter()
                .last()
                .or(output);
        }
        let output = output.unwrap();
        assert_eq!(annex_b_nals(&output.data).len(), 2);
        assert!(output.is_keyframe);
    }
}
#[test]
fn h264_stap_a_and_h265_ap() {
    let packet = |payload: &'static [u8]| RtpPacket {
        header: RtpHeader {
            marker: true,
            payload_type: 96,
            sequence: 1,
            timestamp: 2,
            ssrc: 3,
        },
        payload: Bytes::from_static(payload),
    };
    let mut h264 = Depacketizer::new(Codec::H264, 0, 90000);
    let out = h264
        .push(packet(&[24, 0, 2, 0x61, 1, 0, 2, 0x65, 2]))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(annex_b_nals(&out.data).len(), 2);
    assert!(out.is_keyframe);
    let mut h265 = Depacketizer::new(Codec::H265, 0, 90000);
    let out = h265
        .push(packet(&[
            48 << 1,
            1,
            0,
            3,
            2 << 1,
            1,
            1,
            0,
            3,
            19 << 1,
            1,
            2,
        ]))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(annex_b_nals(&out.data).len(), 2);
    assert!(out.is_keyframe);
}
#[test]
fn audio_roundtrips() {
    for codec in [Codec::Aac, Codec::Pcma, Codec::Pcmu] {
        let f = frame(codec, &[1, 2, 3, 4]);
        let mut packetizer = Packetizer::new(codec, 96, 1, 1200).unwrap();
        let mut depacketizer = Depacketizer::new(
            codec,
            0,
            if matches!(codec, Codec::H264 | Codec::H265) {
                90000
            } else if codec == Codec::Aac {
                48000
            } else {
                8000
            },
        );
        let packet = packetizer.packetize(&f).unwrap().remove(0);
        if codec == Codec::Aac {
            assert_eq!(&packet.payload[..4], &[0, 16, 0, 32]);
        }
        assert_eq!(
            depacketizer
                .push(packet)
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
                .data,
            f.data
        );
    }
}
#[test]
fn jitter_timer_releases_after_a_gap_without_a_new_packet() {
    let packet = |sequence| RtpPacket {
        header: RtpHeader {
            marker: true,
            payload_type: 96,
            sequence,
            timestamp: sequence as u32,
            ssrc: 1,
        },
        payload: Bytes::from_static(b"x"),
    };
    let mut jitter = JitterBuffer::new(JitterBufferConfig {
        max_delay: Duration::from_millis(1),
        ..JitterBufferConfig::default()
    });
    assert_eq!(jitter.push(packet(10)).len(), 1);
    assert!(jitter.push(packet(12)).is_empty());
    std::thread::sleep(Duration::from_millis(2));
    assert_eq!(jitter.drain_expired()[0].header.sequence, 12);
}

#[test]
fn video_access_unit_is_dropped_after_sequence_loss() {
    let packet = |sequence, timestamp, marker, payload| RtpPacket {
        header: RtpHeader {
            marker,
            payload_type: 96,
            sequence,
            timestamp,
            ssrc: 1,
        },
        payload,
    };
    let mut depacketizer = Depacketizer::new(Codec::H264, 0, 90000);
    assert!(
        depacketizer
            .push(packet(1, 10, false, Bytes::from_static(&[0x61, 1])))
            .unwrap()
            .is_empty()
    );
    assert!(
        depacketizer
            .push(packet(3, 10, true, Bytes::from_static(&[0x65, 2])))
            .unwrap()
            .is_empty()
    );
    assert!(
        !depacketizer
            .push(packet(4, 11, true, Bytes::from_static(&[0x65, 3])))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn jitter_reorders_deduplicates_and_wraps() {
    let packet = |sequence| RtpPacket {
        header: RtpHeader {
            marker: true,
            payload_type: 96,
            sequence,
            timestamp: sequence as u32,
            ssrc: 1,
        },
        payload: Bytes::from_static(b"x"),
    };
    let mut jitter = JitterBuffer::new(JitterBufferConfig::default());
    assert_eq!(jitter.push(packet(10))[0].header.sequence, 10);
    assert!(jitter.push(packet(12)).is_empty());
    let ready = jitter.push(packet(11));
    assert_eq!(
        ready.iter().map(|p| p.header.sequence).collect::<Vec<_>>(),
        vec![11, 12]
    );
    assert!(jitter.push(packet(11)).is_empty());
    jitter.reset();
    assert_eq!(jitter.push(packet(u16::MAX))[0].header.sequence, u16::MAX);
    assert_eq!(jitter.push(packet(0))[0].header.sequence, 0);
}

mod fu_keyframe_tests {
    use super::*;

    fn packet(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: 96,
                sequence: seq,
                timestamp: ts,
                ssrc: 1,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    /// A multi-packet H.264 IDR (FU-A: 3 fragments) must assemble with
    /// `is_keyframe = true` — the FU start packet carries the NAL type,
    /// which must survive to the FU end packet.
    #[test]
    fn h264_fu_a_multi_fragment_idr_is_keyframe() {
        let mut d = Depacketizer::new(Codec::H264, 0, 90_000);
        let ts = 1000u32;
        // FU-A start: NAL type 5 (IDR), S=1 E=0.
        let fu_start = [0x7c, 0x85, 0x01, 0x02, 0x03];
        let units = d.push(packet(10, ts, false, &fu_start)).unwrap();
        assert!(units.is_empty());
        // FU-A continuation: S=0 E=0.
        let fu_mid = [0x7c, 0x05, 0x04, 0x05];
        assert!(d.push(packet(11, ts, false, &fu_mid)).unwrap().is_empty());
        // FU-A end: S=0 E=1, marker set.
        let fu_end = [0x7c, 0x45, 0x06];
        let units = d.push(packet(12, ts, true, &fu_end)).unwrap();
        assert_eq!(units.len(), 1, "one access unit");
        assert!(units[0].is_keyframe, "FU-A IDR must be a keyframe");
        // Annex-B output: 00 00 00 01 65 ...
        assert_eq!(&units[0].data[..5], &[0, 0, 0, 1, 0x65]);
    }

    /// A multi-packet H.264 P-frame (FU-A, NAL type 1) must NOT be marked
    /// as a keyframe.
    #[test]
    fn h264_fu_a_multi_fragment_delta_is_not_keyframe() {
        let mut d = Depacketizer::new(Codec::H264, 0, 90_000);
        let ts = 2000u32;
        let fu_start = [0x7c, 0x81, 0x01];
        assert!(d.push(packet(20, ts, false, &fu_start)).unwrap().is_empty());
        let fu_end = [0x7c, 0x41, 0x02];
        let units = d.push(packet(21, ts, true, &fu_end)).unwrap();
        assert_eq!(units.len(), 1);
        assert!(!units[0].is_keyframe, "P-frame FU-A must not be a keyframe");
    }

    /// H.265 FU (type 49) IDR (NAL type 19) across fragments.
    #[test]
    fn h265_fu_multi_fragment_idr_is_keyframe() {
        let mut d = Depacketizer::new(Codec::H265, 0, 90_000);
        let ts = 3000u32;
        // FU header byte 0: type 49 << 1 | layer; byte 2: S=1 E=0, type 19.
        let fu_start = [0x62, 0x01, 0x93, 0xaa, 0xbb];
        assert!(d.push(packet(30, ts, false, &fu_start)).unwrap().is_empty());
        let fu_end = [0x62, 0x01, 0x53, 0xcc];
        let units = d.push(packet(31, ts, true, &fu_end)).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].is_keyframe, "H265 FU IDR must be a keyframe");
    }
}

mod audit_tests {
    use super::*;

    fn opus_packet(payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker: true,
                payload_type: 120,
                sequence: 1,
                timestamp: 1,
                ssrc: 0,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn opus_depacketizes_single_packet() {
        let mut dep = Depacketizer::new(Codec::Opus, 0, 48_000);
        let unit = dep.push(opus_packet(&[0xf8, 0xff, 0xfe, 0x00])).unwrap();
        assert_eq!(unit.len(), 1);
        assert_eq!(unit[0].data.as_ref(), &[0xf8, 0xff, 0xfe, 0x00]);
        assert!(unit[0].is_keyframe);
    }

    /// Unsupported codecs must surface a typed error, never a panic.
    #[test]
    fn unsupported_codec_returns_error_not_panic() {
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let err = dep.push(opus_packet(&[0x80])).unwrap_err();
        assert!(
            matches!(err, Error::Media(_)),
            "typed error for unsupported codec: {err:?}"
        );
    }
}

mod vp8_tests {
    use super::*;
    use std::time::Duration;

    fn vp8_keyframe() -> Bytes {
        // VP8 frame header: P bit (bit 0) = 0 → keyframe.
        Bytes::from(vec![
            0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x00, 0x00, 0x00, 0x00,
        ])
    }

    fn packet(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: 96,
                sequence: seq,
                timestamp: ts,
                ssrc: 1,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn vp8_roundtrip_single_packet() {
        let frame = vp8_keyframe();
        let mut pack = Packetizer::new(Codec::Vp8, 0, 90_000, 1200).unwrap();
        let packets = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp8,
                pts: Duration::from_millis(0),
                dts: None,
                rtp_timestamp: 1000,
                is_keyframe: true,
                data: frame.clone(),
            })
            .unwrap();
        assert_eq!(packets.len(), 1);
        // Descriptor: S=1, PID=0 → 0x10; marker set.
        assert!(packets[0].header.marker);
        assert_eq!(packets[0].payload[0], 0x10);

        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        let units = dep.push(packets[0].clone()).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, frame);
        assert!(units[0].is_keyframe, "P bit 0 = keyframe");
    }

    #[test]
    fn vp8_roundtrip_fragmented() {
        // Frame larger than MTU → multiple fragments with S/PID/marker.
        let frame: Bytes = {
            let mut v = vp8_keyframe().to_vec();
            v.resize(3000, 0xab);
            Bytes::from(v)
        };
        let mut pack = Packetizer::new(Codec::Vp8, 0, 90_000, 1200).unwrap();
        let packets = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp8,
                pts: Duration::from_millis(0),
                dts: None,
                rtp_timestamp: 2000,
                is_keyframe: true,
                data: frame.clone(),
            })
            .unwrap();
        assert!(packets.len() >= 2, "large frame is fragmented");
        // Descriptor flags: first S=1, last marker=1, PID increments.
        assert_eq!(packets[0].payload[0] & 0x10, 0x10, "S on first fragment");
        assert_eq!(packets[0].payload[0] & 0x0f, 0, "PID 0");
        assert!(packets.last().unwrap().header.marker, "marker on last");

        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        let mut units = Vec::new();
        for p in &packets {
            units.extend(dep.push(p.clone()).unwrap());
        }
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data, frame, "fragments reassembled");
        assert!(units[0].is_keyframe);
    }

    #[test]
    fn vp8_continuation_without_start_is_dropped() {
        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        // First fragment lost: a continuation packet must not panic and
        // must not emit a frame.
        let units = dep
            .push(packet(5, 3000, false, &[0x01, 0x02, 0x03]))
            .unwrap();
        assert!(units.is_empty());
        // The next complete frame recovers normally.
        let p = packet(6, 3000, true, &[0x10, 0x00, 0x02]);
        let units = dep.push(p).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].is_keyframe);
    }

    #[test]
    fn vp8_interframe_is_not_keyframe() {
        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        let units = dep
            .push(packet(1, 4000, true, &[0x10, 0x01, 0x02]))
            .unwrap();
        assert!(!units[0].is_keyframe, "P bit 1 = interframe");
    }
}

mod vp8_gap_tests {
    use super::*;

    fn packet(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: 96,
                sequence: seq,
                timestamp: ts,
                ssrc: 1,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    /// A sequence gap mid-frame must discard the in-flight fragments:
    /// splicing fragments of two frames would emit a corrupted frame.
    #[test]
    fn vp8_sequence_gap_discards_inflight_frame() {
        let mut dep = Depacketizer::new(Codec::Vp8, 0, 90_000);
        // Frame 1: fragments seq 1 (S) ... seq 2.
        let _ = dep
            .push(packet(1, 100, false, &[0x10, 0x00, 0x01]))
            .unwrap();
        // Gap: seq 4 (not 3) — starts a new frame with S=1.
        let units = dep.push(packet(4, 200, true, &[0x10, 0x00, 0x02])).unwrap();
        assert_eq!(units.len(), 1, "new frame still delivered");
        assert_eq!(units[0].data.as_ref(), &[0x00, 0x02]);
        // The spliced buffer must NOT contain bytes from the abandoned frame.
        assert_ne!(units[0].data.as_ref(), &[0x00, 0x01, 0x00, 0x02]);
    }
}

mod vp9_tests {
    use super::*;
    use std::time::Duration;

    fn vp9_keyframe() -> Vec<u8> {
        // VP9 uncompressed header (profile 0): frame_type bit (0x10)
        // = 0 → keyframe; a few more header bytes to be realistic.
        vec![0x82, 0x49, 0x83, 0x42, 0x00, 0x01, 0x02, 0x03]
    }

    fn packet(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> RtpPacket {
        RtpPacket {
            header: RtpHeader {
                marker,
                payload_type: 98,
                sequence: seq,
                timestamp: ts,
                ssrc: 1,
            },
            payload: Bytes::copy_from_slice(payload),
        }
    }

    #[test]
    fn vp9_single_packet_round_trip() {
        let frame_data = vp9_keyframe();
        let mut pack = Packetizer::new(Codec::Vp9, 98, 7, 1400).unwrap();
        let packets = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp9,
                pts: Duration::from_millis(33000),
                dts: None,
                rtp_timestamp: 33_000,
                is_keyframe: true,
                data: Bytes::from(frame_data.clone()),
            })
            .unwrap();
        assert_eq!(packets.len(), 1);
        // Descriptor: I=1, B=1, E=1.
        assert_eq!(packets[0].payload[0], 0x80 | 0x04 | 0x02);
        assert_eq!(
            packets[0].payload[1] & 0x80,
            0x80,
            "M=1 (15-bit picture ID)"
        );
        assert_eq!(
            u16::from_be_bytes([packets[0].payload[1] & 0x7f, packets[0].payload[2]]),
            0
        );
        assert_eq!(&packets[0].payload[3..], &frame_data[..]);
        assert!(packets[0].header.marker);

        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let units = dep.push(packets[0].clone()).unwrap();
        assert_eq!(units.len(), 1);
        assert!(units[0].is_keyframe, "VP9 frame_type bit 0 = keyframe");
        assert_eq!(units[0].data.as_ref(), &frame_data[..]);
        assert_eq!(units[0].rtp_timestamp, 33_000);
    }

    #[test]
    fn vp9_fragmented_round_trip_and_picture_id() {
        let frame_data: Vec<u8> = {
            let mut v = vp9_keyframe();
            v.extend(std::iter::repeat_n(0x5a, 3000));
            v
        };
        let mut pack = Packetizer::new(Codec::Vp9, 98, 7, 1200).unwrap();
        let f1 = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp9,
                pts: Duration::ZERO,
                dts: None,
                rtp_timestamp: 0,
                is_keyframe: true,
                data: Bytes::from(frame_data.clone()),
            })
            .unwrap();
        assert!(f1.len() >= 3, "fragmented into {}", f1.len());
        // First fragment: B=1, E=0; last: B=0, E=1; picture ID constant.
        assert_eq!(f1[0].payload[0] & 0x04, 0x04);
        assert_eq!(f1[0].payload[0] & 0x02, 0);
        let last = f1.last().unwrap();
        assert_eq!(last.payload[0] & 0x02, 0x02);
        assert!(last.header.marker);
        let pid0 = u16::from_be_bytes([f1[0].payload[1] & 0x7f, f1[0].payload[2]]);
        let pid_last = u16::from_be_bytes([last.payload[1] & 0x7f, last.payload[2]]);
        assert_eq!(pid0, pid_last, "picture ID constant within a frame");

        // Second frame: picture ID increments.
        let f2 = pack
            .packetize(&AccessUnit {
                track_id: 0,
                codec: Codec::Vp9,
                pts: Duration::from_millis(3000),
                dts: None,
                rtp_timestamp: 3000,
                is_keyframe: false,
                data: Bytes::from(vec![0x11, 0x02, 0x03]),
            })
            .unwrap();
        let pid1 = u16::from_be_bytes([f2[0].payload[1] & 0x7f, f2[0].payload[2]]);
        assert_eq!(pid1, pid0.wrapping_add(1));

        // Reassemble: feed every fragment of both frames.
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let mut out = Vec::new();
        let mut seq = 0u16;
        for p in f1.iter().chain(f2.iter()) {
            seq = seq.wrapping_add(1);
            out.extend(
                dep.push(packet(seq, p.header.timestamp, p.header.marker, &p.payload))
                    .unwrap(),
            );
        }
        assert_eq!(out.len(), 2, "both frames reassembled");
        assert!(out[0].is_keyframe);
        assert!(!out[1].is_keyframe);
        assert_eq!(out[0].data.as_ref(), &frame_data[..]);
        assert_eq!(out[1].data.as_ref(), &[0x11, 0x02, 0x03]);
    }

    #[test]
    fn vp9_seven_bit_picture_id_descriptor() {
        // ffmpeg/libwebrtc may send I=1 with M=0: a 7-bit picture ID in
        // octet 1, so the descriptor is 2 bytes total. The depacketizer
        // must find the payload at offset 2, not 3.
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let units = dep
            .push(packet(7, 900, true, &[0x86, 0x2a, 0xde, 0xad, 0xbe]))
            .unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data.as_ref(), &[0xde, 0xad, 0xbe]);
    }

    #[test]
    fn vp9_sequence_gap_discards_inflight_frame() {
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        // Fragment 1 of a frame (I=1, M=1, B=1, E=0).
        let _ = dep
            .push(packet(1, 100, false, &[0x84, 0x80, 0x01, 0xaa, 0xbb]))
            .unwrap();
        // Gap (seq 3), new frame I=1 M=1 B=1 E=1.
        let units = dep
            .push(packet(3, 200, true, &[0x86, 0x80, 0x02, 0xcc]))
            .unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(
            units[0].data.as_ref(),
            &[0xcc],
            "abandoned fragments dropped"
        );
    }

    #[test]
    fn vp9_end_bit_without_marker_completes_frame() {
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        let units = dep
            .push(packet(1, 500, false, &[0x86, 0x80, 0x05, 0xde, 0xad]))
            .unwrap();
        assert_eq!(units.len(), 1, "E bit ends the frame even without marker");
        assert_eq!(units[0].data.as_ref(), &[0xde, 0xad]);
    }
}
