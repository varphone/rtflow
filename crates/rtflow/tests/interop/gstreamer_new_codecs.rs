//! GStreamer playback of legacy RTSP codecs from an rtflow server.

use rtflow::{Codec, MountConfig, RtspServer, ServerConfig};
use rtflow_testkit::{description, wait_until};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::process::Command;

fn ffmpeg_encoder(codec: Codec) -> Vec<&'static str> {
    match codec {
        Codec::Jpeg => vec![
            "-c:v", "mjpeg", "-huffman", "default", "-q:v", "8", "-pix_fmt", "yuvj420p", "-g", "10",
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

fn gst_depay(codec: Codec) -> &'static str {
    match codec {
        Codec::Jpeg => "rtpjpegdepay ! jpegdec",
        Codec::H263 => "rtph263depay ! avdec_h263",
        Codec::Mpeg4 => "rtpmp4vdepay ! avdec_mpeg4",
        _ => unreachable!(),
    }
}

async fn run(codec: Codec) {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("gst-new-codec", &[codec]);
    for track in &mut media.tracks {
        track.control = Arc::from(format!("streamid={}", track.id));
        match codec {
            Codec::Jpeg => {
                track.payload_type = 26;
                track.width = Some(176);
                track.height = Some(144);
            }
            Codec::H263 => {
                track.payload_type = 34;
                track.width = Some(176);
                track.height = Some(144);
            }
            _ => {}
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
    let mut publish = Command::new("ffmpeg")
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
            // Publish until the test kills the process: a fixed -t can end the
            // session while a slower depayloader/decoder is still draining,
            // which surfaces as an RTSP teardown race ("Received end-of-file").
        ])
        .args(ffmpeg_encoder(codec))
        .args(["-rtsp_transport", "tcp", "-f", "rtsp", &location])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ffmpeg publisher");
    wait_until(Duration::from_secs(20), || {
        mount.snapshot().publisher_connected
    })
    .await;
    let pipeline = format!(
        "rtspsrc location={location} protocols=tcp latency=0 ! {} ! fakesink num-buffers=10",
        gst_depay(codec)
    );
    let play = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new("gst-launch-1.0")
            .arg("-q")
            .args(pipeline.split_whitespace())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output(),
    )
    .await;
    publish.kill().await.ok();
    server.shutdown().await.unwrap();
    let output = play.expect("gst-launch timed out").expect("run gst-launch");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{codec:?} gst-launch failed:\n{stderr}\n{pipeline}"
    );
    assert!(
        !stderr.contains("ERROR"),
        "{codec:?} gst-launch reported errors:\n{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gstreamer_plays_legacy_codecs() {
    for codec in [Codec::Mpeg4, Codec::Jpeg, Codec::H263] {
        eprintln!("=== {codec:?} ===");
        run(codec).await;
    }
}
