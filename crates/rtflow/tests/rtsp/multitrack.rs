use bytes::Bytes;
use rtflow::client::{ClientOptions, RtspClient};
use rtflow::transport::UpstreamTransport;
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig,
    TrackDescription,
};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
fn description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("av"),
        tracks: vec![
            TrackDescription {
                id: 0,
                kind: MediaKind::Video,
                codec: Codec::H264,
                clock_rate: 90_000,
                channels: None,
                payload_type: 96,
                fmtp: None,
                control: Arc::from("trackID=0"),
                crypto: None,
                width: None,
                height: None,
                frame_rate: None,
                direction: None,
                ptime: None,
                rtcp: None,
                bandwidth_as: None,
                rtx: None,
            },
            TrackDescription {
                id: 1,
                kind: MediaKind::Audio,
                codec: Codec::Pcma,
                clock_rate: 8_000,
                channels: Some(1),
                payload_type: 8,
                fmtp: None,
                control: Arc::from("trackID=1"),
                crypto: None,
                width: None,
                height: None,
                frame_rate: None,
                direction: None,
                ptime: None,
                rtcp: None,
                bandwidth_as: None,
                rtx: None,
            },
        ],
        connection: None,
    }
}
async fn run(transport: UpstreamTransport) {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/av"),
            MountConfig::Local {
                description: description(),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let options = ClientOptions::builder()
        .transport(transport)
        .build()
        .unwrap();
    let mut client = RtspClient::connect(
        url::Url::parse(&format!("rtsp://{}/av", server.local_addr())).unwrap(),
        options,
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    for (frame_id, codec, data, key) in [
        (0, Codec::H264, Bytes::from_static(&[0x65, 1]), true),
        (1, Codec::Pcma, Bytes::from_static(&[2, 3]), true),
    ] {
        publisher
            .try_push(AccessUnit {
                track_id: frame_id,
                codec,
                pts: Duration::ZERO,
                dts: None,
                rtp_timestamp: 1,
                is_keyframe: key,
                data,
            })
            .unwrap();
    }
    let mut tracks = BTreeSet::new();
    for _ in 0..2 {
        let frame = tokio::time::timeout(Duration::from_secs(1), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tracks.insert(frame.track_id);
    }
    assert_eq!(tracks, BTreeSet::from([0, 1]));
    server.shutdown().await.unwrap();
}
#[tokio::test]
async fn udp_audio_video() {
    run(UpstreamTransport::Udp).await
}
#[tokio::test]
async fn tcp_audio_video() {
    run(UpstreamTransport::TcpInterleaved).await
}
