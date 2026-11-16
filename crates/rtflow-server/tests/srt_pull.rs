//! SRT pull: the server acts as caller and republishes into a mount.
use rtflow_server::{
    Config,
    config::{PathRule, SrtPull},
    server::ServerApp,
};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

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

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("udp")
        .local_addr()
        .expect("addr")
        .port()
}

fn spawn_listener(port: u16) -> Child {
    Command::new("ffmpeg")
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
            "10",
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
        .arg(format!("srt://127.0.0.1:{port}?mode=listener"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg listener")
}

#[tokio::test(flavor = "multi_thread")]
async fn srt_pull_publishes_to_mount_and_hls() {
    if !ffmpeg_supports_srt() {
        eprintln!("ffmpeg with libsrt not found; skipping SRT pull test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let port = free_udp_port();
    let mut listener = spawn_listener(port);
    tokio::time::sleep(Duration::from_millis(700)).await;

    let mut config = Config::default();
    config.server.name = "srt-pull".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.enabled = false;
    config.rtmp.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.recording.enabled = false;
    config.srt.enabled = false;
    config.srt.pull = vec![SrtPull {
        remote: format!("127.0.0.1:{port}"),
        stream_id: Some("live/pulled".into()),
        ..SrtPull::default()
    }];
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin = false;
    config.http.admin_password = String::new();
    config.discovery.public = true;
    config.hls.segment_duration = Duration::from_secs(1);
    config.hls.window = 5;
    config.paths.insert("live/*".into(), PathRule::default());
    let handle = ServerApp::start(config).await.unwrap();
    let http = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let mut online = false;
    for _ in 0..60 {
        if let Ok(response) = client
            .get(format!("{http}/api/v1/paths/live/pulled"))
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
    assert!(online, "pulled SRT mount never came online");

    let stats = client
        .get(format!("{http}/api/v1/srt"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(stats.contains("\"role\":\"caller\""), "{stats}");
    assert!(stats.contains("\"connected\":true"), "{stats}");

    let mut playlist = String::new();
    for _ in 0..40 {
        if let Ok(response) = client
            .get(format!("{http}/hls/live/pulled/index.m3u8"))
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
    assert!(
        playlist.contains("#EXTINF"),
        "no HLS output for pulled stream"
    );

    let _ = listener.kill();
    let _ = listener.wait();
    handle.shutdown().await;
}
