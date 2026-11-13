use bytes::Bytes;
use rtflow::{
    AccessUnit, Codec, MediaDescription, MediaKind, MountConfig, RtspServer, ServerConfig,
    TrackDescription,
};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

#[tokio::test]
#[ignore = "requires GStreamer; run explicitly in the media interoperability job"]
async fn gstreamer_rtspsrc_receives_interleaved_h264() {
    let available = std::process::Command::new("gst-inspect-1.0")
        .arg("rtspsrc")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(available, "GStreamer rtspsrc is required for this test");

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/gst"),
            MountConfig::Local {
                description: MediaDescription {
                    session_name: Arc::from("gstreamer-interop"),
                    tracks: vec![TrackDescription {
                        id: 0,
                        kind: MediaKind::Video,
                        codec: Codec::H264,
                        clock_rate: 90_000,
                        channels: None,
                        payload_type: 96,
                        fmtp: Some(Arc::from("packetization-mode=1")),
                        control: Arc::from("video"),
                        crypto: None,
                        width: None,
                        height: None,
                        frame_rate: None,
                    }],
                    connection: None,
                },
            },
        )
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let location = format!("rtsp://{}/gst", server.local_addr());
    let mut child = Command::new("gst-launch-1.0")
        .args([
            "-q",
            "rtspsrc",
            &format!("location={location}"),
            "protocols=tcp",
            "latency=0",
            "!",
            "rtph264depay",
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

    tokio::time::timeout(Duration::from_secs(3), async {
        while mount.snapshot().subscriber_count == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let access_unit = Bytes::from_static(&[
        0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0xda, 0x01, 0x40, 0x16, 0xec, 0x04, 0x40, 0, 0, 3, 0,
        0x40, 0, 0, 0x0c, 0x83, 0xc6, 0x0c, 0x92, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 0, 1,
        0x65, 0x88, 0x84, 0x00, 0x0a, 0xf2, 0x62, 0x80,
    ]);
    for index in 0..5 {
        publisher
            .try_push(AccessUnit {
                track_id: 0,
                codec: Codec::H264,
                pts: Duration::from_millis(index * 33),
                dts: None,
                rtp_timestamp: index as u32 * 3_000,
                is_keyframe: true,
                data: access_unit.clone(),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut stdout = child.stdout.take().unwrap();
    let mut received = [0u8; 64];
    let count = tokio::time::timeout(Duration::from_secs(5), stdout.read(&mut received))
        .await
        .expect("GStreamer did not receive an H264 buffer")
        .unwrap();
    assert!(count > 0, "GStreamer depayloader produced no bytes");
    let _ = child.kill().await;
    let _ = child.wait().await;
    server.shutdown().await.unwrap();
}
