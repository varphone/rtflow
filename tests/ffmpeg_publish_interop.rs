mod support;

use rtsp::{
    Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    proxy::UpstreamTransport,
};
use std::{collections::HashSet, process::Stdio, sync::Arc, time::Duration};
use support::{description, wait_until, within};
use tokio::process::{Child, Command};

fn require_ffmpeg() {
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .expect("FFmpeg is required for this test");
    assert!(output.status.success(), "FFmpeg could not list encoders");
    let encoders = String::from_utf8_lossy(&output.stdout);
    assert!(encoders.contains("libx264"), "FFmpeg libx264 is required");
    assert!(encoders.contains("aac"), "FFmpeg AAC encoder is required");
}

fn spawn_ffmpeg(location: &str, transport: &str) -> Child {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=15",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000",
            "-map",
            "0:v:0",
            "-map",
            "1:a:0",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-x264-params",
            "repeat-headers=1",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-keyint_min",
            "15",
            "-sc_threshold",
            "0",
            "-c:a",
            "aac",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-rtsp_transport",
            transport,
            "-muxdelay",
            "0",
            "-f",
            "rtsp",
            location,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn run_publish(transport: &str) {
    require_ffmpeg();
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("ffmpeg-live-publish", &[Codec::H264, Codec::Aac]);
    for track in &mut media.tracks {
        track.control = Arc::from(format!("streamid={}", track.id));
    }
    let mount = server
        .register_mount(
            Arc::from("/ingest"),
            MountConfig::Publisher {
                description: media,
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let location = format!("rtsp://{}/ingest", server.local_addr());
    let mut player = RtspClient::connect(
        url::Url::parse(&location).unwrap(),
        ClientOptions {
            transport: UpstreamTransport::TcpInterleaved,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let mut child = spawn_ffmpeg(&location, transport);

    wait_until(Duration::from_secs(10), || {
        mount.snapshot().publisher_connected
    })
    .await;
    let received = within(Duration::from_secs(10), async {
        let mut codecs = HashSet::new();
        while codecs.len() < 2 {
            let unit = player.next().await.unwrap().unwrap();
            assert!(!unit.data.is_empty());
            codecs.insert(unit.codec);
        }
        codecs
    })
    .await;
    assert_eq!(received, HashSet::from([Codec::H264, Codec::Aac]));
    assert!(mount.snapshot().stats.accepted_frames > 0);

    child.kill().await.unwrap();
    child.wait().await.unwrap();
    wait_until(Duration::from_secs(5), || {
        !mount.snapshot().publisher_connected
    })
    .await;
    let accepted_before_replacement = mount.snapshot().stats.accepted_frames;
    let mut replacement = spawn_ffmpeg(&location, transport);
    wait_until(Duration::from_secs(10), || {
        let snapshot = mount.snapshot();
        snapshot.publisher_connected && snapshot.stats.accepted_frames > accepted_before_replacement
    })
    .await;
    replacement.kill().await.unwrap();
    replacement.wait().await.unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires FFmpeg lavfi, libx264, AAC, and the RTSP muxer"]
async fn ffmpeg_generates_h264_aac_over_interleaved_tcp() {
    run_publish("tcp").await;
}

#[tokio::test]
#[ignore = "requires FFmpeg lavfi, libx264, AAC, and the RTSP muxer"]
async fn ffmpeg_generates_h264_aac_over_udp() {
    run_publish("udp").await;
}
