use super::*;

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
                nal_offsets: None,
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
                nal_offsets: None,
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
