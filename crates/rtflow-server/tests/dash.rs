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

fn attrs<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let needle = format!("{name}=\"");
    let mut values = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&needle) {
        let value_start = start + needle.len();
        let tail = &rest[value_start..];
        let Some(end) = tail.find('"') else {
            break;
        };
        values.push(&tail[..end]);
        rest = &tail[end + 1..];
    }
    values
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
    assert_eq!(mpd.matches("<AdaptationSet").count(), 2, "{mpd}");
    assert!(mpd.contains("mimeType=\"video/mp4\""), "{mpd}");
    assert!(mpd.contains("mimeType=\"audio/mp4\""), "{mpd}");
    let codecs = attrs(&mpd, "codecs");
    assert!(
        codecs.iter().any(|codec| codec.contains("avc1")),
        "{codecs:?}"
    );
    assert!(
        codecs.iter().any(|codec| codec.contains("mp4a")),
        "{codecs:?}"
    );
    assert!(mpd.contains("urn:mpeg:dash:utc:http-iso:2014"), "{mpd}");
    assert!(mpd.contains("value=\"/api/v1/time\""), "{mpd}");
    let response = client
        .get(format!("{http}/api/v1/time"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let time = response.text().await.unwrap();
    assert!(time.ends_with('Z'), "time={time}");

    let initializations = attrs(&mpd, "initialization");
    assert!(
        initializations.contains(&"video-0-init.mp4"),
        "{initializations:?}"
    );
    assert!(
        initializations.contains(&"audio-init.mp4"),
        "{initializations:?}"
    );
    let media = attrs(&mpd, "media");
    let starts = attrs(&mpd, "startNumber");
    let video_media = media
        .iter()
        .find(|value| value.starts_with("video-0-"))
        .expect("video media template");
    let audio_media = media
        .iter()
        .find(|value| value.starts_with("audio-"))
        .expect("audio media template");
    let video_start = starts.first().copied().unwrap_or("1");
    let audio_start = starts.get(1).copied().unwrap_or("1");
    let video_segment = video_media.replace("$Number$", video_start);
    let audio_segment = audio_media.replace("$Number$", audio_start);
    assert!(!video_segment.starts_with('/'), "{video_segment}");
    assert!(!audio_segment.starts_with('/'), "{audio_segment}");

    for init in ["video-0-init.mp4", "audio-init.mp4"] {
        let init_url = format!("{http}/dash/live/dash/{init}");
        let response = client.get(&init_url).send().await.unwrap();
        assert_eq!(response.status(), 200, "init URL {init_url}");
        let data = response.bytes().await.unwrap();
        assert!(data.len() > 8, "{init} too small");
        assert_eq!(&data[4..8], b"ftyp", "{init} is not an fMP4 init segment");
    }
    for segment in [video_segment, audio_segment] {
        let segment_url = format!("{http}/dash/live/dash/{segment}");
        let response = client.get(&segment_url).send().await.unwrap();
        assert_eq!(response.status(), 200, "segment URL {segment_url}");
        let data = response.bytes().await.unwrap();
        assert!(!data.is_empty(), "segment is empty: {segment_url}");
    }

    assert!(mpd.contains("timeShiftBufferDepth=\"PT5.000S\""), "{mpd}");

    let _ = child.kill();
    let _ = child.wait();
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dash_vod_from_recording() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not found; skipping DASH VOD test");
        return;
    }
    rtflow::logging::init_with(rtflow::logging::LogConfig {
        filter: Some("rtflow_server=warn,rtflow_dash=warn".into()),
        ..Default::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let recordings = dir.path().join("recordings");
    let mut config = Config::default();
    config.server.name = "dash-vod".into();
    config.server.state_dir = dir.path().join("state");
    config.rtsp.listen = "127.0.0.1:0".parse().unwrap();
    config.rtmp.listen = "127.0.0.1:0".parse().unwrap();
    config.srt.enabled = false;
    config.webrtc.enabled = false;
    config.flv.enabled = false;
    config.http.listen = "127.0.0.1:0".parse().unwrap();
    config.http.admin_password = "testpass".into();
    config.discovery.public = true;
    config.hls.segment_duration = Duration::from_secs(1);
    config.hls.window = 5;
    config.recording.enabled = true;
    config.recording.root = recordings;
    config.recording.segment_duration = Duration::from_secs(2);
    config.paths.insert(
        "live/*".into(),
        rtflow_server::config::PathRule {
            record: true,
            ..rtflow_server::config::PathRule::default()
        },
    );
    let handle = ServerApp::start(config).await.unwrap();
    let rtmp = handle.rtmp_addr.expect("RTMP listener");
    let http = format!("http://{}", handle.http_addr);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();

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
        .arg(format!("rtmp://{rtmp}/live/vod-dash"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("spawn ffmpeg");
    let _ = child.wait();

    let mut dash_url = String::new();
    for _ in 0..80 {
        let _ = client
            .post(format!("{http}/admin/api/v1/vods/refresh"))
            .basic_auth("admin", Some("testpass"))
            .send()
            .await;
        if let Ok(response) = client
            .get(format!("{http}/admin/api/v1/vods"))
            .basic_auth("admin", Some("testpass"))
            .send()
            .await
            && let Ok(vods) = response.json::<serde_json::Value>().await
            && let Some(item) = vods.as_array().and_then(|items| items.first())
            && let Some(dash) = item["dash"].as_str()
        {
            dash_url = format!("{http}{dash}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(!dash_url.is_empty(), "DASH VOD did not appear");
    let response = client.get(&dash_url).send().await.unwrap();
    assert_eq!(response.status(), 200, "manifest {dash_url}");
    let mpd = response.text().await.unwrap();
    assert!(mpd.contains("type=\"static\""), "{mpd}");
    assert!(mpd.contains("video-0-init.mp4"), "{mpd}");
    assert!(mpd.contains("audio-init.mp4"), "{mpd}");
    assert_eq!(mpd.matches("<AdaptationSet").count(), 2, "{mpd}");

    let base = dash_url.trim_end_matches("/manifest.mpd");
    for init in ["video-0-init.mp4", "audio-init.mp4"] {
        let response = client.get(format!("{base}/{init}")).send().await.unwrap();
        assert_eq!(response.status(), 200, "init {init}");
        let data = response.bytes().await.unwrap();
        assert!(data.len() > 8, "{init} too small");
        assert_eq!(&data[4..8], b"ftyp", "{init} is not fMP4");
    }
    let starts = attrs(&mpd, "startNumber");
    let media = attrs(&mpd, "media");
    let video = media
        .iter()
        .find(|value| value.starts_with("video-"))
        .expect("video media");
    let audio = media
        .iter()
        .find(|value| value.starts_with("audio-"))
        .expect("audio media");
    for (template, start) in [
        (video, starts.first().copied().unwrap_or("0")),
        (audio, starts.get(1).copied().unwrap_or("0")),
    ] {
        let name = template.replace("$Number$", start);
        let response = client.get(format!("{base}/{name}")).send().await.unwrap();
        assert_eq!(response.status(), 200, "segment {name}");
        assert!(!response.bytes().await.unwrap().is_empty(), "empty {name}");
    }
    handle.shutdown().await;
}
