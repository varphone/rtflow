//! Proxy upstream uses Auto transport and falls back after a 461 SETUP.
use rtflow::{
    MountConfig,
    client::{ClientOptions, RtspClient, UpstreamTransport},
    media::Codec,
    proxy::UpstreamConfig,
    server::{RtspServer, ServerConfig},
};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn proxy_falls_back_from_udp_to_tcp_on_461() {
    let mut upstream_config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    // Port 1 is privileged/unavailable in the test environment, so UDP SETUP
    // cannot bind and the server answers 461.
    upstream_config.udp_port_range = Some(1..=2);
    let upstream = RtspServer::bind(upstream_config).await.unwrap();
    let source = upstream
        .register_mount(
            Arc::from("/cam"),
            MountConfig::Local {
                description: rtflow_testkit::description("cam", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let publisher = source.publisher().unwrap();
    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    proxy
        .register_mount(
            Arc::from("/proxy"),
            MountConfig::Proxy {
                upstream: Box::new(UpstreamConfig::new(
                    url::Url::parse(&format!("rtsp://{}/cam", upstream.local_addr())).unwrap(),
                    UpstreamTransport::Auto,
                )),
            },
        )
        .await
        .unwrap();
    let mut downstream = RtspClient::connect(
        format!("rtsp://{}/proxy", proxy.local_addr()),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .expect("proxy connect");
    rtflow_testkit::wait_until(Duration::from_secs(3), || {
        source.snapshot().subscriber_count > 0
    })
    .await;
    let expected = rtflow_testkit::unit(0, Codec::H264, 1);
    publisher.try_push(expected.clone()).unwrap();
    let received = tokio::time::timeout(Duration::from_secs(3), downstream.next())
        .await
        .expect("frame timeout")
        .expect("stream ended")
        .expect("media error");
    assert_eq!(received.data, expected.data);
    downstream.teardown().await.unwrap();
    proxy.shutdown().await.unwrap();
    upstream.shutdown().await.unwrap();
}
