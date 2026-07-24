use bytes::BytesMut;
use rtsp::{
    Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig, TrackDescription,
    rtsp::{self as wire_rtsp, Message, Response},
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn request(stream: &mut TcpStream, wire: &str) -> Response {
    stream.write_all(wire.as_bytes()).await.unwrap();
    let mut input = BytesMut::new();
    let mut buffer = [0u8; 1024];
    loop {
        if let Some(Message::Response(response)) = wire_rtsp::parse(&mut input, 4096, 4096).unwrap()
        {
            return response;
        }
        let size = stream.read(&mut buffer).await.unwrap();
        assert!(size > 0);
        input.extend_from_slice(&buffer[..size]);
    }
}

#[tokio::test]
async fn rejects_invalid_session_and_state_transitions() {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    server
        .register_mount(
            Arc::from("/state"),
            MountConfig::Local {
                description: MediaDescription {
                    session_name: Arc::from("state"),
                    tracks: vec![TrackDescription {
                        id: 0,
                        kind: MediaKind::Video,
                        codec: Codec::H264,
                        clock_rate: 90_000,
                        channels: None,
                        payload_type: 96,
                        fmtp: None,
                        control: Arc::from("video"),
                    }],
                },
            },
        )
        .await
        .unwrap();
    let base = format!("rtsp://{}/state", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();

    let response = request(
        &mut stream,
        &format!("PLAY {base} RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
    )
    .await;
    assert_eq!(response.status, 454);

    let setup = request(
        &mut stream,
        &format!(
            "SETUP {base}/video RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 200);
    let session = setup.headers.get("session").unwrap();

    let response = request(
        &mut stream,
        &format!(
            "SETUP {base}/video RTSP/1.0\r\nCSeq: 3\r\nSession: wrong\r\nTransport: RTP/AVP/TCP;unicast;interleaved=2-3\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(response.status, 454);

    let response = request(
        &mut stream,
        &format!("PAUSE {base} RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\n\r\n"),
    )
    .await;
    assert_eq!(response.status, 455);

    tokio::time::sleep(Duration::from_millis(5)).await;
    server.shutdown().await.unwrap();
}
