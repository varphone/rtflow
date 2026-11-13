//! Real ffmpeg publish→playback round trips for AV1/JPEG/H.263/MPEG-4.

use rtflow::{Codec, MountConfig, RtspServer, ServerConfig};
use rtflow_testkit::{description, wait_until};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::process::Command;

fn ffmpeg_bin() -> PathBuf {
    std::env::var_os("FFMPEG")
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            let path = PathBuf::from(home).join(".local/opt/ffmpeg/ffmpeg");
            path.exists().then_some(path)
        })
        .unwrap_or_else(|| PathBuf::from("ffmpeg"))
}

fn encoder_args(codec: Codec) -> Vec<&'static str> {
    match codec {
        Codec::Av1 => vec![
            "-c:v",
            "libsvtav1",
            "-preset",
            "12",
            "-crf",
            "50",
            "-g",
            "10",
            "-pix_fmt",
            "yuv420p",
        ],
        Codec::Jpeg => vec![
            "-c:v",
            "mjpeg",
            "-huffman",
            "default",
            "-q:v",
            "8",
            "-pix_fmt",
            "yuvj420p",
            "-g",
            "10",
            "-payload_type",
            "96",
        ],
        Codec::H263 => vec![
            "-c:v", "h263", "-s", "176x144", "-r", "10", "-g", "10", "-pix_fmt", "yuv420p",
        ],
        Codec::Mpeg4 => vec![
            "-c:v", "mpeg4", "-q:v", "5", "-g", "10", "-pix_fmt", "yuv420p",
        ],
        _ => unreachable!(),
    }
}

async fn run_codec(codec: Codec) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("ffmpeg-new-codec", &[codec]);
    for track in &mut media.tracks {
        track.control = Arc::from(format!("streamid={}", track.id));
        if codec == Codec::Jpeg {
            track.payload_type = 96;
            track.width = Some(176);
            track.height = Some(144);
        }
        if codec == Codec::H263 {
            track.width = Some(176);
            track.height = Some(144);
        }
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
    let mut publish = Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=176x144:rate=10",
            "-t",
            "12",
        ])
        .args(encoder_args(codec))
        .args(if codec == Codec::Av1 {
            vec!["-strict", "-2"]
        } else {
            Vec::new()
        })
        .args([
            "-rtsp_transport",
            "tcp",
            "-muxdelay",
            "0",
            "-f",
            "rtsp",
            &location,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ffmpeg publisher");
    wait_until(Duration::from_secs(20), || {
        mount.snapshot().publisher_connected
    })
    .await;

    let play = Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-loglevel",
            if codec == Codec::Av1 {
                "debug"
            } else {
                "warning"
            },
            "-nostdin",
            "-strict",
            "-2",
            "-rtsp_transport",
            "tcp",
            "-analyzeduration",
            "1000000",
            "-probesize",
            "5000000",
            "-i",
            &location,
            "-t",
            "3",
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("run ffmpeg player");
    let stderr = String::from_utf8_lossy(&play.stderr);
    let accepted = mount.snapshot().stats.accepted_frames;
    publish.kill().await.ok();
    server.shutdown().await.unwrap();
    assert!(
        play.status.success(),
        "{codec:?} ffmpeg playback failed (accepted={accepted}):\n{stderr}"
    );
    assert!(accepted > 0, "{codec:?}: no frames accepted from ffmpeg");
    if codec == Codec::Av1 {
        assert!(
            stderr.contains("Frame "),
            "Av1 ffmpeg decoded no frames:\n{stderr}"
        );
    } else {
        assert!(
            !stderr.to_ascii_lowercase().contains("decode"),
            "{codec:?} ffmpeg reported decode errors:\n{stderr}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ffmpeg_round_trips_legacy_codecs() {
    for codec in [Codec::Mpeg4, Codec::H263, Codec::Jpeg] {
        run_codec(codec).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires FFmpeg master/8.x with AV1 RTP support (see FFMPEG env var)"]
async fn ffmpeg_round_trips_av1() {
    run_codec(Codec::Av1).await;
}
