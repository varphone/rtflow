#![cfg(all(feature = "srt", feature = "server"))]

//! SRT sender (push) integration: send MPEG-TS to an ffmpeg SRT listener.
//!
//! Skipped when ffmpeg is absent or was built without libsrt.
use rtflow::mount::MountRegistry;
use rtflow::srt::{SrtCallerConfig, SrtConfig, SrtSender, SrtServer};
use std::io::Read;
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
    std::net::UdpSocket::bind("127.0.0.1:0")
        .expect("udp")
        .local_addr()
        .expect("addr")
        .port()
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(name)
}

/// ffmpeg listening for an SRT caller and decoding to null.
fn spawn_receiver(port: u16, passphrase: Option<&str>, seconds: u32) -> Child {
    let mut url = format!("srt://127.0.0.1:{port}?mode=listener");
    if let Some(passphrase) = passphrase {
        url.push_str(&format!("&passphrase={passphrase}"));
    }
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            &url,
            "-t",
            &seconds.to_string(),
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn ffmpeg receiver")
}

async fn send_fixture(sender: &mut SrtSender) {
    let mut file = std::fs::File::open(fixture("h264-aac.ts")).expect("fixture");
    let mut data = Vec::new();
    file.read_to_end(&mut data).expect("read fixture");
    for chunk in data.chunks(1316) {
        sender.send(chunk).await.expect("send chunk");
    }
    sender.tick().await.expect("tick");
}

#[tokio::test(flavor = "multi_thread")]
async fn srt_sender_pushes_to_listener() {
    if !ffmpeg_supports_srt() {
        eprintln!("ffmpeg with libsrt not found; skipping SRT sender test");
        return;
    }
    let port = free_udp_port();
    let mut receiver = spawn_receiver(port, None, 4);
    tokio::time::sleep(Duration::from_millis(700)).await;

    let mut sender = SrtSender::connect(SrtCallerConfig {
        remote: format!("127.0.0.1:{port}").parse().unwrap(),
        stream_id: Some("live/push".into()),
        ..SrtCallerConfig::default()
    })
    .await
    .expect("sender connects");
    send_fixture(&mut sender).await;
    // Give the receiver time to drain and acknowledge before shutdown.
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = sender.tick().await;
    }
    assert!(sender.stats().bytes_sent > 0);
    let _ = sender.close().await;
    let status = receiver.wait().expect("receiver exits");
    assert!(status.success(), "ffmpeg failed to decode pushed SRT");
}

#[tokio::test(flavor = "multi_thread")]
async fn srt_sender_pushes_encrypted() {
    if !ffmpeg_supports_srt() {
        eprintln!("ffmpeg with libsrt not found; skipping encrypted sender test");
        return;
    }
    let port = free_udp_port();
    let passphrase = "secretpass12345";
    let mut receiver = spawn_receiver(port, Some(passphrase), 4);
    tokio::time::sleep(Duration::from_millis(700)).await;

    let mut sender = SrtSender::connect(SrtCallerConfig {
        remote: format!("127.0.0.1:{port}").parse().unwrap(),
        stream_id: Some("live/push-enc".into()),
        passphrase: Some(passphrase.into()),
        pbkeylen: 16,
        ..SrtCallerConfig::default()
    })
    .await
    .expect("encrypted sender connects");
    send_fixture(&mut sender).await;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = sender.tick().await;
    }
    let _ = sender.close().await;
    let status = receiver.wait().expect("receiver exits");
    assert!(status.success(), "ffmpeg failed to decode encrypted SRT");
}

#[tokio::test(flavor = "multi_thread")]
async fn srt_sender_reaches_internal_listener() {
    let mounts = std::sync::Arc::new(MountRegistry::new(10));
    let server = SrtServer::with_config(
        SrtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            latency_ms: 120,
            ..SrtConfig::default()
        },
        mounts.clone(),
    )
    .await
    .unwrap();
    let addr = server.local_addr().unwrap();
    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    let mut sender = SrtSender::connect(SrtCallerConfig {
        remote: addr,
        stream_id: Some("live/internal".into()),
        ..SrtCallerConfig::default()
    })
    .await
    .expect("sender connects");
    send_fixture(&mut sender).await;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = sender.tick().await;
        if mounts.get_handle("/live/internal").is_some() {
            break;
        }
    }
    assert!(
        mounts.get_handle("/live/internal").is_some(),
        "internal listener did not register the pushed mount"
    );
    let _ = sender.close().await;
    cancel.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn srt_sender_rekeys_without_data_loss() {
    let mounts = std::sync::Arc::new(MountRegistry::new(10));
    let server = SrtServer::with_config(
        SrtConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            latency_ms: 120,
            passphrase: Some("secretpass12345".into()),
            pbkeylen: 16,
            ..SrtConfig::default()
        },
        mounts.clone(),
    )
    .await
    .unwrap();
    let stats = server.stats_handle();
    let addr = server.local_addr().unwrap();
    let cancel = server.cancellation_token();
    tokio::spawn(server.run());

    let mut sender = SrtSender::connect(SrtCallerConfig {
        remote: addr,
        stream_id: Some("live/rekey".into()),
        passphrase: Some("secretpass12345".into()),
        pbkeylen: 16,
        key_refresh_packets: 20,
        ..SrtCallerConfig::default()
    })
    .await
    .expect("sender connects");

    for _ in 0..5 {
        send_fixture(&mut sender).await;
        let _ = sender.tick().await;
    }
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = sender.tick().await;
        let current = stats.read().unwrap().clone();
        if current
            .first()
            .is_some_and(|s| s.bytes_received >= 4 * 8084)
            && mounts.get_handle("/live/rekey").is_some()
        {
            break;
        }
    }
    let current = stats.read().unwrap().clone();
    let entry = current.first().expect("connection stats");
    assert!(entry.bytes_received >= 4 * 8084, "{entry:?}");
    assert_eq!(entry.undecryptable, 0, "{entry:?}");
    assert!(mounts.get_handle("/live/rekey").is_some());
    let _ = sender.close().await;
    cancel.cancel();
}
