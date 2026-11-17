//! Static RTSP proxy configuration: upstream RTSP mount → proxy mount → HLS.
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
async fn configured_proxy_reaches_hls() {
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
    config.server.name = "proxy-e2e".into();
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
    config.proxy.mounts.push(ProxyMount {
        path: "/proxy/cam".into(),
        upstreams: vec![ProxyUpstream {
            url: upstream_url,
            transport: rtflow_server::config::ProxyTransport::Tcp,
            ..ProxyUpstream::default()
        }],
        ..ProxyMount::default()
    });
    config.paths.insert("**".into(), PathRule::default());
    let handle = ServerApp::start(config).await.unwrap();
    let http = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let mut registered = false;
    for _ in 0..80 {
        if let Ok(response) = client.get(format!("{http}/api/v1/proxies")).send().await
            && response.status().is_success()
        {
            let body = response.text().await.unwrap_or_default();
            if body.contains("/proxy/cam") {
                registered = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(registered, "proxy status never appeared");

    let mut playlist = String::new();
    for _ in 0..80 {
        if let Ok(response) = client
            .get(format!("{http}/hls/proxy/cam/index.m3u8"))
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
    assert!(playlist.contains("#EXTINF"), "no HLS segments: {playlist}");

    feeder.abort();
    let _ = upstream.shutdown().await;
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn always_proxy_survives_restart() {
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
    config.server.name = "proxy-restart".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = "testpass".into();
    config.discovery.public = true;
    config.proxy.mounts.push(ProxyMount {
        path: "/proxy/always".into(),
        mode: rtflow_server::config::ProxyMode::Always,
        upstreams: vec![ProxyUpstream {
            url: upstream_url,
            transport: rtflow_server::config::ProxyTransport::Tcp,
            ..ProxyUpstream::default()
        }],
        ..ProxyMount::default()
    });
    let handle = ServerApp::start(config).await.unwrap();
    let http = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let mut last_status = String::new();
    let mut streaming = false;
    for _ in 0..80 {
        if let Ok(response) = client.get(format!("{http}/api/v1/proxies")).send().await {
            last_status = response.text().await.unwrap_or_default();
            if last_status.contains("\"state\":\"streaming\"") {
                streaming = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        streaming,
        "always proxy never reached streaming: {last_status}"
    );

    let restarted = client
        .post(format!(
            "{http}/admin/api/v1/proxies/restart?path=%2Fproxy%2Falways"
        ))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap();
    assert!(restarted.status().is_success());

    streaming = false;
    for _ in 0..80 {
        if let Ok(response) = client.get(format!("{http}/api/v1/proxies")).send().await {
            last_status = response.text().await.unwrap_or_default();
            if last_status.contains("\"state\":\"streaming\"") {
                streaming = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        streaming,
        "always proxy did not survive restart: {last_status}"
    );

    feeder.abort();
    let _ = upstream.shutdown().await;
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn schedule_gates_pull_sources() {
    use chrono::{Duration as ChronoDuration, Local};
    use rtflow_server::config::{ProxyMode, ProxySchedule, ScheduleDay};

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

    let now = Local::now();
    let all_days = vec![
        ScheduleDay::Mon,
        ScheduleDay::Tue,
        ScheduleDay::Wed,
        ScheduleDay::Thu,
        ScheduleDay::Fri,
        ScheduleDay::Sat,
        ScheduleDay::Sun,
    ];
    let active = ProxySchedule {
        days: all_days.clone(),
        start: (now - ChronoDuration::hours(1)).format("%H:%M").to_string(),
        end: (now + ChronoDuration::hours(1)).format("%H:%M").to_string(),
    };
    let inactive = ProxySchedule {
        days: all_days,
        start: (now + ChronoDuration::hours(2)).format("%H:%M").to_string(),
        end: (now + ChronoDuration::hours(3)).format("%H:%M").to_string(),
    };

    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.name = "proxy-schedule".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = "testpass".into();
    config.discovery.public = true;
    for (path, schedule) in [("/proxy/active", active), ("/proxy/idle", inactive)] {
        config.proxy.mounts.push(ProxyMount {
            path: path.into(),
            mode: ProxyMode::Always,
            schedule: Some(schedule),
            upstreams: vec![ProxyUpstream {
                url: upstream_url.clone(),
                transport: rtflow_server::config::ProxyTransport::Tcp,
                ..ProxyUpstream::default()
            }],
            ..ProxyMount::default()
        });
    }
    let handle = ServerApp::start(config).await.unwrap();
    let http = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let mut states = Vec::new();
    for _ in 0..80 {
        let body = client
            .get(format!("{http}/admin/api/v1/proxies"))
            .basic_auth("admin", Some("testpass"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        states = body["proxies"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        (
                            item["path"].as_str().unwrap_or_default().to_owned(),
                            item["state"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if states
            .iter()
            .any(|(path, state)| path == "/proxy/active" && state == "streaming")
            && states
                .iter()
                .any(|(path, state)| path == "/proxy/idle" && state == "scheduled")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        states
            .iter()
            .any(|(path, state)| path == "/proxy/active" && state == "streaming"),
        "{states:?}"
    );
    assert!(
        states
            .iter()
            .any(|(path, state)| path == "/proxy/idle" && state == "scheduled"),
        "{states:?}"
    );

    feeder.abort();
    let _ = upstream.shutdown().await;
    handle.shutdown().await;
}
