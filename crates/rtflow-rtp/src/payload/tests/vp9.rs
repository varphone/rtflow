use super::*;

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
                nal_offsets: None,
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
        assert_eq!(packets[0].payload[0], 0x80 | 0x08 | 0x04);
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
                nal_offsets: None,
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
        assert_eq!(f1[0].payload[0] & 0x08, 0x08);
        assert_eq!(f1[0].payload[0] & 0x04, 0);
        let last = f1.last().unwrap();
        assert_eq!(last.payload[0] & 0x04, 0x04);
        assert!(last.header.marker);
        let pid0 = u16::from_be_bytes([f1[0].payload[1] & 0x7f, f1[0].payload[2]]);
        let pid_last = u16::from_be_bytes([last.payload[1] & 0x7f, last.payload[2]]);
        assert_eq!(pid0, pid_last, "picture ID constant within a frame");

        // Second frame: picture ID increments.
        let f2 = pack
            .packetize(&AccessUnit {
                nal_offsets: None,
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
            .push(packet(7, 900, true, &[0x8c, 0x2a, 0xde, 0xad, 0xbe]))
            .unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].data.as_ref(), &[0xde, 0xad, 0xbe]);
    }

    #[test]
    fn vp9_sequence_gap_discards_inflight_frame() {
        let mut dep = Depacketizer::new(Codec::Vp9, 0, 90_000);
        // Fragment 1 of a frame (I=1, M=1, B=1, E=0).
        let _ = dep
            .push(packet(1, 100, false, &[0x88, 0x80, 0x01, 0xaa, 0xbb]))
            .unwrap();
        // Gap (seq 3), new frame I=1 M=1 B=1 E=1.
        let units = dep
            .push(packet(3, 200, true, &[0x8c, 0x80, 0x02, 0xcc]))
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
            .push(packet(1, 500, false, &[0x8c, 0x80, 0x05, 0xde, 0xad]))
            .unwrap();
        assert_eq!(units.len(), 1, "E bit ends the frame even without marker");
        assert_eq!(units[0].data.as_ref(), &[0xde, 0xad]);
    }
}
