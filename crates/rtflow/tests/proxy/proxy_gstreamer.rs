//! GStreamer rtspsrc pulls a proxy mount (external interop).
use rtflow::{
    MountConfig,
    client::UpstreamTransport,
    media::Codec,
    proxy::UpstreamConfig,
    server::{RtspServer, ServerConfig},
};
use std::{process::Command, sync::Arc, time::Duration};

fn gstreamer_available() -> bool {
    Command::new("gst-launch-1.0")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread")]
async fn gstreamer_plays_proxy_mount() {
    if !gstreamer_available() {
        eprintln!("gst-launch-1.0 not found; skipping proxy interop test");
        return;
    }
    let upstream = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let source = upstream
        .register_mount(
            Arc::from("/cam"),
            MountConfig::Local {
                description: rtflow_testkit::description("cam", &[Codec::H264]),
            },
        )
        .await
        .unwrap();
    let publisher = source.publisher().unwrap();
    let feeder = tokio::spawn(async move {
        let mut sequence = 0u32;
        loop {
            if publisher
                .try_push(rtflow_testkit::unit(0, Codec::H264, sequence))
                .is_err()
            {
                break;
            }
            sequence = sequence.wrapping_add(1);
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    });
    let proxy = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    proxy
        .register_mount(
            Arc::from("/proxy"),
            MountConfig::Proxy {
                upstream: Box::new(UpstreamConfig::new(
                    url::Url::parse(&format!("rtsp://{}/cam", upstream.local_addr())).unwrap(),
                    UpstreamTransport::TcpInterleaved,
                )),
            },
        )
        .await
        .unwrap();

    let location = format!("rtsp://{}/proxy", proxy.local_addr());
    let mut ok = false;
    for _ in 0..3 {
        let location = location.clone();
        let status = tokio::task::spawn_blocking(move || {
            Command::new("gst-launch-1.0")
                .args([
                    "-q",
                    "-e",
                    "rtspsrc",
                    "protocols=tcp",
                    "latency=100",
                    &format!("location={location}"),
                    "!",
                    "fakesink",
                    "num-buffers=30",
                ])
                .status()
        })
        .await
        .unwrap()
        .unwrap();
        if status.success() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(ok, "gst-launch failed repeatedly");
    feeder.abort();
    proxy.shutdown().await.unwrap();
    upstream.shutdown().await.unwrap();
}
