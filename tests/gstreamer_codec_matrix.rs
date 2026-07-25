mod support;

use rtflow::{
    AccessUnit, Codec, FileSource, FileSourceOptions, MountConfig, RtspServer, ServerConfig,
};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use support::{description, wait_until};
use tokio::{io::AsyncReadExt, process::Command};

fn require_plugin(name: &str) {
    let available = std::process::Command::new("gst-inspect-1.0")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(available, "GStreamer plugin {name} is required");
}

fn encoded_sample(codec: Codec) -> AccessUnit {
    let name = match codec {
        Codec::H264 | Codec::Aac => "h264-aac.mp4",
        Codec::H265 => "h265.mp4",
        Codec::Pcma => "pcma.wav",
        Codec::Pcmu => "pcmu.wav",
    };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let source = FileSource::open(path, FileSourceOptions::default()).unwrap();
    let mut demuxer = source.into_demuxer();
    loop {
        let sample = demuxer
            .next_sample()
            .unwrap()
            .expect("fixture ended before the requested codec");
        if sample.unit.codec == codec {
            return sample.unit;
        }
    }
}

async fn run(codec: Codec, protocol: &str, depayloader: &str) {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/matrix"),
            MountConfig::Local {
                description: description("gstreamer-matrix", &[codec]),
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let location = format!("rtsp://{}/matrix", server.local_addr());
    let mut child = Command::new("gst-launch-1.0")
        .args([
            "-q",
            "rtspsrc",
            &format!("location={location}"),
            &format!("protocols={protocol}"),
            "latency=0",
            "!",
            depayloader,
            "!",
            "fdsink",
            "fd=1",
            "sync=false",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_until(Duration::from_secs(5), || {
        mount.snapshot().subscriber_count > 0
    })
    .await;
    let sample = encoded_sample(codec);
    for sequence in 0..8u32 {
        let mut unit = sample.clone();
        unit.track_id = 0;
        unit.pts = Duration::from_millis(u64::from(sequence) * 40);
        unit.rtp_timestamp = sequence.saturating_mul(3_600);
        publisher.try_push(unit).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut output = child.stdout.take().unwrap();
    let mut buffer = [0u8; 64];
    let count = tokio::time::timeout(Duration::from_secs(5), output.read(&mut buffer))
        .await
        .unwrap_or_else(|_| panic!("GStreamer produced no {codec:?} data over {protocol}"))
        .unwrap();
    assert!(
        count > 0,
        "GStreamer emitted EOF for {codec:?} over {protocol}"
    );
    let _ = child.kill().await;
    let _ = child.wait().await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires GStreamer RTP plugins; run in the media interoperability job"]
async fn every_phase_one_codec_depays_over_udp_and_tcp() {
    require_plugin("rtspsrc");
    for depayloader in [
        "rtph264depay",
        "rtph265depay",
        "rtpmp4gdepay",
        "rtppcmadepay",
        "rtppcmudepay",
    ] {
        require_plugin(depayloader);
    }
    for protocol in ["udp", "tcp"] {
        for (codec, depayloader) in [
            (Codec::H264, "rtph264depay"),
            (Codec::H265, "rtph265depay"),
            (Codec::Aac, "rtpmp4gdepay"),
            (Codec::Pcma, "rtppcmadepay"),
            (Codec::Pcmu, "rtppcmudepay"),
        ] {
            run(codec, protocol, depayloader).await;
        }
    }
}
