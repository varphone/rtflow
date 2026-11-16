#![cfg(feature = "srtp")]

//! SRTP upstream: proxy decrypts the camera stream and serves downstream SRTP.
use base64::Engine as _;
use rtflow::{
    MountConfig,
    client::{ClientOptions, RtspClient, UpstreamTransport},
    media::{Codec, crypto::SdpCrypto},
    proxy::UpstreamConfig,
    server::{RtspServer, ServerConfig},
};
use std::{sync::Arc, time::Duration};

fn description(name: &str) -> rtflow::media::MediaDescription {
    let mut description = rtflow_testkit::description(name, &[Codec::H264]);
    description.tracks[0].crypto = Some(SdpCrypto {
        tag: 1,
        crypto_suite: "AES_128_CM_HMAC_SHA1_80".into(),
        key_params: format!(
            "inline:{}",
            base64::engine::general_purpose::STANDARD.encode([7u8; 30])
        ),
        session_params: None,
    });
    description
}

#[tokio::test]
async fn proxy_forwards_srtp_upstream_to_srtp_downstream() {
    let upstream = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = upstream
        .register_mount(
            Arc::from("/secure"),
            MountConfig::Local {
                description: description("secure"),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let upstream_url =
        url::Url::parse(&format!("rtsp://{}/secure", upstream.local_addr())).unwrap();

    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    proxy
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
    let mut downstream = RtspClient::connect(
        format!("rtsp://{}/proxy", proxy.local_addr()),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .expect("srtp proxy connect");

    rtflow_testkit::wait_until(Duration::from_secs(2), || {
        mount.snapshot().subscriber_count > 0
    })
    .await;
    let expected = rtflow_testkit::unit(0, Codec::H264, 1);
    publisher.try_push(expected.clone()).unwrap();
    let received = tokio::time::timeout(Duration::from_secs(3), downstream.next())
        .await
        .expect("srtp proxy frame timeout")
        .expect("srtp proxy stream ended")
        .expect("srtp proxy media error");
    assert_eq!(received.data, expected.data);

    downstream.teardown().await.unwrap();
    proxy.shutdown().await.unwrap();
    upstream.shutdown().await.unwrap();
}
