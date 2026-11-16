//! Long-running proxy fan-out soak. Ignored by default; run with:
//! `PROXY_SOAK_SECS=28800 cargo test -p rtflow-server --test proxy_soak -- --ignored --nocapture`
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
#[ignore = "long-running proxy soak"]
async fn proxy_soak() {
    let seconds = std::env::var("PROXY_SOAK_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60);
    let proxies: usize = std::env::var("PROXY_SOAK_MOUNTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
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
    config.server.name = "proxy-soak".into();
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
    for index in 0..proxies {
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
    let base = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    let mut ok = 0u64;
    let mut failed = 0u64;
    let mut cycle = 0u64;
    while std::time::Instant::now() < deadline {
        let mut tasks = Vec::with_capacity(proxies);
        for index in 0..proxies {
            let client = client.clone();
            let url = format!("{base}/hls/proxy/cam{index}/index.m3u8");
            tasks.push(tokio::spawn(async move {
                matches!(
                    client
                        .get(url)
                        .timeout(Duration::from_secs(5))
                        .send()
                        .await,
                    Ok(response) if response.status().is_success()
                )
            }));
        }
        for task in tasks {
            match task.await {
                Ok(true) => ok += 1,
                _ => failed += 1,
            }
        }
        cycle += 1;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    eprintln!("proxy soak: {cycle} cycles, ok={ok}, failed={failed}");
    // The first cycle is startup (describing/connecting); allow 10% overall.
    assert!(
        failed * 10 <= ok,
        "too many soak failures: ok={ok} failed={failed}"
    );
    feeder.abort();
    let _ = upstream.shutdown().await;
    handle.shutdown().await;
}
