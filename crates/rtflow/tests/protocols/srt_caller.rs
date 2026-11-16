#![cfg(all(feature = "srt", feature = "server"))]

//! SRT caller (pull) integration: pull MPEG-TS from an ffmpeg SRT listener.
//!
//! Skipped when ffmpeg is absent or was built without libsrt.
use rtflow::srt::{SrtCaller, SrtCallerConfig};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("udp port");
    socket.local_addr().expect("addr").port()
}

fn spawn_listener(port: u16, passphrase: Option<&str>) -> Child {
    let mut url = format!("srt://127.0.0.1:{port}?mode=listener");
    if let Some(passphrase) = passphrase {
        url.push_str(&format!("&passphrase={passphrase}"));
    }
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
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ffmpeg listener")
}

async fn pull_some_media(mut caller: SrtCaller) -> usize {
    let mut total = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), caller.recv()).await {
            Ok(Ok(payload)) => {
                assert!(
                    payload.iter().take(188).any(|byte| *byte == 0x47) || payload.is_empty(),
                    "payload does not look like MPEG-TS"
                );
                total += payload.len();
                if total >= 188 * 20 {
                    break;
                }
            }
            Ok(Err(error)) => panic!("caller recv failed: {error}"),
            Err(_) => {}
        }
    }
    let _ = caller.close().await;
    total
}

#[tokio::test(flavor = "multi_thread")]
async fn srt_caller_pulls_from_listener() {
    if !ffmpeg_supports_srt() {
        eprintln!("ffmpeg with libsrt not found; skipping SRT caller test");
        return;
    }
    let port = free_udp_port();
    let mut listener = spawn_listener(port, None);
    tokio::time::sleep(Duration::from_millis(700)).await;

    let caller = SrtCaller::connect(SrtCallerConfig {
        remote: format!("127.0.0.1:{port}").parse().unwrap(),
        stream_id: Some("live/pull".into()),
        latency_ms: 120,
        ..SrtCallerConfig::default()
    })
    .await
    .expect("caller connects");
    let total = pull_some_media(caller).await;
    assert!(total >= 188 * 20, "received only {total} bytes");

    let _ = listener.kill();
    let _ = listener.wait();
}

#[tokio::test(flavor = "multi_thread")]
async fn srt_caller_pulls_encrypted_stream() {
    if !ffmpeg_supports_srt() {
        eprintln!("ffmpeg with libsrt not found; skipping SRT encrypted caller test");
        return;
    }
    let port = free_udp_port();
    let passphrase = "secretpass12345";
    let mut listener = spawn_listener(port, Some(passphrase));
    tokio::time::sleep(Duration::from_millis(700)).await;

    let caller = SrtCaller::connect(SrtCallerConfig {
        remote: format!("127.0.0.1:{port}").parse().unwrap(),
        stream_id: Some("live/encpull".into()),
        latency_ms: 120,
        passphrase: Some(passphrase.into()),
        pbkeylen: 16,
        ..SrtCallerConfig::default()
    })
    .await
    .expect("encrypted caller connects");
    let total = pull_some_media(caller).await;
    assert!(total >= 188 * 20, "received only {total} bytes");

    let _ = listener.kill();
    let _ = listener.wait();
}
