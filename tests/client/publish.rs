//! RTSP publishing client: ANNOUNCE + SETUP + RECORD + RTP push.
use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    transport::UpstreamTransport,
};
use rtflow_testkit::{description, wait_until};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn publishes_h264_into_publisher_mount() {
    let media = description("publish", &[Codec::H264]);
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/ingest"),
            MountConfig::Publisher {
                description: media.clone(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let mut subscriber = mount.subscribe();
    let url = format!("rtsp://{}/ingest", server.local_addr());
    let mut publisher = RtspClient::publish_str(
        &url,
        media,
        ClientOptions::builder()
            .transport(UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .expect("publish connect");
    wait_until(Duration::from_secs(2), || {
        mount.snapshot().publisher_connected
    })
    .await;
    for sequence in 0..5u32 {
        publisher
            .push(rtflow_testkit::unit(0, Codec::H264, sequence))
            .await
            .unwrap();
    }
    wait_until(Duration::from_secs(3), || {
        mount.snapshot().stats.accepted_frames > 0
    })
    .await;
    let frame = tokio::time::timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .expect("subscriber timeout")
        .expect("subscriber closed");
    assert_eq!(frame.codec, Codec::H264);
    assert!(!frame.data.is_empty());
    publisher.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn publishes_h264_over_interleaved_transport() {
    let media = description("publish-tcp", &[Codec::H264]);
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/ingest-tcp"),
            MountConfig::Publisher {
                description: media.clone(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let mut subscriber = mount.subscribe();
    let url = format!("rtsp://{}/ingest-tcp", server.local_addr());
    let mut publisher = RtspClient::publish_str(
        &url,
        media,
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .expect("interleaved publish connect");
    for sequence in 0..5u32 {
        publisher
            .push(rtflow_testkit::unit(0, Codec::H264, sequence))
            .await
            .unwrap();
    }
    wait_until(Duration::from_secs(3), || {
        mount.snapshot().stats.accepted_frames > 0
    })
    .await;
    let frame = tokio::time::timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .expect("subscriber timeout")
        .expect("subscriber closed");
    assert_eq!(frame.codec, Codec::H264);
    publisher.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn publishes_multiple_tracks_in_one_session() {
    let media = description("publish-av", &[Codec::H264, Codec::Aac]);
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/ingest-av"),
            MountConfig::Publisher {
                description: media.clone(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let mut subscriber = mount.subscribe();
    let url = format!("rtsp://{}/ingest-av", server.local_addr());
    let mut publisher = RtspClient::publish_str(
        &url,
        media,
        ClientOptions::builder()
            .transport(UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    publisher
        .push(rtflow_testkit::unit(0, Codec::H264, 0))
        .await
        .unwrap();
    publisher
        .push(rtflow_testkit::unit(1, Codec::Aac, 0))
        .await
        .unwrap();
    wait_until(Duration::from_secs(3), || {
        mount.snapshot().stats.accepted_frames >= 2
    })
    .await;
    let mut codecs = std::collections::HashSet::new();
    while codecs.len() < 2 {
        let frame = tokio::time::timeout(Duration::from_secs(3), subscriber.recv())
            .await
            .unwrap()
            .unwrap();
        codecs.insert(frame.codec);
    }
    assert!(codecs.contains(&Codec::H264));
    assert!(codecs.contains(&Codec::Aac));
    publisher.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[cfg(feature = "srtp")]
#[tokio::test]
async fn publishes_srtp_udp() {
    use base64::Engine as _;
    use rtflow::media::crypto::SdpCrypto;
    let mut media = description("publish-srtp", &[Codec::H264]);
    media.tracks[0].crypto = Some(SdpCrypto {
        tag: 1,
        crypto_suite: "AES_128_CM_HMAC_SHA1_80".into(),
        key_params: format!(
            "inline:{}",
            base64::engine::general_purpose::STANDARD.encode([7u8; 30])
        ),
        session_params: None,
    });
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/ingest-srtp"),
            MountConfig::Publisher {
                description: media.clone(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let mut subscriber = mount.subscribe();
    let url = format!("rtsp://{}/ingest-srtp", server.local_addr());
    let mut publisher = RtspClient::publish_str(
        &url,
        media,
        ClientOptions::builder()
            .transport(UpstreamTransport::Udp)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    publisher
        .push(rtflow_testkit::unit(0, Codec::H264, 0))
        .await
        .unwrap();
    wait_until(Duration::from_secs(3), || {
        mount.snapshot().stats.accepted_frames > 0
    })
    .await;
    let frame = tokio::time::timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.codec, Codec::H264);
    publisher.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}

#[cfg(feature = "srtp")]
#[tokio::test]
async fn publishes_srtp_interleaved() {
    use base64::Engine as _;
    use rtflow::media::crypto::SdpCrypto;
    let mut media = description("publish-srtp", &[Codec::H264]);
    media.tracks[0].crypto = Some(SdpCrypto {
        tag: 1,
        crypto_suite: "AES_128_CM_HMAC_SHA1_80".into(),
        key_params: format!(
            "inline:{}",
            base64::engine::general_purpose::STANDARD.encode([7u8; 30])
        ),
        session_params: None,
    });
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/ingest-srtp-tcp"),
            MountConfig::Publisher {
                description: media.clone(),
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let mut subscriber = mount.subscribe();
    let url = format!("rtsp://{}/ingest-srtp-tcp", server.local_addr());
    let mut publisher = RtspClient::publish_str(
        &url,
        media,
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    publisher
        .push(rtflow_testkit::unit(0, Codec::H264, 0))
        .await
        .unwrap();
    wait_until(Duration::from_secs(3), || {
        mount.snapshot().stats.accepted_frames > 0
    })
    .await;
    let frame = tokio::time::timeout(Duration::from_secs(3), subscriber.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.codec, Codec::H264);
    publisher.teardown().await.unwrap();
    server.shutdown().await.unwrap();
}
