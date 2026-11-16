//! SRT push: mux a live mount to MPEG-TS and send it to another SRT listener
//! (here the same server's listener, registering a second mount).
use rtflow_server::{
    Config,
    config::{PathRule, SrtPush},
    server::ServerApp,
};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

fn ffmpeg_ready() -> bool {
    let Ok(output) = Command::new("ffmpeg")
        .args(["-hide_banner", "-protocols"])
        .output()
    else {
        return false;
    };
    let protocols = String::from_utf8_lossy(&output.stdout);
    protocols.lines().any(|line| line.trim() == "srt")
        && protocols.lines().any(|line| line.trim() == "rtmp")
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("udp")
        .local_addr()
        .expect("addr")
        .port()
}

fn publish_rtmp(addr: std::net::SocketAddr) -> Child {
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
            "flv",
        ])
        .arg(format!("rtmp://{addr}/live/src"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg rtmp")
}

#[tokio::test(flavor = "multi_thread")]
async fn srt_push_relays_mount_to_srt_listener() {
    if !ffmpeg_ready() {
        eprintln!("ffmpeg without srt/rtmp; skipping SRT push test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let srt_port = free_udp_port();
    let mut config = Config::default();
    config.server.name = "srt-push".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.enabled = false;
    config.rtmp.listen = "127.0.0.1:0".parse().unwrap();
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.recording.enabled = false;
    config.srt.enabled = true;
    config.srt.listen = format!("127.0.0.1:{srt_port}").parse().unwrap();
    config.srt.push = vec![SrtPush {
        mount: "/live/src".into(),
        remote: format!("127.0.0.1:{srt_port}"),
        stream_id: Some("live/relayed".into()),
        ..SrtPush::default()
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
    let rtmp = handle.rtmp_addr.expect("rtmp listener");
    let mut publisher = publish_rtmp(rtmp);
    let client = reqwest::Client::new();

    let mut relayed = false;
    for _ in 0..80 {
        if let Ok(response) = client
            .get(format!("{http}/api/v1/paths/live/relayed"))
            .send()
            .await
            && response.status().is_success()
        {
            let body = response.text().await.unwrap_or_default();
            if body.contains("\"publisher\":true") {
                relayed = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(relayed, "SRT push never registered /live/relayed");

    let mut playlist = String::new();
    for _ in 0..40 {
        if let Ok(response) = client
            .get(format!("{http}/hls/live/relayed/index.m3u8"))
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
    assert!(playlist.contains("#EXTINF"), "no HLS for pushed stream");

    let _ = publisher.kill();
    let _ = publisher.wait();
    handle.shutdown().await;
}
