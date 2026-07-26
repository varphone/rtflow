use bytes::Bytes;
use rtflow::client::{ClientOptions, RtspClient};
use rtflow::proxy::UpstreamTransport;
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig,
    TrackDescription,
    protocol::range::{NptPosition, NptRange},
};
use std::{sync::Arc, time::Duration};
#[tokio::test]
async fn local_h264_frame_reaches_interleaved_client() {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let description = MediaDescription {
        session_name: Arc::from("test"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
            crypto: None,
        }],
        connection: None,
    };
    let mount = server
        .register_mount(Arc::from("/stream"), MountConfig::Local { description })
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = url::Url::parse(&format!("rtsp://{}/stream", server.local_addr())).unwrap();
    let options = ClientOptions {
        transport: UpstreamTransport::TcpInterleaved,
        ..Default::default()
    };
    let mut client = RtspClient::connect(url, options).await.unwrap();
    client.pause().await.unwrap();
    assert!(!client.is_playing());
    client.play().await.unwrap();
    assert!(client.is_playing());
    tokio::time::sleep(Duration::from_millis(20)).await;
    publisher
        .try_push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 1,
            is_keyframe: true,
            data: Bytes::from_static(&[0x65, 1, 2]),
        })
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.data, Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2]));
    let quality = client.quality_snapshot().await;
    let track_quality = quality.tracks.get(&0).expect("track quality is available");
    assert_eq!(track_quality.packets_received, 1);
    assert_eq!(track_quality.packets_lost, 0);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let stats = mount.snapshot().stats;
            if stats.rtcp_sender_reports > 0 && stats.rtcp_receiver_reports > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_stream_accepts_gstreamer_style_zero_npt_range() {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let description = MediaDescription {
        session_name: Arc::from("live-range"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("trackID=0"),
            crypto: None,
        }],
        connection: None,
    };
    let mount = server
        .register_mount(Arc::from("/live"), MountConfig::Local { description })
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let mut client = RtspClient::connect(
        url::Url::parse(&format!("rtsp://{}/live", server.local_addr())).unwrap(),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .play_range(NptRange {
                start: NptPosition::Time(Duration::ZERO),
                end: None,
            })
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    publisher
        .try_push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 90_000,
            is_keyframe: true,
            data: Bytes::from_static(&[0x65, 0x01]),
        })
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.data, Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x01]));

    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}
