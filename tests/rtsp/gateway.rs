//! The multi-protocol gateway must share its mount registry with RTSP.
#![cfg(feature = "server")]

use rtflow::{
    Codec, MountConfig, ServerConfig,
    client::{ClientOptions, RtspClient},
    gateway::Gateway,
    transport::UpstreamTransport,
};
use rtflow_testkit::description;
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn gateway_mount_is_visible_to_rtsp() {
    let mut gateway = Gateway::new(16);
    gateway
        .start_rtsp(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    // Register through the gateway's shared registry, not the RTSP server.
    gateway
        .mounts()
        .register(
            Arc::from("/gateway-shared"),
            MountConfig::Local {
                description: description("gateway-shared", &[Codec::H264]),
            },
        )
        .unwrap();
    let server = gateway.rtsp().expect("RTSP server started");
    let url = format!("rtsp://{}/gateway-shared", server.local_addr());
    let session = RtspClient::connect_str(
        &url,
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .keep_alive_interval(None)
            .build()
            .unwrap(),
    )
    .await
    .expect("RTSP must see the gateway mount");
    assert_eq!(session.description.tracks.len(), 1);
    session.teardown().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), gateway.shutdown()).await;
}

#[cfg(feature = "http")]
#[tokio::test]
async fn dropping_gateway_stops_http_accept_loop() {
    use rtflow::http::HttpConfig;
    let mut gateway = Gateway::new(8);
    gateway
        .start_http(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let addr = gateway.http_addr().expect("HTTP address");
    drop(gateway);
    for _ in 0..40 {
        if tokio::net::TcpStream::connect(addr).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("HTTP accept loop kept the port after Gateway drop");
}
