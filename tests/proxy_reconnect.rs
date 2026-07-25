use bytes::Bytes;
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig,
    TrackDescription,
    client::{ClientOptions, RtspClient},
    proxy::{ProxyState, ReconnectPolicy, UpstreamConfig, UpstreamTransport},
};
use std::{sync::Arc, time::Duration};

fn description() -> MediaDescription {
    MediaDescription {
        session_name: Arc::from("reconnect"),
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
    }
}

#[tokio::test]
async fn proxy_recovers_when_upstream_appears_after_backoff() {
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_addr = reservation.local_addr().unwrap();
    drop(reservation);

    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut upstream_config = UpstreamConfig::new(
        url::Url::parse(&format!("rtsp://{upstream_addr}/source")).unwrap(),
        UpstreamTransport::TcpInterleaved,
    );
    upstream_config.cached_description = Some(description());
    upstream_config.request_timeout = Duration::from_millis(50);
    upstream_config.reconnect = ReconnectPolicy {
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(40),
        multiplier: 2.0,
        jitter_ratio: 0.0,
    };
    let proxy_mount = proxy
        .register_mount(
            Arc::from("/proxy"),
            MountConfig::Proxy {
                upstream: Box::new(upstream_config),
            },
        )
        .await
        .unwrap();
    let mut client = RtspClient::connect(
        url::Url::parse(&format!("rtsp://{}/proxy", proxy.local_addr())).unwrap(),
        ClientOptions {
            transport: UpstreamTransport::TcpInterleaved,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                proxy_mount.snapshot().proxy_state,
                Some(ProxyState::BackingOff { .. })
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    let upstream = RtspServer::bind(ServerConfig::new(upstream_addr))
        .await
        .unwrap();
    let source = upstream
        .register_mount(
            Arc::from("/source"),
            MountConfig::Local {
                description: description(),
            },
        )
        .await
        .unwrap();
    let publisher = source.publisher().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while source.snapshot().subscriber_count == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    publisher
        .try_push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 1,
            is_keyframe: true,
            data: Bytes::from_static(&[0x65, 9, 9]),
        })
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.data, Bytes::from_static(&[0x65, 9, 9]));

    proxy.shutdown().await.unwrap();
    upstream.shutdown().await.unwrap();
}
