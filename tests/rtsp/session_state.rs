use bytes::{Bytes, BytesMut};
use rtflow::{
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
    let mount = server
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
                        crypto: None,
                        width: None,
                        height: None,
                        frame_rate: None,
                        direction: None,
                        ptime: None,
                        rtcp: None,
                        bandwidth_as: None,
                    }],
                    connection: None,
                },
            },
        )
        .await
        .unwrap();
    let _warm_receiver = mount.subscribe();
    mount
        .publisher()
        .unwrap()
        .try_push(rtflow::AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 90_000,
            is_keyframe: true,
            data: Bytes::from_static(&[0x65, 0x88, 0x84, 0x21]),
        })
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

    let response = request(
        &mut stream,
        &format!("PLAY {base} RTSP/1.0\r\nCSeq: 5\r\nSession: {session}\r\n\r\n"),
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.headers.get("range").map(String::as_str),
        Some("npt=now-")
    );
    assert!(
        response
            .headers
            .get("rtp-info")
            .is_some_and(|value| value.contains("seq=0;rtptime=90000"))
    );

    tokio::time::sleep(Duration::from_millis(5)).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn reports_protocol_errors_without_closing_the_session() {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let base = format!("rtsp://{}/missing", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();

    let response = request(&mut stream, &format!("OPTIONS {base} RTSP/1.0\r\n\r\n")).await;
    assert_eq!(response.status, 400);
    assert!(!response.headers.contains_key("cseq"));

    let response = request(
        &mut stream,
        &format!("FROBNICATE {base} RTSP/1.0\r\nCSeq: 2\r\n\r\n"),
    )
    .await;
    assert_eq!(response.status, 501);

    let response = request(
        &mut stream,
        &format!("OPTIONS {base} RTSP/1.0\r\nCSeq: 3\r\nRequire: fancy-extension\r\n\r\n"),
    )
    .await;
    assert_eq!(response.status, 551);
    assert_eq!(
        response.headers.get("unsupported").map(String::as_str),
        Some("fancy-extension")
    );

    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn absolute_timeline_mode_reports_uptime_range() {
    use rtflow::{
        MountOptions, TimelineMode,
        rtsp::range::{NptPosition, NptRange},
    };
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let description = rtflow_testkit::description("absolute", &[Codec::H264]);
    let mount = server
        .register_mount_with(
            Arc::from("/absolute"),
            MountConfig::Local { description },
            {
                let mut options = MountOptions::default();
                options.timeline = Some(TimelineMode::Absolute {
                    offset: Duration::from_secs(1_000_000),
                });
                options
            },
        )
        .await
        .unwrap();
    let _warm = mount.subscribe();
    mount
        .publisher()
        .unwrap()
        .try_push(rtflow_testkit::unit(0, Codec::H264, 0))
        .unwrap();
    let base = format!("rtsp://{}/absolute", server.local_addr());

    let mut first = None;
    for _sequence in 0..2 {
        let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
        let setup = request(
            &mut stream,
            &format!(
                "SETUP {base}/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(setup.status, 200);
        let session = setup.headers.get("session").unwrap().clone();
        let response = request(
            &mut stream,
            &format!(
                "PLAY {base} RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\nRange: npt=0-\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response.status, 200);
        let range = NptRange::parse(response.headers.get("range").unwrap()).unwrap();
        let NptPosition::Time(start) = range.start else {
            panic!("absolute mode returned {range:?}");
        };
        assert!(start >= Duration::from_secs(1_000_000));
        if let Some(previous) = first {
            assert!(start >= previous, "timeline reset across connections");
        }
        first = Some(start);
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(stream);
    }
    server.shutdown().await.unwrap();
}
