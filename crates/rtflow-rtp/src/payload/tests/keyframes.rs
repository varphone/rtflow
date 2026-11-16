use super::*;

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

    /// Random camera RTP timestamps above i32::MAX must not overflow the
    /// wrap extension into a constant saturated PTS.
    #[test]
    fn h264_high_initial_timestamp_has_advancing_pts() {
        let mut d = Depacketizer::new(Codec::H264, 0, 90_000);
        let base = 0xF000_0000u32;
        let first = d.push(packet(1, base, true, &[0x65, 0x01, 0x02])).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].pts, std::time::Duration::ZERO);
        let second = d
            .push(packet(2, base.wrapping_add(3_000), true, &[0x41, 0x03]))
            .unwrap();
        assert_eq!(second.len(), 1);
        assert!(
            second[0].pts >= std::time::Duration::from_millis(33)
                && second[0].pts <= std::time::Duration::from_millis(34),
            "unexpected PTS {:?}",
            second[0].pts
        );
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
