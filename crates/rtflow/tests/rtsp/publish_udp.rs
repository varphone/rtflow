use bytes::{Bytes, BytesMut};
use rtflow::{
    Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig, TrackDescription,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
    rtcp::Packet as RtcpPacket,
    rtp::{RtpHeader, RtpPacket},
    rtsp::{self as wire_rtsp, Message, Response},
    transport::{TransportSpec, UdpPair},
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

fn description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("udp-publish"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("camera-video"),
            crypto: None,
            width: None,
            height: None,
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            rtx: None,
        }],
        connection: None,
    }
}

fn announced_sdp() -> &'static str {
    "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=udp-publish\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:camera-video\r\n"
}

async fn request(stream: &mut TcpStream, wire: &str) -> Response {
    stream.write_all(wire.as_bytes()).await.unwrap();
    let mut input = BytesMut::new();
    let mut buffer = [0u8; 2048];
    loop {
        if let Some(Message::Response(response)) =
            wire_rtsp::parse(&mut input, 16 * 1024, 256 * 1024).unwrap()
        {
            return response;
        }
        let size = stream.read(&mut buffer).await.unwrap();
        assert!(size > 0);
        input.extend_from_slice(&buffer[..size]);
    }
}

#[tokio::test]
async fn udp_record_is_forwarded_to_tcp_player() {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.rtcp_report_interval = Duration::from_millis(100);
    let server = RtspServer::bind(config).await.unwrap();
    let mount = server
        .register_mount(
            Arc::from("/published"),
            MountConfig::Publisher {
                description: description(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let base = format!("rtsp://{}/published", server.local_addr());
    let mut player = RtspClient::connect(
        url::Url::parse(&base).unwrap(),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();

    let pair = UdpPair::bind("127.0.0.1".parse().unwrap(), None)
        .await
        .unwrap();
    let client_rtp = pair.rtp_port().unwrap();
    let client_rtcp = pair.rtcp_port().unwrap();
    let mut control = TcpStream::connect(server.local_addr()).await.unwrap();
    let announce = request(
        &mut control,
        &format!(
            "ANNOUNCE {base} RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
            announced_sdp().len(),
            announced_sdp()
        ),
    )
    .await;
    assert_eq!(announce.status, 200);
    let setup = request(
        &mut control,
        &format!(
            "SETUP {base}/camera-video RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP;unicast;client_port={client_rtp}-{client_rtcp}\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 200);
    let transport = TransportSpec::parse(setup.headers.get("transport").unwrap()).unwrap();
    let TransportSpec::Udp {
        server_rtp: Some(server_rtp),
        server_rtcp: Some(server_rtcp),
        ..
    } = transport
    else {
        panic!("server did not return UDP RTP/RTCP ports");
    };
    let session = setup.headers.get("session").unwrap();
    let record = request(
        &mut control,
        &format!("RECORD {base} RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\n\r\n"),
    )
    .await;
    assert_eq!(record.status, 200);

    let packet = RtpPacket {
        header: RtpHeader {
            marker: true,
            payload_type: 96,
            sequence: 1,
            timestamp: 90_000,
            ssrc: 55,
        },
        payload: Bytes::from_static(&[0x65, 3, 2, 1]),
    };
    pair.rtp
        .send_to(
            &packet.encode(),
            std::net::SocketAddr::new(server.local_addr().ip(), server_rtp),
        )
        .await
        .unwrap();
    let received = tokio::time::timeout(Duration::from_secs(1), player.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        received.data,
        Bytes::from_static(&[0, 0, 0, 1, 0x65, 3, 2, 1])
    );
    assert!(mount.snapshot().publisher_connected);

    let mut rtcp = [0u8; 2048];
    let (size, _) = tokio::time::timeout(Duration::from_secs(2), pair.rtcp.recv_from(&mut rtcp))
        .await
        .expect("server RR timeout")
        .unwrap();
    let packets = RtcpPacket::parse(Bytes::copy_from_slice(&rtcp[..size])).unwrap();
    assert!(
        packets.iter().any(|packet| matches!(
            packet,
            RtcpPacket::ReceiverReport { .. } | RtcpPacket::ReceiverReportWithReports { .. }
        )),
        "no ReceiverReport in {packets:?}"
    );
    assert!(mount.snapshot().stats.rtcp_receiver_reports > 0);

    let report = rtflow::rtcp::CompoundPacket::new(vec![RtcpPacket::SenderReport {
        ssrc: 55,
        ntp: 0x1122_3344_5566_7788,
        rtp_timestamp: 90_000,
        packets: 1,
        octets: 4,
    }])
    .unwrap()
    .try_encode()
    .unwrap();
    pair.rtcp
        .send_to(
            &report,
            std::net::SocketAddr::new(server.local_addr().ip(), server_rtcp),
        )
        .await
        .unwrap();
    rtflow_testkit::wait_until(Duration::from_secs(2), || {
        mount.snapshot().stats.rtcp_publisher_sender_reports > 0
    })
    .await;

    drop(control);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn udp_publisher_rtcp_mux_receives_receiver_report() {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.rtcp_report_interval = Duration::from_millis(100);
    let server = RtspServer::bind(config).await.unwrap();
    server
        .register_mount(
            Arc::from("/mux"),
            MountConfig::Publisher {
                description: description(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let base = format!("rtsp://{}/mux", server.local_addr());
    let pair = UdpPair::bind_mux("127.0.0.1".parse().unwrap(), None, 4096)
        .await
        .unwrap();
    let port = pair.rtp_port().unwrap();
    let mut control = TcpStream::connect(server.local_addr()).await.unwrap();
    let announce = request(
        &mut control,
        &format!(
            "ANNOUNCE {base} RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
            announced_sdp().len(),
            announced_sdp()
        ),
    )
    .await;
    assert_eq!(announce.status, 200);
    let setup = request(
        &mut control,
        &format!(
            "SETUP {base}/camera-video RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP;unicast;client_port={port};rtcp-mux;mode=record\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 200);
    let transport = TransportSpec::parse(setup.headers.get("transport").unwrap()).unwrap();
    let TransportSpec::Udp {
        server_rtp: Some(server_rtp),
        ..
    } = transport
    else {
        panic!("no server RTP port");
    };
    let session = setup.headers.get("session").unwrap();
    let record = request(
        &mut control,
        &format!("RECORD {base} RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"),
    )
    .await;
    assert_eq!(record.status, 200);

    let packet = RtpPacket {
        header: RtpHeader {
            marker: true,
            payload_type: 96,
            sequence: 1,
            timestamp: 90_000,
            ssrc: 88,
        },
        payload: Bytes::from_static(&[0x65, 1, 2, 3]),
    };
    pair.rtp
        .send_to(
            &packet.encode(),
            std::net::SocketAddr::new(server.local_addr().ip(), server_rtp),
        )
        .await
        .unwrap();
    let mut buffer = [0u8; 2048];
    let (size, _) = tokio::time::timeout(Duration::from_secs(2), pair.rtp.recv_from(&mut buffer))
        .await
        .expect("mux RR timeout")
        .unwrap();
    let packets = RtcpPacket::parse(Bytes::copy_from_slice(&buffer[..size])).unwrap();
    assert!(packets.iter().any(|packet| matches!(
        packet,
        RtcpPacket::ReceiverReport { .. } | RtcpPacket::ReceiverReportWithReports { .. }
    )));
    drop(control);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn udp_publisher_accepts_nat_source_port() {
    use tokio::net::UdpSocket;
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/nat"),
            MountConfig::Publisher {
                description: description(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let base = format!("rtsp://{}/nat", server.local_addr());
    let pair = UdpPair::bind("127.0.0.1".parse().unwrap(), None)
        .await
        .unwrap();
    let client_rtp = pair.rtp_port().unwrap();
    let client_rtcp = pair.rtcp_port().unwrap();
    let mut control = TcpStream::connect(server.local_addr()).await.unwrap();
    let announce = request(
        &mut control,
        &format!(
            "ANNOUNCE {base} RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
            announced_sdp().len(),
            announced_sdp()
        ),
    )
    .await;
    assert_eq!(announce.status, 200);
    let setup = request(
        &mut control,
        &format!(
            "SETUP {base}/camera-video RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP;unicast;client_port={client_rtp}-{client_rtcp}\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 200);
    let transport = TransportSpec::parse(setup.headers.get("transport").unwrap()).unwrap();
    let TransportSpec::Udp {
        server_rtp: Some(server_rtp),
        ..
    } = transport
    else {
        panic!("no server RTP port");
    };
    let session = setup.headers.get("session").unwrap();
    let record = request(
        &mut control,
        &format!("RECORD {base} RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"),
    )
    .await;
    assert_eq!(record.status, 200);

    // A NATed publisher can send from a different source port than the one
    // negotiated; the first valid RTP locks that source instead of dropping.
    let nat = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let packet = RtpPacket {
        header: RtpHeader {
            marker: true,
            payload_type: 96,
            sequence: 1,
            timestamp: 90_000,
            ssrc: 66,
        },
        payload: Bytes::from_static(&[0x65, 1, 2, 3]),
    };
    nat.send_to(
        &packet.encode(),
        std::net::SocketAddr::new(server.local_addr().ip(), server_rtp),
    )
    .await
    .unwrap();
    rtflow_testkit::wait_until(Duration::from_secs(2), || {
        mount.snapshot().stats.accepted_frames > 0
    })
    .await;
    drop(control);
    server.shutdown().await.unwrap();
}
