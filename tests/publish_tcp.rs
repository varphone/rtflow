use bytes::{Bytes, BytesMut};
use rtsp::{
    Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig, TrackDescription,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
    rtp::{RtpHeader, RtpPacket},
    rtsp::{self as wire_rtsp, Message, Response},
    transport::InterleavedFrame,
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

fn description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("published"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
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
async fn interleaved_record_is_forwarded_to_a_player() {
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

    let mut publisher = TcpStream::connect(server.local_addr()).await.unwrap();
    let announce = request(
        &mut publisher,
        &format!("ANNOUNCE {base} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    )
    .await;
    assert_eq!(announce.status, 200);
    let setup = request(
        &mut publisher,
        &format!(
            "SETUP {base}/trackID=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 200);
    let session = setup.headers.get("session").unwrap();
    let record = request(
        &mut publisher,
        &format!("RECORD {base} RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"),
    )
    .await;
    assert_eq!(record.status, 200);
    assert!(mount.snapshot().publisher_connected);

    let packet = RtpPacket {
        header: RtpHeader {
            marker: true,
            payload_type: 96,
            sequence: 1,
            timestamp: 90_000,
            ssrc: 7,
        },
        payload: Bytes::from_static(&[0x65, 4, 5, 6]),
    };
    publisher
        .write_all(
            &InterleavedFrame {
                channel: 0,
                payload: packet.encode(),
            }
            .encode(),
        )
        .await
        .unwrap();
    let received = tokio::time::timeout(Duration::from_secs(1), player.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(received.data, Bytes::from_static(&[0x65, 4, 5, 6]));

    drop(publisher);
    tokio::time::timeout(Duration::from_secs(1), async {
        while mount.snapshot().publisher_connected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.shutdown().await.unwrap();
}

async fn open_record_session(address: std::net::SocketAddr, base: &str) -> (TcpStream, Response) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let setup = request(
        &mut stream,
        &format!(
            "SETUP {base}/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 200);
    let session = setup.headers.get("session").unwrap();
    let record = request(
        &mut stream,
        &format!("RECORD {base} RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\n\r\n"),
    )
    .await;
    (stream, record)
}

#[tokio::test]
async fn concurrent_publisher_is_rejected_and_disconnect_allows_replacement() {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/replaceable"),
            MountConfig::Publisher {
                description: description(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let base = format!("rtsp://{}/replaceable", server.local_addr());
    let (first, response) = open_record_session(server.local_addr(), &base).await;
    assert_eq!(response.status, 200);
    let (second, response) = open_record_session(server.local_addr(), &base).await;
    assert_eq!(response.status, 409);
    drop(second);
    drop(first);
    tokio::time::timeout(Duration::from_secs(1), async {
        while mount.snapshot().publisher_connected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (replacement, response) = open_record_session(server.local_addr(), &base).await;
    assert_eq!(response.status, 200);
    drop(replacement);
    server.shutdown().await.unwrap();
}
