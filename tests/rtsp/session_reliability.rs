use bytes::Bytes;
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig,
    TrackDescription,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
};
use std::{sync::Arc, time::Duration};

fn description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("reliable"),
        tracks: vec![TrackDescription {
            id: 0,
            kind: MediaKind::Video,
            codec: Codec::H264,
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: None,
            control: Arc::from("video-main"),
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

async fn run(transport: UpstreamTransport) {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    // Aggressive but realistic: a sub-second session timeout with a
    // keep-alive that must keep the session alive. (Values below ~200 ms
    // race with test-runner scheduling and produce spurious CSeq-mismatch
    // flakes on loaded CI machines.)
    config.session_timeout = Duration::from_millis(250);
    let server = RtspServer::bind(config).await.unwrap();
    let mount = server
        .register_mount(
            Arc::from("/live/camera"),
            MountConfig::Local {
                description: description(),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let mut client = RtspClient::connect(
        url::Url::parse(&format!("rtsp://{}/live/camera", server.local_addr())).unwrap(),
        ClientOptions::builder()
            .transport(transport)
            .keep_alive_interval(Some(Duration::from_millis(50)))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();

    // No frames pushed yet: the session must be kept alive by the
    // keep-alive loop alone (window < session_timeout so a dead session
    // would surface as a disconnect error instead of a timeout).
    assert!(
        tokio::time::timeout(Duration::from_millis(200), client.next())
            .await
            .is_err()
    );
    publisher
        .try_push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 90_000,
            is_keyframe: true,
            data: Bytes::from_static(&[0x65, 7, 8]),
        })
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.data, Bytes::from_static(&[0, 0, 0, 1, 0x65, 7, 8]));

    drop(client);
    tokio::time::timeout(Duration::from_secs(1), async {
        while mount.snapshot().subscriber_count != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn udp_keepalive_and_drop_cleanup() {
    run(UpstreamTransport::Udp).await;
}

#[tokio::test]
async fn tcp_keepalive_and_drop_cleanup() {
    run(UpstreamTransport::TcpInterleaved).await;
}
