//! Pull source provisioning: RTSP upstream → live HLS → recording → VOD.
use rtflow::MountConfig;
use rtflow::media::Codec;
use rtflow::server::{RtspServer, ServerConfig};
use rtflow_server::{
    Config,
    config::{ProxyMode, ProxyMount, ProxyOutputs, ProxyUpstream},
    server::ServerApp,
};
use std::{path::Path, sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread")]
async fn pull_source_provisions_live_recording_and_vod() {
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
    let api_upstream = upstream_url.clone();
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
    let recordings = dir.path().join("recordings");
    let mut config = Config::default();
    config.server.name = "pull-e2e".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = "testpass".into();
    config.discovery.public = true;
    config.hls.segment_duration = Duration::from_secs(1);
    config.hls.window = 5;
    config.recording.enabled = true;
    config.recording.root = recordings.clone();
    config.recording.segment_duration = Duration::from_secs(1);
    config.recording.auto_start = true;
    config.proxy.mounts.push(ProxyMount {
        path: "/pull/cam".into(),
        mode: ProxyMode::Always,
        outputs: Some(ProxyOutputs {
            record: true,
            ..ProxyOutputs::default()
        }),
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

    // The pull source provisions a managed path policy without any [paths] rule.
    let mut record = serde_json::Value::Null;
    for _ in 0..80 {
        if let Ok(response) = client
            .get(format!("{http}/admin/api/v1/paths/pull/cam"))
            .basic_auth("admin", Some("testpass"))
            .send()
            .await
            && response.status().is_success()
        {
            record = response.json().await.unwrap_or_default();
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(record["name"], "pull/cam", "{record}");
    assert_eq!(record["record"], true, "{record}");
    assert_eq!(record["hls"], true, "{record}");
    assert_eq!(record["read"], true, "{record}");
    assert_eq!(record["metadata"]["managed_by"], "rtsp-pull", "{record}");

    // Live HLS.
    let mut playlist = String::new();
    for _ in 0..100 {
        if let Ok(response) = client
            .get(format!("{http}/hls/pull/cam/index.m3u8"))
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

    // Pulled streams are reported as publishing and recording.
    let mut status = serde_json::Value::Null;
    for _ in 0..40 {
        if let Ok(response) = client
            .get(format!("{http}/api/v1/paths/pull/cam"))
            .send()
            .await
            && response.status().is_success()
        {
            status = response.json().await.unwrap_or_default();
            if status["publisher"] == true {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(status["publisher"], true, "{status}");
    assert_eq!(status["recording"], true, "{status}");

    let config = client
        .get(format!("{http}/admin/api/v1/proxies/config"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(config["mounts"][0]["outputs"]["record"], true, "{config}");
    assert_eq!(config["mounts"][0]["managed"], true, "{config}");

    // Recording starts automatically and writes segments.
    let mut segments = Vec::new();
    for _ in 0..100 {
        segments = rtsp_segments(&recordings);
        if !segments.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if segments.is_empty() {
        let mut found = Vec::new();
        let mut stack = vec![recordings.clone()];
        while let Some(dir) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else {
                        found.push(path);
                    }
                }
            }
        }
        panic!("no recording segment was written; files={found:?}");
    }

    let mut recording = serde_json::Value::Null;
    for _ in 0..40 {
        if let Ok(response) = client
            .get(format!("{http}/admin/api/v1/recordings"))
            .basic_auth("admin", Some("testpass"))
            .send()
            .await
        {
            let body: serde_json::Value = response.json().await.unwrap_or_default();
            if let Some(entry) = body
                .as_array()
                .and_then(|items| items.iter().find(|item| item["path"] == "pull/cam"))
            {
                recording = entry.clone();
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(recording["active"], true, "{recording}");
    assert!(recording["segments"].is_number(), "{recording}");

    // VOD catalog picks up the recording directory after a refresh.
    let mut vod = false;
    for _ in 0..80 {
        let _ = client
            .post(format!("{http}/admin/api/v1/vods/refresh"))
            .basic_auth("admin", Some("testpass"))
            .send()
            .await;
        if let Ok(response) = client.get(format!("{http}/api/v1/vods")).send().await {
            let body = response.text().await.unwrap_or_default();
            if body.contains("pull/cam") {
                vod = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(vod, "VOD catalog did not list pull/cam");

    // The paths API refuses to delete a pull-managed policy directly.
    let direct = client
        .delete(format!("{http}/admin/api/v1/paths/pull/cam"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(direct.status(), 409, "{}", direct.text().await.unwrap());

    // Deleting the pull source removes the proxy and its managed path policy.
    let deleted = client
        .delete(format!("{http}/admin/api/v1/proxies?path=%2Fpull%2Fcam"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap();
    assert!(
        deleted.status().is_success(),
        "{}",
        deleted.text().await.unwrap()
    );
    let paths = client
        .get(format!("{http}/admin/api/v1/paths/config"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert!(
        !paths
            .as_array()
            .is_some_and(|records| records.iter().any(|record| record["name"] == "pull/cam")),
        "managed path policy was not removed: {paths}"
    );
    let proxies = client
        .get(format!("{http}/api/v1/proxies"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!proxies.contains("/pull/cam"), "{proxies}");

    // Recreate the same pull source; its cached SDP must not force the
    // always-mode proxy back into the on-demand idle path.
    let recreated = client
        .post(format!("{http}/admin/api/v1/proxies"))
        .basic_auth("admin", Some("testpass"))
        .json(&serde_json::json!({
            "path": "/pull/cam",
            "enabled": true,
            "mode": "always",
            "upstreams": [{"url": api_upstream, "transport": "tcp"}],
            "outputs": {
                "read": true, "hls": true, "flv": true, "dash": false,
                "webrtc": false, "record": true, "max_readers": 0
            }
        }))
        .send()
        .await
        .unwrap();
    assert!(
        recreated.status().is_success(),
        "{}",
        recreated.text().await.unwrap()
    );
    let mut state = String::new();
    for _ in 0..80 {
        if let Ok(response) = client
            .get(format!("{http}/admin/api/v1/proxies"))
            .basic_auth("admin", Some("testpass"))
            .send()
            .await
            && let Ok(body) = response.json::<serde_json::Value>().await
            && let Some(entry) = body["proxies"]
                .as_array()
                .and_then(|items| items.iter().find(|item| item["path"] == "/pull/cam"))
        {
            state = entry["state"].as_str().unwrap_or_default().to_owned();
            if state == "streaming" {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        state, "streaming",
        "cached SDP prevented always-mode reconnect"
    );

    let deleted = client
        .delete(format!("{http}/admin/api/v1/proxies?path=%2Fpull%2Fcam"))
        .basic_auth("admin", Some("testpass"))
        .send()
        .await
        .unwrap();
    assert!(deleted.status().is_success());

    feeder.abort();
    let _ = upstream.shutdown().await;
    handle.shutdown().await;
}

fn rtsp_segments(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "rtspseg")
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
