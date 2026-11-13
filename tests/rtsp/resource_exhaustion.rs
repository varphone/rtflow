use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
};
use rtflow_testkit::{RawRtsp, description};
use std::{ops::RangeInclusive, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

fn available_udp_pair() -> RangeInclusive<u16> {
    for port in (30_000u16..60_000).step_by(2) {
        let Ok(rtp) = std::net::UdpSocket::bind(("127.0.0.1", port)) else {
            continue;
        };
        let Ok(rtcp) = std::net::UdpSocket::bind(("127.0.0.1", port + 1)) else {
            continue;
        };
        drop((rtp, rtcp));
        return port..=port + 1;
    }
    panic!("no adjacent UDP test ports are available");
}

#[tokio::test]
async fn session_limit_rejects_excess_connections() {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.max_sessions = 1;
    let server = RtspServer::bind(config).await.unwrap();
    let address = server.local_addr();
    let mut first = RawRtsp::connect(address).await;
    assert_eq!(
        first
            .request("OPTIONS", &format!("rtsp://{address}/"), "")
            .await
            .status,
        200
    );

    let mut excess = TcpStream::connect(address).await.unwrap();
    excess
        .write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n")
        .await
        .unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(1), excess.read(&mut byte))
        .await
        .expect("excess session was not closed");
    match read {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("unexpected excess-session read result: {other:?}"),
    }
    drop(first);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn udp_port_range_exhaustion_fails_the_second_setup() {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.udp_port_range = Some(available_udp_pair());
    let server = RtspServer::bind(config).await.unwrap();
    server
        .register_mount(
            Arc::from("/ports"),
            MountConfig::Local {
                description: description("ports", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let url = url::Url::parse(&format!("rtsp://{}/ports", server.local_addr())).unwrap();
    let first = RtspClient::connect(
        url.clone(),
        ClientOptions::builder()
            .transport(UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(
        RtspClient::connect(
            url,
            ClientOptions::builder()
                .transport(UpstreamTransport::Udp)
                .build()
                .unwrap()
        )
        .await
        .is_err(),
        "a second UDP SETUP must fail when the configured pair is occupied"
    );
    first.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}
