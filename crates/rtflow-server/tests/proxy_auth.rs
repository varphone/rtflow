//! Proxy mounts must keep downstream auth/path rules: RTSP play needs credentials.
use rtflow::client::{ClientOptions, RtspClient, UpstreamTransport};
use rtflow::media::Codec;
use rtflow::server::{RtspServer, ServerConfig};
use rtflow::{Credentials, MountConfig};
use rtflow_server::{
    Config,
    config::{AuthMode, PathRule, ProxyMount, ProxyUpstream, UserConfig},
    server::ServerApp,
};
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread")]
async fn proxy_play_requires_downstream_credentials() {
    rtflow::logging::init_with(rtflow::logging::LogConfig {
        filter: Some("rtflow_server=warn".into()),
        ..Default::default()
    });
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
    let upstream_url = format!("rtsp://{}/cam", upstream.local_addr());
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

    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.name = "proxy-auth".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.listen = "127.0.0.1:0".parse().unwrap();
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = String::new();
    config.discovery.public = true;
    config.auth.mode = AuthMode::Basic;
    config.auth.users.push(UserConfig {
        username: "viewer".into(),
        password: "secret".into(),
        roles: vec!["read".into()],
    });
    config.proxy.mounts.push(ProxyMount {
        path: "/proxy/cam".into(),
        upstreams: vec![ProxyUpstream {
            url: upstream_url,
            transport: rtflow_server::config::ProxyTransport::Tcp,
            ..ProxyUpstream::default()
        }],
        ..ProxyMount::default()
    });
    config.paths.insert(
        "**".into(),
        PathRule {
            read: true,
            ..PathRule::default()
        },
    );
    let handle = ServerApp::start(config).await.unwrap();
    let rtsp = handle.rtsp_addr.expect("RTSP listener");
    let url = format!("rtsp://{rtsp}/proxy/cam");

    let anonymous = RtspClient::connect(
        url::Url::parse(&url).unwrap(),
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await;
    assert!(
        anonymous.is_err(),
        "proxy play without credentials must fail"
    );

    let mut authenticated = None;
    for _ in 0..40 {
        match RtspClient::connect(
            url::Url::parse(&url).unwrap(),
            ClientOptions::builder()
                .transport(UpstreamTransport::TcpInterleaved)
                .credentials(Credentials {
                    username: "viewer".into(),
                    password: "secret".into(),
                })
                .build()
                .unwrap(),
        )
        .await
        {
            Ok(session) => {
                authenticated = Some(session);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let mut session = authenticated.expect("authenticated proxy play");
    let frame = tokio::time::timeout(Duration::from_secs(3), session.next())
        .await
        .expect("frame timeout")
        .expect("stream ended")
        .expect("media error");
    assert_eq!(frame.track_id, 0);

    feeder.abort();
    let _ = upstream.shutdown().await;
    handle.shutdown().await;
}
