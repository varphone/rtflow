//! SRT HaiCrypt interop: ffmpeg pushes AES-CTR encrypted MPEG-TS.
//!
//! Skipped when ffmpeg is absent or built without libsrt.
use rtflow_server::{Config, config::PathRule, server::ServerApp};
use std::{process::Command, time::Duration};

const PASSPHRASE: &str = "secretpass12345";

fn ffmpeg_supports_srt() -> bool {
    let Ok(output) = Command::new("ffmpeg")
        .args(["-hide_banner", "-protocols"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == "srt")
}

#[tokio::test(flavor = "multi_thread")]
async fn srt_encrypted_publish_reaches_hls() {
    if !ffmpeg_supports_srt() {
        eprintln!("ffmpeg with libsrt not found; skipping SRT encryption test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.name = "srt-enc".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.recording.enabled = false;
    config.srt.listen = "127.0.0.1:0".parse().unwrap();
    config.srt.passphrase = Some(PASSPHRASE.into());
    config.srt.pbkeylen = 16;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin = false;
    config.http.admin_password = String::new();
    config.discovery.public = true;
    config.hls.segment_duration = Duration::from_secs(1);
    config.hls.window = 5;
    config.paths.insert("live/*".into(), PathRule::default());
    let handle = ServerApp::start(config).await.unwrap();
    let srt = handle.srt_addr.expect("SRT listener");
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
            "mpegts",
        ])
        .arg(format!(
            "srt://{srt}?streamid=live/enc&passphrase={PASSPHRASE}&pbkeylen=16"
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn ffmpeg");

    // The mount is registered only after successful decryption and TS demux.
    let mut online = false;
    for _ in 0..60 {
        if let Ok(response) = client
            .get(format!("{http}/api/v1/paths/live/enc"))
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
    assert!(online, "encrypted SRT publisher never appeared");

    let mut playlist = String::new();
    for _ in 0..40 {
        if let Ok(response) = client
            .get(format!("{http}/hls/live/enc/index.m3u8"))
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
    assert!(playlist.contains("#EXTINF"), "no HLS segments: {playlist}");
    let _ = child.kill();
    let _ = child.wait();
    handle.shutdown().await;

    // A wrong passphrase must be rejected instead of silently connecting.
    let mut config = Config::default();
    config.server.state_dir = dir.path().join("state2");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.recording.enabled = false;
    config.srt.listen = "127.0.0.1:0".parse().unwrap();
    config.srt.passphrase = Some(PASSPHRASE.into());
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin = false;
    config.http.admin_password = String::new();
    let handle = ServerApp::start(config).await.unwrap();
    let srt = handle.srt_addr.expect("SRT listener");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=160x120:rate=10",
            "-t",
            "2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "10",
            "-f",
            "mpegts",
        ])
        .arg(format!(
            "srt://{srt}?streamid=live/bad&passphrase=wrongpass12345"
        ))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn ffmpeg");
    assert!(!status.success(), "wrong SRT passphrase must be rejected");
    handle.shutdown().await;
}
