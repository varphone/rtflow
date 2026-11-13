use super::*;
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
        // 50ms keeps the test stable when the test binary runs many tests in
        // parallel (a 1ms budget can expire between two pushes).
        max_delay: Duration::from_millis(50),
        ..JitterBufferConfig::default()
    });
    assert_eq!(jitter.push(packet(10)).len(), 1);
    assert!(jitter.push(packet(12)).is_empty());
    std::thread::sleep(Duration::from_millis(60));
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

#[test]
fn h263_rfc2190_round_trip_keeps_start_code() {
    let data = Bytes::from_static(&[0, 0, 0x80, 0x02, 0x08, 0x04, 0x11, 0x22]);
    let unit = AccessUnit {
        track_id: 0,
        codec: Codec::H263,
        pts: std::time::Duration::ZERO,
        dts: None,
        rtp_timestamp: 90_000,
        is_keyframe: true,
        data: data.clone(),
    };
    let mut packetizer = Packetizer::new(Codec::H263, 34, 1, 1200).unwrap();
    let packets = packetizer.packetize(&unit).unwrap();
    let mut depacketizer = Depacketizer::new(Codec::H263, 0, 90_000);
    let mut output = Vec::new();
    for packet in packets {
        output.extend(depacketizer.push(packet).unwrap());
    }
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].data, data);
}
