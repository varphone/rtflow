mod support;

use rtflow::{Codec, MountConfig, RtspServer, ServerConfig};
use std::{process::Stdio, sync::Arc, time::Duration};
use support::{description, wait_until};
use tokio::{io::AsyncReadExt, process::Command};

fn require_command(command: &str, args: &[&str]) {
    let status = std::process::Command::new(command)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap_or_else(|error| panic!("{command} is required: {error}"));
    assert!(status.success(), "{command} {:?} failed", args);
}

#[tokio::test]
#[ignore = "requires FFmpeg encoders and GStreamer RTSP/RTP plugins"]
async fn ffmpeg_generated_stream_is_forwarded_to_gstreamer() {
    require_command("ffmpeg", &["-hide_banner", "-encoders"]);
    for element in [
        "rtspsrc",
        "queue",
        "rtph264depay",
        "h264parse",
        "avdec_h264",
        "rtpmp4gdepay",
        "aacparse",
        "avdec_aac",
        "fakesink",
    ] {
        require_command("gst-inspect-1.0", &[element]);
    }

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mut media = description("live-tool-stream", &[Codec::H264, Codec::Aac]);
    for track in &mut media.tracks {
        track.control = Arc::from(format!("streamid={}", track.id));
    }
    let mount = server
        .register_mount(
            Arc::from("/live"),
            MountConfig::Publisher {
                description: media,
                allow_replace_after_disconnect: true,
            },
        )
        .await
        .unwrap();
    let location = format!("rtsp://{}/live", server.local_addr());
    let mut ffmpeg = Command::new("ffmpeg")
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
            "-g",
            "15",
            "-c:a",
            "aac",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-rtsp_transport",
            "tcp",
            "-f",
            "rtsp",
            &location,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(10), || {
        mount.snapshot().publisher_connected
    })
    .await;

    let baseline = mount.snapshot().stats.accepted_frames;
    let mut gstreamer = Command::new("gst-launch-1.0")
        .args([
            "-q",
            "-e",
            "rtspsrc",
            &format!("location={location}"),
            "protocols=tcp",
            "latency=50",
            "name=source",
            "source.",
            "!",
            "queue",
            "!",
            "application/x-rtp,media=video,encoding-name=H264",
            "!",
            "rtph264depay",
            "!",
            "h264parse",
            "config-interval=-1",
            "!",
            "avdec_h264",
            "!",
            "fdsink",
            "fd=1",
            "sync=true",
            "source.",
            "!",
            "queue",
            "!",
            "application/x-rtp,media=audio,encoding-name=MPEG4-GENERIC",
            "!",
            "rtpmp4gdepay",
            "!",
            "aacparse",
            "!",
            "avdec_aac",
            "!",
            "fakesink",
            "sync=false",
            "async=false",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut decoded_video = gstreamer.stdout.take().unwrap();
    wait_until(Duration::from_secs(10), || {
        let snapshot = mount.snapshot();
        snapshot.subscriber_count > 0 && snapshot.stats.accepted_frames > baseline + 20
    })
    .await;
    let mut decoded = [0u8; 64];
    let decoded_bytes =
        tokio::time::timeout(Duration::from_secs(10), decoded_video.read(&mut decoded))
            .await
            .expect("GStreamer clock did not release a decoded video frame")
            .unwrap();
    assert!(decoded_bytes > 0, "GStreamer produced no decoded video");
    assert!(ffmpeg.try_wait().unwrap().is_none());
    assert!(gstreamer.try_wait().unwrap().is_none());

    let _ = gstreamer.kill().await;
    let _ = gstreamer.wait().await;
    let _ = ffmpeg.kill().await;
    let _ = ffmpeg.wait().await;
    server.shutdown().await.unwrap();
}
