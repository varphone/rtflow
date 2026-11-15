use rcgen::{CertifiedKey, generate_simple_self_signed};
use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig, TlsClientConfig, TlsServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::{UpstreamConfig, UpstreamTransport},
};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn interleaved_media_and_control_round_trip_over_rtsps() {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let server_tls = TlsServerConfig::from_pem(
        cert.pem().as_bytes(),
        signing_key.serialize_pem().as_bytes(),
    )
    .unwrap();
    let client_tls = TlsClientConfig::with_root_certificates(cert.pem().as_bytes()).unwrap();
    let config = ServerConfig::builder("127.0.0.1:0".parse().unwrap())
        .tls(server_tls)
        .build()
        .unwrap();
    let server = RtspServer::bind(config).await.unwrap();
    let mount = server
        .register_mount(
            Arc::from("/secure"),
            MountConfig::Local {
                description: rtflow_testkit::description("secure", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = url::Url::parse(&format!(
        "rtsps://localhost:{}/secure",
        server.local_addr().port()
    ))
    .unwrap();
    let options = ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .tls(client_tls)
        .build()
        .unwrap();
    let mut client = RtspClient::connect(url, options).await.unwrap();
    client.pause().await.unwrap();
    client.play().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    publisher
        .try_push(rtflow_testkit::unit(0, Codec::H264, 1))
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.codec, Codec::H264);
    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn proxy_pulls_from_rtsps_upstream_and_forwards_media() {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let server_tls = TlsServerConfig::from_pem(
        cert.pem().as_bytes(),
        signing_key.serialize_pem().as_bytes(),
    )
    .unwrap();
    let client_tls = TlsClientConfig::with_root_certificates(cert.pem().as_bytes()).unwrap();

    let upstream = RtspServer::bind(
        ServerConfig::builder("127.0.0.1:0".parse().unwrap())
            .tls(server_tls)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let upstream_mount = upstream
        .register_mount(
            Arc::from("/secure-upstream"),
            MountConfig::Local {
                description: rtflow_testkit::description("secure-upstream", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let publisher = upstream_mount.publisher().unwrap();
    let upstream_url = url::Url::parse(&format!(
        "rtsps://localhost:{}/secure-upstream",
        upstream.local_addr().port()
    ))
    .unwrap();
    let mut upstream_config = UpstreamConfig::new(upstream_url, UpstreamTransport::TcpInterleaved);
    upstream_config.tls = Some(client_tls);

    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
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
        url::Url::parse(&format!("rtsp://{}/proxy", proxy.local_addr())).unwrap(),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();

    rtflow_testkit::wait_until(Duration::from_secs(1), || {
        upstream_mount.snapshot().subscriber_count > 0
    })
    .await;
    publisher
        .try_push(rtflow_testkit::unit(0, Codec::H264, 2))
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), downstream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.codec, Codec::H264);

    downstream.teardown().await.unwrap();
    proxy.shutdown().await.unwrap();
    upstream.shutdown().await.unwrap();
}
