//! Dynamic RTSP publisher mounts (ServerConfig::dynamic_publishers).
use rtflow::{
    Codec, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    transport::UpstreamTransport,
};
use rtflow_testkit::{description, wait_until};
use std::time::Duration;

#[tokio::test]
async fn announce_registers_dynamic_publisher_and_cleans_it_up() {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.dynamic_publishers = true;
    let server = RtspServer::bind(config).await.unwrap();
    let media = description("dynamic", &[Codec::H264]);
    let url = format!("rtsp://{}/live/cam", server.local_addr());
    let mut publisher = RtspClient::publish(
        &url,
        media,
        ClientOptions::builder()
            .transport(UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .expect("dynamic publish");
    let registry = server.registry();
    wait_until(Duration::from_secs(2), || {
        registry
            .get_handle("/live/cam")
            .is_some_and(|mount| mount.snapshot().publisher_connected)
    })
    .await;
    let mount = registry.get_handle("/live/cam").unwrap();
    let mut subscriber = mount.subscribe();
    publisher
        .push(rtflow_testkit::unit(0, Codec::H264, 0))
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("subscriber timeout")
        .expect("subscriber closed");
    assert_eq!(frame.codec, Codec::H264);
    drop(subscriber);
    publisher.teardown().await.unwrap();
    wait_until(Duration::from_secs(2), || {
        registry.get_handle("/live/cam").is_none()
    })
    .await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn dynamic_publishers_are_opt_in() {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let media = description("dynamic-off", &[Codec::H264]);
    let url = format!("rtsp://{}/live/off", server.local_addr());
    let result = RtspClient::publish(
        &url,
        media,
        ClientOptions::builder()
            .transport(UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await;
    assert!(result.is_err(), "unknown path must be rejected");
    assert!(server.registry().get_handle("/live/off").is_none());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn setup_mode_must_match_mount_role() {
    use bytes::BytesMut;
    use rtflow::rtsp::{self as wire_rtsp, Message, Response};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };
    async fn request(stream: &mut TcpStream, wire: &str) -> Response {
        stream.write_all(wire.as_bytes()).await.unwrap();
        let mut input = BytesMut::new();
        let mut buffer = [0u8; 2048];
        loop {
            if let Some(Message::Response(response)) =
                wire_rtsp::parse(&mut input, 16 * 1024, 256 * 1024).unwrap()
            {
                return response;
            }
            let size = stream.read(&mut buffer).await.unwrap();
            assert!(size > 0);
            input.extend_from_slice(&buffer[..size]);
        }
    }

    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.dynamic_publishers = true;
    let server = RtspServer::bind(config).await.unwrap();
    let base = format!("rtsp://{}/live/mode", server.local_addr());
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let announce = request(
        &mut stream,
        &format!(
            "ANNOUNCE {base} RTSP/1.0\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=mode\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=0\r\n".len(),
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=mode\r\nt=0 0\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\na=control:trackID=0\r\n"
        ),
    )
    .await;
    assert_eq!(announce.status, 200);
    let setup = request(
        &mut stream,
        &format!(
            "SETUP {base}/trackID=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP;unicast;client_port=34000-34001;mode=play\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(setup.status, 461, "publisher must reject mode=play");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn udp_publish_activity_keeps_idle_control_session_alive() {
    let mut config = ServerConfig::new("127.0.0.1:0".parse().unwrap());
    config.dynamic_publishers = true;
    config.session_timeout = Duration::from_millis(200);
    config.rtcp_report_interval = Duration::from_millis(50);
    let server = RtspServer::bind(config).await.unwrap();
    let media = description("keepalive", &[Codec::H264]);
    let url = format!("rtsp://{}/live/keepalive", server.local_addr());
    let mut publisher = RtspClient::publish(
        &url,
        media,
        ClientOptions::builder()
            .transport(UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .expect("publish");
    for sequence in 0..20u32 {
        publisher
            .push(rtflow_testkit::unit(0, Codec::H264, sequence))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mount = server
        .registry()
        .get_handle("/live/keepalive")
        .expect("dynamic mount");
    assert!(
        mount.snapshot().publisher_connected,
        "RTP activity must keep the control session alive"
    );
    publisher.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}
