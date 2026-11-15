//! End-to-end media path: RTMP publish → HLS playlist → recording file.
//!
//! The test needs `ffmpeg` on PATH; it is skipped when the tool is absent so a
//! plain `cargo test` works in minimal environments.
use rtflow_server::{Config, config::PathRule, server::ServerApp};
use std::{path::Path, process::Command, time::Duration};

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread")]
async fn rtmp_publish_reaches_hls_and_recording() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not found; skipping end-to-end streaming test");
        return;
    }
    rtflow::logging::init_with(rtflow::logging::LogConfig {
        filter: Some("rtflow_server=warn".into()),
        ..Default::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let recordings = dir.path().join("recordings");
    let mut config = Config::default();
    config.server.name = "e2e".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.listen = "127.0.0.1:0".parse().unwrap();
    config.rtmp.listen = "127.0.0.1:0".parse().unwrap();
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = String::new();
    config.discovery.public = true;
    config.hls.segment_duration = Duration::from_secs(1);
    config.hls.window = 5;
    config.recording.enabled = true;
    config.recording.root = recordings.clone();
    config.recording.segment_duration = Duration::from_secs(2);
    config.paths.insert(
        "live/*".into(),
        PathRule {
            record: true,
            ..PathRule::default()
        },
    );
    let handle = ServerApp::start(config).await.unwrap();
    let rtmp = handle.rtmp_addr.expect("RTMP listener");
    let http = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=15",
            "-t",
            "8",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-f",
            "flv",
        ])
        .arg(format!("rtmp://{rtmp}/live/e2e"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn ffmpeg");

    // Discovery must report the stream as publishing.
    let mut online = false;
    for _ in 0..40 {
        if let Ok(response) = client
            .get(format!("{http}/api/v1/paths/live/e2e"))
            .send()
            .await
            && response.status().is_success()
        {
            let body = response.text().await.unwrap_or_default();
            if body.contains("\"publisher\":true") {
                online = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(online, "RTMP publisher never appeared in discovery");

    // HLS playlist becomes available after the first segment.
    let mut playlist = String::new();
    for _ in 0..60 {
        if let Ok(response) = client
            .get(format!("{http}/hls/live/e2e/index.m3u8"))
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
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if !playlist.contains("#EXTINF") {
        let response = client
            .get(format!("{http}/hls/live/e2e/index.m3u8"))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        panic!("HLS playlist has no segments: {status} {body}");
    }
    assert!(playlist.contains("#EXT-X-TARGETDURATION"));

    let _ = child.wait();
    // Recording finalizes once the publisher disconnects.
    let mut segments = Vec::new();
    for _ in 0..40 {
        segments = rtsp_segments(&recordings);
        if !segments.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(!segments.is_empty(), "no recording segment was written");
    let size = std::fs::metadata(&segments[0]).unwrap().len();
    assert!(size > 1024, "recording segment too small: {size} bytes");

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
