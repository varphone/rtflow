use bytes::Bytes;
use rtflow::{
    AccessUnit, Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::{ReconnectPolicy, UpstreamConfig, UpstreamEndpoint, UpstreamTransport},
};
use rtflow_testkit::description;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn unavailable_primary_fails_over_to_compatible_fallback() {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable = reservation.local_addr().unwrap();
    drop(reservation);

    let fallback = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let fallback_mount = fallback
        .register_mount(
            Arc::from("/camera"),
            MountConfig::Local {
                description: description("fallback", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let publisher = fallback_mount.publisher().unwrap();

    let mut upstream = UpstreamConfig::new(
        url::Url::parse(&format!("rtsp://{unavailable}/camera")).unwrap(),
        UpstreamTransport::TcpInterleaved,
    );
    upstream.request_timeout = Duration::from_millis(50);
    upstream.reconnect = ReconnectPolicy {
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(20),
        multiplier: 1.0,
        jitter_ratio: 0.0,
    };
    upstream.add_fallback(UpstreamEndpoint::new(
        url::Url::parse(&format!("rtsp://{}/camera", fallback.local_addr())).unwrap(),
        UpstreamTransport::TcpInterleaved,
    ));

    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    proxy
        .register_mount(
            Arc::from("/failover"),
            MountConfig::Proxy {
                upstream: Box::new(upstream),
            },
        )
        .await
        .unwrap();

    let mut client = RtspClient::connect(
        url::Url::parse(&format!("rtsp://{}/failover", proxy.local_addr())).unwrap(),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while fallback_mount.snapshot().subscriber_count == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    publisher
        .try_push(AccessUnit {
            nal_offsets: None,
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 90_000,
            is_keyframe: true,
            data: Bytes::from_static(&[0x65, 0xfa, 0x11]),
        })
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        frame.data,
        Bytes::from_static(&[0, 0, 0, 1, 0x65, 0xfa, 0x11])
    );

    drop(client);
    proxy.shutdown().await.unwrap();
    fallback.shutdown().await.unwrap();
}
