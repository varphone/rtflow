//! Proxy follows an upstream REDIRECT when cross-origin is allowed.
use rtflow::{
    MountConfig,
    client::{ClientOptions, RtspClient, UpstreamTransport},
    media::Codec,
    rtsp::Method,
    server::{RtspServer, ServerConfig},
};
use rtflow_testkit::{RawRtspServer, ServerReply, reply};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn proxy_follows_cross_origin_redirect() {
    let upstream = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
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
    let real = format!("rtsp://{}/cam", upstream.local_addr());
    let redirect = RawRtspServer::start(Box::new(move |request| {
        if request.method == Method::Options || request.method == Method::Describe {
            let mut response = reply(request, 302, "Found");
            response.headers.insert("location".into(), real.clone());
            return ServerReply::Response(response);
        }
        ServerReply::Response(reply(request, 404, "Not Found"))
    }))
    .await;

    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut upstream_config = rtflow::proxy::UpstreamConfig::new(
        url::Url::parse(&format!("rtsp://{}/start", redirect.address())).unwrap(),
        UpstreamTransport::TcpInterleaved,
    );
    upstream_config.follow_redirects = true;
    upstream_config.allow_cross_origin_redirect = true;
    proxy
        .register_mount(
            Arc::from("/proxy"),
            MountConfig::Proxy {
                upstream: Box::new(upstream_config),
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
    rtflow_testkit::wait_until(Duration::from_secs(2), || {
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
