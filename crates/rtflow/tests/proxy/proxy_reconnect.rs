//! Proxy reconnects to an upstream that restarts on the same port.
use rtflow::{
    MountConfig,
    client::{ClientOptions, RtspClient, UpstreamTransport},
    media::Codec,
    proxy::UpstreamConfig,
    server::{RtspServer, ServerConfig},
};
use std::{sync::Arc, time::Duration};

async fn start_upstream(port: u16) -> (RtspServer, rtflow::LocalPublisher) {
    let mut config = ServerConfig::new(format!("127.0.0.1:{port}").parse().unwrap());
    config.no_subscriber_grace = Duration::from_millis(100);
    let server = RtspServer::bind(config).await.unwrap();
    let mount = server
        .register_mount(
            Arc::from("/cam"),
            MountConfig::Local {
                description: rtflow_testkit::description("cam", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    (server, mount.publisher().unwrap())
}

#[tokio::test]
async fn proxy_reconnects_after_upstream_restart() {
    let first = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let port = first.local_addr().port();
    first.shutdown().await.unwrap();
    let (upstream, publisher) = start_upstream(port).await;
    let feeder = tokio::spawn(async move {
        let mut sequence = 0u32;
        loop {
            if publisher
                .try_push(rtflow_testkit::unit(0, Codec::H264, sequence))
                .is_err()
            {
                break;
            }
            sequence = sequence.wrapping_add(1);
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    });

    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let upstream_config = UpstreamConfig::new(
        url::Url::parse(&format!("rtsp://127.0.0.1:{port}/cam")).unwrap(),
        UpstreamTransport::TcpInterleaved,
    );
    proxy
        .register_mount(
            Arc::from("/proxy"),
            MountConfig::Proxy {
                upstream: Box::new(upstream_config),
            },
        )
        .await
        .unwrap();

    let proxy_addr = proxy.local_addr();
    let play = move || async move {
        RtspClient::connect(
            format!("rtsp://{proxy_addr}/proxy"),
            ClientOptions::builder()
                .transport(UpstreamTransport::TcpInterleaved)
                .build()
                .unwrap(),
        )
        .await
        .ok()
    };
    let mut downstream = play().await.expect("first downstream");
    let first_frame = tokio::time::timeout(Duration::from_secs(3), downstream.next())
        .await
        .expect("first frame timeout")
        .expect("first stream ended")
        .expect("first media error");
    assert_eq!(first_frame.track_id, 0);

    feeder.abort();
    upstream.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let (upstream, publisher) = start_upstream(port).await;
    let feeder = tokio::spawn(async move {
        let mut sequence = 0u32;
        loop {
            if publisher
                .try_push(rtflow_testkit::unit(0, Codec::H264, sequence))
                .is_err()
            {
                break;
            }
            sequence = sequence.wrapping_add(1);
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    });

    let mut recovered = false;
    for _ in 0..40 {
        let Some(mut client) = play().await else {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        };
        if let Ok(Some(Ok(frame))) =
            tokio::time::timeout(Duration::from_secs(2), client.next()).await
        {
            assert_eq!(frame.track_id, 0);
            recovered = true;
            let _ = client.teardown().await;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(recovered, "proxy did not recover after upstream restart");
    feeder.abort();
    let _ = upstream.shutdown().await;
    proxy.shutdown().await.unwrap();
}
