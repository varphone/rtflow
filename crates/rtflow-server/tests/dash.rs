//! DASH live end-to-end: RTMP publish → MPD → init/segment fetch.
//!
//! Requires `ffmpeg` on PATH; skipped when unavailable.
use rtflow_server::{Config, server::ServerApp};
use std::{process::Command, time::Duration};

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn attr<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = xml.find(&needle)? + needle.len();
    let rest = &xml[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

#[tokio::test(flavor = "multi_thread")]
async fn dash_publish_serves_manifest_init_and_segment() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not found; skipping DASH end-to-end test");
        return;
    }
    rtflow::logging::init_with(rtflow::logging::LogConfig {
        filter: Some("rtflow_server=warn,rtflow_dash=warn".into()),
        ..Default::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.server.name = "dash-e2e".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.listen = "127.0.0.1:0".parse().unwrap();
    config.rtmp.listen = "127.0.0.1:0".parse().unwrap();
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = String::new();
    config.discovery.public = true;
    config.hls.segment_duration = Duration::from_secs(1);
    config.hls.window = 5;
    config.dash.segment_duration = Some(Duration::from_secs(1));
    config.dash.window = Some(5);
    let handle = ServerApp::start(config).await.unwrap();
    let rtmp = handle.rtmp_addr.expect("RTMP listener");
    let http = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::new();

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=15",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100",
            "-t",
            "6",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "15",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-f",
            "flv",
        ])
        .arg(format!("rtmp://{rtmp}/live/dash"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn ffmpeg");

    let manifest_url = format!("{http}/dash/live/dash/manifest.mpd");
    let mut mpd = String::new();
    for _ in 0..80 {
        if let Ok(response) = client.get(&manifest_url).send().await
            && response.status().is_success()
        {
            let body = response.text().await.unwrap_or_default();
            if body.contains("<SegmentTimeline>") {
                mpd = body;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        mpd.contains("<SegmentTimeline>"),
        "DASH manifest did not appear: {mpd}"
    );
    assert_eq!(mpd.matches("<AdaptationSet").count(), 1, "{mpd}");
    assert!(mpd.contains("mimeType=\"video/mp4\""), "{mpd}");
    let codecs = attr(&mpd, "codecs").unwrap_or_default();
    assert!(codecs.contains("avc1"), "codecs={codecs}");
    assert!(codecs.contains("mp4a"), "codecs={codecs}");
    assert!(mpd.contains("<UTCTiming"), "{mpd}");

    let initialization = attr(&mpd, "initialization").expect("initialization URL");
    let media = attr(&mpd, "media").expect("media template");
    let start_number = attr(&mpd, "startNumber").unwrap_or("1");
    let segment = media.replace("$Number$", start_number);
    assert!(!initialization.starts_with('/'), "{initialization}");
    assert!(!segment.starts_with('/'), "{segment}");

    let init_url = format!("{http}/dash/live/dash/{initialization}");
    let response = client.get(&init_url).send().await.unwrap();
    assert_eq!(response.status(), 200, "init URL {init_url}");
    let init = response.bytes().await.unwrap();
    assert!(init.len() > 8, "init too small");
    assert_eq!(&init[4..8], b"ftyp", "init is not an fMP4 init segment");

    let segment_url = format!("{http}/dash/live/dash/{segment}");
    let response = client.get(&segment_url).send().await.unwrap();
    assert_eq!(response.status(), 200, "segment URL {segment_url}");
    let data = response.bytes().await.unwrap();
    assert!(!data.is_empty(), "segment is empty");

    assert!(mpd.contains("timeShiftBufferDepth=\"PT5.000S\""), "{mpd}");

    let _ = child.kill();
    let _ = child.wait();
    handle.shutdown().await;
}
