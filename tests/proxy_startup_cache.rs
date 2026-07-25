mod support;

use bytes::Bytes;
use rtsp::{
    AccessUnit, Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::{UpstreamConfig, UpstreamTransport},
};
use std::{sync::Arc, time::Duration};
use support::description;

#[tokio::test]
async fn late_proxy_subscriber_receives_cached_keyframe_and_gop() {
    let upstream = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let upstream_mount = upstream
        .register_mount(
            Arc::from("/camera"),
            MountConfig::Local {
                description: description("camera", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let publisher = upstream_mount.publisher().unwrap();
    let upstream_url =
        url::Url::parse(&format!("rtsp://{}/camera", upstream.local_addr())).unwrap();

    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let proxy_mount = proxy
        .register_mount(
            Arc::from("/proxy"),
            MountConfig::Proxy {
                upstream: Box::new(UpstreamConfig::new(
                    upstream_url,
                    UpstreamTransport::TcpInterleaved,
                )),
            },
        )
        .await
        .unwrap();
    let proxy_url = url::Url::parse(&format!("rtsp://{}/proxy", proxy.local_addr())).unwrap();
    let options = ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .build()
        .unwrap();
    let mut first = RtspClient::connect(proxy_url.clone(), options.clone())
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(1), async {
        while upstream_mount.snapshot().subscriber_count == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    for (timestamp, keyframe, payload) in [
        (90_000, true, Bytes::from_static(&[0x65, 1])),
        (93_000, false, Bytes::from_static(&[0x41, 2])),
    ] {
        publisher
            .try_push(AccessUnit {
                track_id: 0,
                codec: Codec::H264,
                pts: Duration::from_millis(u64::from(timestamp - 90_000) / 90),
                dts: None,
                rtp_timestamp: timestamp,
                is_keyframe: keyframe,
                data: payload,
            })
            .unwrap();
    }
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(1), first.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert_eq!(proxy_mount.snapshot().startup_cache_frames, 2);

    let mut late = RtspClient::connect(proxy_url, options).await.unwrap();
    let cached_keyframe = tokio::time::timeout(Duration::from_secs(1), late.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let cached_delta = tokio::time::timeout(Duration::from_secs(1), late.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(cached_keyframe.is_keyframe);
    assert_eq!(cached_keyframe.data, Bytes::from_static(&[0x65, 1]));
    assert_eq!(cached_delta.data, Bytes::from_static(&[0x41, 2]));

    drop((first, late));
    proxy.shutdown().await.unwrap();
    upstream.shutdown().await.unwrap();
}
