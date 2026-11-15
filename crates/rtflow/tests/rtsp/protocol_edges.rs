//! RTSP protocol edge cases: Content-Base, Proxy-Require, session IDs and VP8/VP9.

use bytes::BytesMut;
use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig,
    rtsp::{self as wire_rtsp, Message, Response, sdp},
};
use rtflow_testkit::description;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn request(stream: &mut TcpStream, wire: &str) -> Response {
    stream.write_all(wire.as_bytes()).await.unwrap();
    let mut input = BytesMut::new();
    let mut buffer = [0u8; 4096];
    loop {
        if let Some(Message::Response(response)) =
            wire_rtsp::parse(&mut input, 16 * 1024, 64 * 1024).unwrap()
        {
            return response;
        }
        let size = stream.read(&mut buffer).await.unwrap();
        assert!(size > 0);
        input.extend_from_slice(&buffer[..size]);
    }
}

async fn server_with(codec: Codec) -> RtspServer {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    server
        .register_mount(
            Arc::from("/edge"),
            MountConfig::Local {
                description: description("edge", &[codec]),
            },
        )
        .await
        .unwrap();
    server
}

#[tokio::test]
async fn content_base_is_absolute_and_resolves_track_controls() {
    let server = server_with(Codec::H264).await;
    let base = format!("rtsp://{}/edge", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let response = request(
        &mut stream,
        &format!("DESCRIBE {base} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n"),
    )
    .await;
    assert_eq!(response.status, 200);
    let content_base = response
        .headers
        .get("content-base")
        .expect("content-base header");
    assert!(
        content_base.starts_with(&format!("rtsp://{}", server.local_addr())),
        "Content-Base must be absolute, got {content_base}"
    );
    assert!(content_base.ends_with("/edge/"), "got {content_base}");
    let description = sdp::parse(&response.body).unwrap();
    let setup = sdp::control_uri(content_base, &description.tracks[0].control).unwrap();
    assert!(setup.starts_with(&format!("rtsp://{}", server.local_addr())));
}

#[tokio::test]
async fn unsupported_proxy_require_is_rejected_with_551() {
    let server = server_with(Codec::H264).await;
    let base = format!("rtsp://{}/edge", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let response = request(
        &mut stream,
        &format!("OPTIONS {base} RTSP/1.0\r\nCSeq: 7\r\nProxy-Require: funky-option\r\n\r\n"),
    )
    .await;
    assert_eq!(response.status, 551);
    assert_eq!(
        response.headers.get("unsupported").map(String::as_str),
        Some("funky-option")
    );
}

async fn setup_session(server: &RtspServer) -> String {
    let base = format!("rtsp://{}/edge", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let describe = request(
        &mut stream,
        &format!("DESCRIBE {base} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n"),
    )
    .await;
    let description = sdp::parse(&describe.body).unwrap();
    let control = sdp::control_uri(&base, &description.tracks[0].control).unwrap();
    let setup = request(
        &mut stream,
        &format!(
            "SETUP {control} RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP;unicast;client_port=31000-31001\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 200, "SETUP failed: {:?}", setup.headers);
    setup
        .headers
        .get("session")
        .expect("session header")
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn wire_session_ids_are_random_hex_and_unique() {
    let server = server_with(Codec::H264).await;
    let first = setup_session(&server).await;
    let second = setup_session(&server).await;
    for id in [&first, &second] {
        assert_eq!(id.len(), 32, "expected 128-bit hex session id, got {id}");
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "session id must be lowercase hex: {id}"
        );
    }
    assert_ne!(first, second);
}

#[tokio::test]
async fn vp8_description_round_trips_through_rtsp() {
    let server = server_with(Codec::Vp8).await;
    let base = format!("rtsp://{}/edge", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let response = request(
        &mut stream,
        &format!("DESCRIBE {base} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n"),
    )
    .await;
    assert_eq!(response.status, 200);
    let description = sdp::parse(&response.body).unwrap();
    assert_eq!(description.tracks[0].codec, Codec::Vp8);
    tokio::time::sleep(Duration::from_millis(10)).await;
}

#[tokio::test]
async fn set_parameter_public_and_scale_semantics() {
    let server = server_with(Codec::H264).await;
    let base = format!("rtsp://{}/edge", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let options = request(
        &mut stream,
        &format!("OPTIONS {base} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    )
    .await;
    assert_eq!(options.status, 200);
    let public = options.headers.get("public").unwrap();
    assert!(public.contains("SET_PARAMETER"), "Public: {public}");

    let describe = request(
        &mut stream,
        &format!("DESCRIBE {base} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n"),
    )
    .await;
    let description = sdp::parse(&describe.body).unwrap();
    let control = sdp::control_uri(&base, &description.tracks[0].control).unwrap();
    let setup = request(
        &mut stream,
        &format!(
            "SETUP {control} RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP;unicast;client_port=32000-32001\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 200);
    let session = setup.headers.get("session").unwrap().clone();

    let set_parameter = request(
        &mut stream,
        &format!("SET_PARAMETER {base} RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\n\r\n"),
    )
    .await;
    assert_eq!(set_parameter.status, 200);

    let play = request(
        &mut stream,
        &format!(
            "PLAY {base} RTSP/1.0\r\nCSeq: 5\r\nSession: {session}\r\nRange: npt=0-\r\nScale: 2.0\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(play.status, 551);
}

#[cfg(feature = "multicast")]
#[tokio::test]
async fn multicast_setup_advertises_group_and_plays() {
    use rtflow::rtsp::sdp::SdpConnection;

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("multicast", &[Codec::H264]);
    media.connection = Some(SdpConnection {
        addr_type: "IP4".into(),
        address: "239.255.0.1".parse().unwrap(),
        ttl: Some(1),
        port: Some(5004),
    });
    server
        .register_mount(Arc::from("/mc"), MountConfig::Local { description: media })
        .await
        .unwrap();
    let base = format!("rtsp://{}/mc", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let describe = request(
        &mut stream,
        &format!("DESCRIBE {base} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n"),
    )
    .await;
    assert_eq!(describe.status, 200);
    let body = String::from_utf8(describe.body.clone()).unwrap();
    assert!(body.contains("c=IN IP4 239.255.0.1/1"), "{body}");
    assert!(body.contains("m=video 5004"), "{body}");

    let description = sdp::parse(&describe.body).unwrap();
    let control = sdp::control_uri(&base, &description.tracks[0].control).unwrap();
    let setup = request(
        &mut stream,
        &format!("SETUP {control} RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP;multicast\r\n\r\n"),
    )
    .await;
    assert_eq!(setup.status, 200, "{:?}", setup.headers);
    let transport = setup.headers.get("transport").unwrap();
    assert!(transport.contains("multicast"), "{transport}");
    assert!(transport.contains("destination=239.255.0.1"), "{transport}");
    assert!(transport.contains("port=5004"), "{transport}");
    let session = setup.headers.get("session").unwrap().clone();
    let play = request(
        &mut stream,
        &format!("PLAY {base} RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"),
    )
    .await;
    assert_eq!(play.status, 200, "{:?}", play.headers);
    server.shutdown().await.unwrap();
}

#[cfg(feature = "multicast")]
#[tokio::test]
async fn multicast_client_receives_group_media() {
    use rtflow::{
        client::{ClientOptions, RtspClient},
        rtsp::sdp::SdpConnection,
    };

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("multicast", &[Codec::H264]);
    media.connection = Some(SdpConnection {
        addr_type: "IP4".into(),
        address: "239.255.0.7".parse().unwrap(),
        ttl: Some(1),
        port: Some(15004),
    });
    let mount = server
        .register_mount(Arc::from("/mc"), MountConfig::Local { description: media })
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = format!("rtsp://{}/mc", server.local_addr());
    let mut client = RtspClient::connect(
        &url,
        ClientOptions::builder()
            .transport(rtflow::transport::UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .expect("multicast connect");
    publisher
        .try_push(rtflow_testkit::unit(0, Codec::H264, 1))
        .unwrap();
    let unit = tokio::time::timeout(Duration::from_secs(2), client.next())
        .await
        .expect("multicast frame timeout")
        .expect("multicast stream ended")
        .expect("multicast media error");
    assert_eq!(unit.codec, Codec::H264);
    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[cfg(feature = "srtp")]
#[tokio::test]
async fn srtp_playback_decrypts_media() {
    use base64::Engine as _;
    use rtflow::{
        client::{ClientOptions, RtspClient},
        media::crypto::SdpCrypto,
    };

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("secure", &[Codec::H264]);
    media.tracks[0].crypto = Some(SdpCrypto {
        tag: 1,
        crypto_suite: "AES_128_CM_HMAC_SHA1_80".into(),
        key_params: format!(
            "inline:{}",
            base64::engine::general_purpose::STANDARD.encode([7u8; 30])
        ),
        session_params: None,
    });
    let mount = server
        .register_mount(
            Arc::from("/secure"),
            MountConfig::Local { description: media },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = format!("rtsp://{}/secure", server.local_addr());
    let mut client = RtspClient::connect(&url, ClientOptions::default())
        .await
        .expect("srtp connect");
    let expected = rtflow_testkit::unit(0, Codec::H264, 1);
    publisher.try_push(expected.clone()).unwrap();
    let received = tokio::time::timeout(Duration::from_secs(2), client.next())
        .await
        .expect("srtp frame timeout")
        .expect("srtp stream ended")
        .expect("srtp media error");
    assert_eq!(received.data, expected.data);
    rtflow_testkit::wait_until(Duration::from_secs(2), || {
        mount.snapshot().stats.rtcp_receiver_reports > 0
    })
    .await;
    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[cfg(feature = "srtp")]
#[tokio::test]
async fn srtp_over_tcp_playback_decrypts_media() {
    use base64::Engine as _;
    use rtflow::{
        client::{ClientOptions, RtspClient},
        media::crypto::SdpCrypto,
        transport::UpstreamTransport,
    };

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("secure-tcp", &[Codec::H264]);
    media.tracks[0].crypto = Some(SdpCrypto {
        tag: 1,
        crypto_suite: "AES_128_CM_HMAC_SHA1_80".into(),
        key_params: format!(
            "inline:{}",
            base64::engine::general_purpose::STANDARD.encode([9u8; 30])
        ),
        session_params: None,
    });
    let mount = server
        .register_mount(
            Arc::from("/secure-tcp"),
            MountConfig::Local { description: media },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = format!("rtsp://{}/secure-tcp", server.local_addr());
    let mut client = RtspClient::connect(
        &url,
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .expect("srtp tcp connect");
    let expected = rtflow_testkit::unit(0, Codec::H264, 1);
    publisher.try_push(expected.clone()).unwrap();
    let received = tokio::time::timeout(Duration::from_secs(2), client.next())
        .await
        .expect("srtp tcp frame timeout")
        .expect("srtp tcp stream ended")
        .expect("srtp tcp media error");
    assert_eq!(received.data, expected.data);
    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[cfg(all(feature = "srtp", feature = "multicast"))]
#[tokio::test]
async fn srtp_multicast_client_decrypts_group_media() {
    use base64::Engine as _;
    use rtflow::{
        client::{ClientOptions, RtspClient},
        media::crypto::SdpCrypto,
        rtsp::sdp::SdpConnection,
    };

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("secure-multicast", &[Codec::H264]);
    media.connection = Some(SdpConnection {
        addr_type: "IP4".into(),
        address: "239.255.0.9".parse().unwrap(),
        ttl: Some(1),
        port: Some(15014),
    });
    media.tracks[0].crypto = Some(SdpCrypto {
        tag: 1,
        crypto_suite: "AES_128_CM_HMAC_SHA1_80".into(),
        key_params: format!(
            "inline:{}",
            base64::engine::general_purpose::STANDARD.encode([11u8; 30])
        ),
        session_params: None,
    });
    let mount = server
        .register_mount(
            Arc::from("/secure-mc"),
            MountConfig::Local { description: media },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = format!("rtsp://{}/secure-mc", server.local_addr());
    let mut client = RtspClient::connect(
        &url,
        ClientOptions::builder()
            .transport(rtflow::transport::UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .expect("srtp multicast connect");
    let expected = rtflow_testkit::unit(0, Codec::H264, 1);
    publisher.try_push(expected.clone()).unwrap();
    let received = tokio::time::timeout(Duration::from_secs(2), client.next())
        .await
        .expect("srtp multicast timeout")
        .expect("srtp multicast ended")
        .expect("srtp multicast media error");
    assert_eq!(received.data, expected.data);
    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[cfg(feature = "multicast")]
#[tokio::test]
async fn multicast_client_sends_rtcp_reports() {
    use rtflow::{
        client::{ClientOptions, RtspClient},
        rtcp::Packet as RtcpPacket,
        rtsp::{multicast::MulticastConfig, sdp::SdpConnection},
    };

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("multicast-rtcp", &[Codec::H264]);
    media.connection = Some(SdpConnection {
        addr_type: "IP4".into(),
        address: "239.255.0.11".parse().unwrap(),
        ttl: Some(1),
        port: Some(15024),
    });
    server
        .register_mount(
            Arc::from("/mc-rtcp"),
            MountConfig::Local { description: media },
        )
        .await
        .unwrap();
    let monitor = MulticastConfig::new("239.255.0.11".parse().unwrap(), 15025)
        .bind()
        .await
        .expect("join RTCP group");
    let url = format!("rtsp://{}/mc-rtcp", server.local_addr());
    let client = RtspClient::connect(
        &url,
        ClientOptions::builder()
            .transport(rtflow::transport::UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .expect("multicast connect");
    let mut buffer = [0u8; 2048];
    let (size, _) = tokio::time::timeout(Duration::from_secs(2), monitor.recv_from(&mut buffer))
        .await
        .expect("multicast RTCP timeout")
        .expect("multicast RTCP recv");
    let packets = RtcpPacket::parse(bytes::Bytes::copy_from_slice(&buffer[..size]))
        .expect("multicast RR parse");
    assert!(packets.iter().any(|packet| matches!(
        packet,
        RtcpPacket::ReceiverReport { .. } | RtcpPacket::ReceiverReportWithReports { .. }
    )));
    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn setup_echoes_destination_source_and_mode() {
    let server = server_with(Codec::H264).await;
    let base = format!("rtsp://{}/edge", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let describe = request(
        &mut stream,
        &format!("DESCRIBE {base} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\n\r\n"),
    )
    .await;
    let description = sdp::parse(&describe.body).unwrap();
    let control = sdp::control_uri(&base, &description.tracks[0].control).unwrap();
    let setup = request(
        &mut stream,
        &format!(
            "SETUP {control} RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP;unicast;client_port=33000-33001;destination=203.0.113.5;source=192.0.2.10;mode=play\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 200, "{:?}", setup.headers);
    let transport = setup.headers.get("transport").unwrap();
    assert!(transport.contains("destination=203.0.113.5"), "{transport}");
    assert!(transport.contains("source=192.0.2.10"), "{transport}");
    assert!(transport.contains("mode=play"), "{transport}");
    server.shutdown().await.unwrap();
}
