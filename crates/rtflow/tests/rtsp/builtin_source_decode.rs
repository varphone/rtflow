//! Decode the built-in live source with a real FFmpeg RTSP client.
#![cfg(feature = "test-source")]

use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig,
    source::{SyntheticSource, SyntheticSourceConfig},
};
use rtflow_testkit::description;
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

fn supports_av1_rtp() -> bool {
    let sdp = std::env::temp_dir().join("rtflow-source-av1.sdp");
    let _ = std::fs::remove_file(&sdp);
    let Ok(status) = std::process::Command::new(ffmpeg_bin())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=size=16x16:d=0.2",
            "-frames:v",
            "1",
            "-c:v",
            "libaom-av1",
            "-strict",
            "-2",
            "-f",
            "rtp",
            "-sdp_file",
            sdp.to_str().unwrap(),
            "rtp://127.0.0.1:9",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    else {
        return false;
    };
    status.success() && std::fs::read_to_string(&sdp).is_ok_and(|text| text.contains("AV1/90000"))
}

async fn decode_one(codec: Codec) {
    let mut media = description("builtin", &[codec]);
    if matches!(
        codec,
        Codec::H264
            | Codec::H265
            | Codec::Vp8
            | Codec::Vp9
            | Codec::Av1
            | Codec::Jpeg
            | Codec::H263
            | Codec::Mpeg4
    ) {
        for track in &mut media.tracks {
            track.width = Some(176);
            track.height = Some(144);
        }
    }
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/live"),
            MountConfig::Local {
                description: media.clone(),
            },
        )
        .await
        .unwrap();
    let source = SyntheticSource::spawn(
        mount.publisher().unwrap(),
        media,
        SyntheticSourceConfig {
            fps: 25,
            keyframe_interval: 10,
        },
    )
    .unwrap();
    let url = format!("rtsp://{}/live", server.local_addr());
    let output = Command::new(ffmpeg_bin())
        .kill_on_drop(true)
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-nostdin",
            "-rtsp_transport",
            "tcp",
            "-i",
            &url,
            "-t",
            "1",
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let output = tokio::time::timeout(Duration::from_secs(10), output).await;
    let output = match output {
        Ok(output) => output,
        Err(_) => {
            source.abort();
            let _ = source.await;
            server.shutdown().await.unwrap();
            panic!("{codec:?} ffmpeg playback timed out");
        }
    };
    source.abort();
    let _ = source.await;
    server.shutdown().await.unwrap();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            eprintln!("ffmpeg unavailable, skipping {codec:?}: {error}");
            return;
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{codec:?} built-in source playback failed:\n{stderr}"
    );
    assert!(
        !stderr.contains("Could not find codec parameters")
            && !stderr.contains("Invalid data")
            && !stderr.contains("decode_slice_header"),
        "{codec:?} built-in source produced an undecodable stream:\n{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builtin_source_plays_with_ffmpeg() {
    for codec in [
        Codec::H264,
        Codec::H265,
        Codec::Aac,
        Codec::Pcma,
        Codec::Pcmu,
        Codec::Opus,
        Codec::Vp8,
        Codec::Vp9,
        Codec::Mp3,
        Codec::G726_16,
        Codec::G726_24,
        Codec::G726_32,
        Codec::G726_40,
        Codec::Jpeg,
        Codec::H263,
        Codec::Mpeg4,
    ] {
        eprintln!("=== {codec:?} ===");
        decode_one(codec).await;
    }
    if supports_av1_rtp() {
        eprintln!("=== Av1 ===");
        decode_one(Codec::Av1).await;
    } else {
        eprintln!("skipping Av1: FFmpeg has no AV1 RTP support");
    }
}
