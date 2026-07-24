use bytes::{Bytes, BytesMut};
use rtsp::{
    Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig, TrackDescription,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
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
        }],
    }
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
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
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
        ClientOptions {
            transport: UpstreamTransport::TcpInterleaved,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let pair = UdpPair::bind("127.0.0.1".parse().unwrap(), None)
        .await
        .unwrap();
    let client_rtp = pair.rtp_port().unwrap();
    let client_rtcp = pair.rtcp_port().unwrap();
    let mut control = TcpStream::connect(server.local_addr()).await.unwrap();
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
        ..
    } = transport
    else {
        panic!("server did not return UDP RTP port");
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
    assert_eq!(received.data, Bytes::from_static(&[0x65, 3, 2, 1]));
    assert!(mount.snapshot().publisher_connected);

    drop(control);
    server.shutdown().await.unwrap();
}
