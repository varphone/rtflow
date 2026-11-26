//! A previously described upstream remains registered while offline.
use rtflow::MountConfig;
use rtflow::media::Codec;
use rtflow::server::{RtspServer, ServerConfig};
use rtflow_server::{
    Config,
    config::{PathRule, ProxyMount, ProxyUpstream},
    server::ServerApp,
};
use std::{sync::Arc, time::Duration};

fn config(state: &std::path::Path, upstream: String) -> Config {
    let mut config = Config::default();
    config.server.name = "proxy-cache".into();
    config.server.state_dir = state.to_path_buf();
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = String::new();
    config.discovery.public = true;
    config.proxy.mounts.push(ProxyMount {
        path: "/proxy/cam".into(),
        upstreams: vec![ProxyUpstream {
            url: upstream,
            transport: rtflow_server::config::ProxyTransport::Tcp,
            ..ProxyUpstream::default()
        }],
        ..ProxyMount::default()
    });
    config.paths.insert("**".into(), PathRule::default());
    config
}

async fn tracks(client: &reqwest::Client, base: &str) -> Option<u64> {
    let body = client
        .get(format!("{base}/api/v1/proxies"))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    value
        .get("proxies")?
        .as_array()?
        .first()?
        .get("tracks")?
        .as_u64()
}

#[tokio::test(flavor = "multi_thread")]
async fn cached_sdp_keeps_proxy_registered_offline() {
    let upstream = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    upstream
        .register_mount(
            Arc::from("/cam"),
            MountConfig::Local {
                description: rtflow_testkit::description("cam", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let upstream_url = format!("rtsp://{}/cam", upstream.local_addr());
    let dir = tempfile::tempdir().unwrap();
    rtflow_server::install_crypto_provider();
    let client = reqwest::Client::new();

    let first = ServerApp::start(config(dir.path(), upstream_url.clone()))
        .await
        .unwrap();
    let base = format!("http://{}", first.http_addr);
    let mut described = false;
    for _ in 0..40 {
        if tracks(&client, &base).await == Some(1) {
            described = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(described, "initial describe did not register");
    first.shutdown().await;
    upstream.shutdown().await.unwrap();

    let second = ServerApp::start(config(dir.path(), upstream_url))
        .await
        .unwrap();
    let base = format!("http://{}", second.http_addr);
    let mut cached = false;
    for _ in 0..40 {
        if tracks(&client, &base).await == Some(1) {
            cached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        cached,
        "proxy did not register from cached SDP while offline"
    );
    second.shutdown().await;
}
