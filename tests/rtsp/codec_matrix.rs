use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
};
use rtflow_testkit::{description, unit, wait_until, within};
use std::{sync::Arc, time::Duration};

async fn round_trip(codec: Codec, transport: UpstreamTransport) {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/codec"),
            MountConfig::Local {
                description: description("codec-matrix", &[codec]),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = url::Url::parse(&format!("rtsp://{}/codec", server.local_addr())).unwrap();
    let mut client = RtspClient::connect(
        url,
        ClientOptions::builder()
            .transport(transport)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    wait_until(Duration::from_secs(1), || {
        mount.snapshot().subscriber_count == 1
    })
    .await;

    let expected = unit(0, codec, 1);
    publisher.try_push(expected.clone()).unwrap();
    let received = within(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.codec, codec);
    assert_eq!(received.track_id, 0);
    assert_eq!(received.data, expected.data);
    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn all_phase_one_codecs_round_trip_over_udp_and_tcp() {
    for transport in [UpstreamTransport::Udp, UpstreamTransport::TcpInterleaved] {
        for codec in [
            Codec::H264,
            Codec::H265,
            Codec::Aac,
            Codec::Pcma,
            Codec::Pcmu,
        ] {
            round_trip(codec, transport).await;
        }
    }
}

#[tokio::test]
async fn pause_blocks_delivery_and_play_resumes() {
    for transport in [UpstreamTransport::Udp, UpstreamTransport::TcpInterleaved] {
        let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let mount = server
            .register_mount(
                Arc::from("/pause"),
                MountConfig::Local {
                    description: description("pause", &[Codec::H264]),
                },
            )
            .await
            .unwrap();
        let publisher = mount.publisher().unwrap();
        let mut client = RtspClient::connect(
            url::Url::parse(&format!("rtsp://{}/pause", server.local_addr())).unwrap(),
            ClientOptions::builder()
                .transport(transport)
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
        client.pause().await.unwrap();
        publisher.try_push(unit(0, Codec::H264, 1)).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(80), client.next())
                .await
                .is_err()
        );
        client.play().await.unwrap();
        wait_until(Duration::from_secs(1), || {
            mount.snapshot().subscriber_count == 1
        })
        .await;
        publisher.try_push(unit(0, Codec::H264, 2)).unwrap();
        let received = within(Duration::from_secs(1), client.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.data, unit(0, Codec::H264, 2).data);
        client.teardown().await.unwrap();
        server.shutdown().await.unwrap();
    }
}
