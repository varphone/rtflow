#![cfg(feature = "multicast")]

//! Proxy relays a multicast RTSP upstream to a TCP downstream.
use rtflow::{
    MountConfig,
    client::{ClientOptions, RtspClient, UpstreamTransport},
    media::Codec,
    proxy::UpstreamConfig,
    rtsp::sdp::SdpConnection,
    server::{RtspServer, ServerConfig},
};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn proxy_relays_multicast_upstream() {
    let upstream = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = rtflow_testkit::description("mc", &[Codec::H264]);
    media.connection = Some(SdpConnection {
        addr_type: "IP4".into(),
        address: "239.255.0.11".parse().unwrap(),
        ttl: Some(1),
        port: Some(15110),
    });
    let source = upstream
        .register_mount(Arc::from("/mc"), MountConfig::Local { description: media })
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
                    url::Url::parse(&format!("rtsp://{}/mc", upstream.local_addr())).unwrap(),
                    UpstreamTransport::Udp,
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
