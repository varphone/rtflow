//! Many proxy mounts share one upstream and fan out to HLS.
use rtflow::MountConfig;
use rtflow::media::Codec;
use rtflow::server::{RtspServer, ServerConfig};
use rtflow_server::{
    Config,
    config::{PathRule, ProxyMount, ProxyUpstream},
    server::ServerApp,
};
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread")]
async fn ten_proxy_mounts_serve_hls() {
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
    let upstream_url = format!("rtsp://{}/cam", upstream.local_addr());

    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.name = "proxy-scale".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = String::new();
    config.discovery.public = true;
    config.hls.segment_duration = Duration::from_secs(1);
    config.hls.window = 5;
    for index in 0..10 {
        config.proxy.mounts.push(ProxyMount {
            path: format!("/proxy/cam{index}"),
            upstreams: vec![ProxyUpstream {
                url: upstream_url.clone(),
                transport: rtflow_server::config::ProxyTransport::Tcp,
                ..ProxyUpstream::default()
            }],
            ..ProxyMount::default()
        });
    }
    config.paths.insert("**".into(), PathRule::default());
    let handle = ServerApp::start(config).await.unwrap();
    let http = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let mut registered = false;
    for _ in 0..100 {
        if let Ok(response) = client.get(format!("{http}/api/v1/proxies")).send().await
            && response.status().is_success()
        {
            let body = response.text().await.unwrap_or_default();
            if body.matches("/proxy/cam").count() >= 10 {
                registered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(registered, "proxy mounts did not register");

    for index in 0..10 {
        let mut playlist = String::new();
        for _ in 0..60 {
            if let Ok(response) = client
                .get(format!("{http}/hls/proxy/cam{index}/index.m3u8"))
                .send()
                .await
                && response.status().is_success()
            {
                let body = response.text().await.unwrap_or_default();
                if body.contains("#EXTINF") {
                    playlist = body;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            playlist.contains("#EXTINF"),
            "no HLS segments for proxy {index}"
        );
    }
    feeder.abort();
    let _ = upstream.shutdown().await;
    handle.shutdown().await;
}
