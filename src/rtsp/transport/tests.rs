use super::*;
#[test]
fn rtcp_mux_single_port_and_channel() {
    let udp = TransportSpec::parse("RTP/AVP;unicast;client_port=5000;rtcp-mux").unwrap();
    match udp {
        TransportSpec::Udp {
            client_rtp,
            client_rtcp,
            rtcp_mux,
            ..
        } => {
            assert_eq!((client_rtp, client_rtcp), (5000, 5000));
            assert!(rtcp_mux);
            assert!(udp.render().contains("client_port=5000;rtcp-mux"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let tcp = TransportSpec::parse("RTP/AVP/TCP;unicast;interleaved=4;rtcp-mux").unwrap();
    match tcp {
        TransportSpec::TcpInterleaved {
            rtp_channel,
            rtcp_channel,
            rtcp_mux,
            ..
        } => {
            assert_eq!((rtp_channel, rtcp_channel), (4, 4));
            assert!(rtcp_mux);
            assert!(tcp.render().contains("interleaved=4;rtcp-mux"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(
        TransportSpec::parse("RTP/AVP;unicast;client_port=5000").is_err(),
        "single UDP port requires rtcp-mux"
    );
}

#[test]
fn destination_source_and_mode_round_trip() {
    let udp = TransportSpec::parse(
        "RTP/AVP;unicast;client_port=5000-5001;destination=239.1.1.1;source=192.0.2.7;mode=record",
    )
    .unwrap();
    let rendered = udp.render();
    assert!(rendered.contains("destination=239.1.1.1"), "{rendered}");
    assert!(rendered.contains("source=192.0.2.7"), "{rendered}");
    assert!(rendered.contains("mode=record"), "{rendered}");
    match udp {
        TransportSpec::Udp { mode, source, .. } => {
            assert_eq!(mode, Some(TransportMode::Record));
            assert_eq!(source, Some("192.0.2.7".parse().unwrap()));
        }
        other => panic!("unexpected {other:?}"),
    }
    let tcp = TransportSpec::parse("RTP/AVP/TCP;unicast;interleaved=0-1;mode=play").unwrap();
    assert!(tcp.render().contains("mode=play"));
    let multicast = TransportSpec::parse(
        "RTP/AVP;multicast;destination=239.1.1.1;port=5004;ttl=8;source=192.0.2.7;mode=play",
    )
    .unwrap();
    let rendered = multicast.render();
    assert!(rendered.contains("source=192.0.2.7"), "{rendered}");
    assert!(rendered.contains("mode=play"), "{rendered}");
}

#[test]
fn savp_profiles_set_secure_flag() {
    let udp = TransportSpec::parse("RTP/SAVP;unicast;client_port=5000-5001").unwrap();
    match udp {
        TransportSpec::Udp { secure, .. } => assert!(secure),
        other => panic!("unexpected {other:?}"),
    }
    let tcp = TransportSpec::parse("RTP/SAVP/TCP;unicast;interleaved=0-1").unwrap();
    match tcp {
        TransportSpec::TcpInterleaved { secure, .. } => assert!(secure),
        other => panic!("unexpected {other:?}"),
    }
    assert!(udp.render().starts_with("RTP/SAVP;"));
}

#[test]
fn udp_roundtrip() {
    let t = TransportSpec::parse("RTP/AVP;unicast;client_port=1000-1001").unwrap();
    assert_eq!(t.render(), "RTP/AVP;unicast;client_port=1000-1001");
    assert_eq!(
        TransportSpec::parse("RTP/AVP/UDP;unicast;client_port=1000-1001").unwrap(),
        t
    );
}
#[test]
fn parses_transport_alternatives_in_client_order() {
    let alternatives = TransportSpec::parse_all(
        "RTP/AVP;unicast;client_port=5000-5001, RTP/AVP/TCP;unicast;interleaved=0-1",
    )
    .unwrap();
    assert!(matches!(alternatives[0], TransportSpec::Udp { .. }));
    assert!(matches!(
        alternatives[1],
        TransportSpec::TcpInterleaved { .. }
    ));
    assert!(
        TransportSpec::parse(
            "RTP/AVP;unicast;client_port=5000-5001, RTP/AVP/TCP;unicast;interleaved=0-1"
        )
        .is_err()
    );
}

#[test]
fn rejects_invalid_interleaved_bounds() {
    assert!(TransportSpec::parse("RTP/AVP/TCP;interleaved=256-257").is_err());
    assert!(
        InterleavedFrame {
            channel: 0,
            payload: Bytes::from(vec![0; u16::MAX as usize + 1]),
        }
        .try_encode()
        .is_err()
    );
}
#[test]
fn interleaved_roundtrip() {
    let mut b = BytesMut::from(&b"$\x00\x00\x01x"[..]);
    assert_eq!(
        InterleavedFrame::parse(&mut b, 10)
            .unwrap()
            .unwrap()
            .payload,
        b"x"[..]
    )
}
