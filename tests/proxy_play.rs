use bytes::Bytes;
use rtflow::client::{ClientOptions, RtspClient};
use rtflow::proxy::{UpstreamConfig, UpstreamTransport};
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig,
    TrackDescription,
};
use std::{sync::Arc, time::Duration};
fn description() -> MediaDescription {
    MediaDescription {
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
        }],
    }
}
#[tokio::test]
async fn proxy_forwards_upstream_frames() {
    let upstream = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
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
    let mut proxy_config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    proxy_config.no_subscriber_grace = Duration::from_millis(50);
    let proxy = RtspServer::bind(proxy_config).await.unwrap();
    let config = UpstreamConfig::new(
        url::Url::parse(&format!("rtsp://{}/source", upstream.local_addr())).unwrap(),
        UpstreamTransport::Udp,
    );
    let proxy_mount = proxy
        .register_mount(
            Arc::from("/proxy"),
            MountConfig::Proxy {
                upstream: Box::new(config),
            },
        )
        .await
        .unwrap();
    let mut client = RtspClient::connect(
        url::Url::parse(&format!("rtsp://{}/proxy", proxy.local_addr())).unwrap(),
        ClientOptions::default(),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    publisher
        .try_push(AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 1,
            is_keyframe: true,
            data: Bytes::from_static(&[0x65, 9, 8]),
        })
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.data, Bytes::from_static(&[0x65, 9, 8]));
    drop(client);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                proxy_mount.snapshot().proxy_state,
                Some(rtflow::proxy::ProxyState::Idle)
            ) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    proxy.shutdown().await.unwrap();
    upstream.shutdown().await.unwrap();
}
