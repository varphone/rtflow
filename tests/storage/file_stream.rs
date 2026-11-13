use bytes::Bytes;
use rtflow::{
    AccessUnit, Codec, ContainerFormat, DemuxedSample, Demuxer, FileSource, FileSourceOptions,
    LoopMode, MediaDescription, MediaKind, MountConfig, RawFormat, RtspServer, ServerConfig,
    TrackDescription,
    client::{ClientOptions, RtspClient},
    pipeline::{IdentityElement, PipelineBuilder, PipelineState, QueueConfig},
    proxy::UpstreamTransport,
};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncReadExt;

struct MediaFixture {
    root: PathBuf,
}

impl MediaFixture {
    fn generate() -> Option<Self> {
        if !command_available("ffmpeg", &["-version"]) {
            eprintln!("skipped: FFmpeg is not installed");
            return None;
        }
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("rtsp-file-stream-{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let fixture = Self { root };
        fixture.ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=96x64:rate=5",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=48000",
            "-t",
            "1",
            "-shortest",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-g",
            "5",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "48k",
            fixture.path("sample.mp4").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-i",
            fixture.path("sample.mp4").to_str().unwrap(),
            "-c",
            "copy",
            fixture.path("sample.mov").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-i",
            fixture.path("sample.mp4").to_str().unwrap(),
            "-c",
            "copy",
            "-movflags",
            "frag_keyframe+empty_moov",
            fixture.path("sample-frag.mp4").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-i",
            fixture.path("sample.mp4").to_str().unwrap(),
            "-c",
            "copy",
            fixture.path("sample.mkv").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-i",
            fixture.path("sample.mp4").to_str().unwrap(),
            "-c",
            "copy",
            "-f",
            "mpegts",
            fixture.path("sample.ts").to_str().unwrap(),
        ]);
        let ts = fs::read(fixture.path("sample.ts")).unwrap();
        assert_eq!(ts.len() % 188, 0);
        let mut m2ts = Vec::with_capacity(ts.len() / 188 * 192);
        let (packets, _) = ts.as_chunks::<188>();
        for packet in packets {
            m2ts.extend_from_slice(&[0, 0, 0, 0]);
            m2ts.extend_from_slice(packet);
        }
        fs::write(fixture.path("sample.m2ts"), m2ts).unwrap();
        fixture.ffmpeg(&[
            "-i",
            fixture.path("sample.mp4").to_str().unwrap(),
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-bsf:v",
            "h264_mp4toannexb",
            "-f",
            "h264",
            fixture.path("sample.h264").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-i",
            fixture.path("sample.mp4").to_str().unwrap(),
            "-map",
            "0:a:0",
            "-c:a",
            "copy",
            "-f",
            "adts",
            fixture.path("sample.aac").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=8000",
            "-t",
            "0.2",
            "-c:a",
            "pcm_alaw",
            fixture.path("sample.wav").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-i",
            fixture.path("sample.wav").to_str().unwrap(),
            "-c:a",
            "copy",
            fixture.path("sample-alaw.mov").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-i",
            fixture.path("sample.wav").to_str().unwrap(),
            "-c:a",
            "copy",
            fixture.path("sample-alaw.mkv").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=8000",
            "-t",
            "0.2",
            "-c:a",
            "pcm_alaw",
            "-f",
            "alaw",
            fixture.path("sample.pcma").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=8000",
            "-t",
            "0.2",
            "-c:a",
            "pcm_mulaw",
            "-f",
            "mulaw",
            fixture.path("sample.pcmu").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=8000",
            "-t",
            "0.2",
            "-c:a",
            "pcm_mulaw",
            fixture.path("sample-ulaw.wav").to_str().unwrap(),
        ]);
        fixture.ffmpeg(&[
            "-i",
            fixture.path("sample.mp4").to_str().unwrap(),
            "-map",
            "0:v:0",
            "-an",
            "-c",
            "copy",
            fixture.path("video.mp4").to_str().unwrap(),
        ]);
        if command_available("ffmpeg", &["-hide_banner", "-encoders"])
            && Command::new("ffmpeg")
                .args(["-hide_banner", "-encoders"])
                .output()
                .ok()
                .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains("libx265"))
        {
            fixture.ffmpeg(&[
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x48:rate=5",
                "-t",
                "0.4",
                "-threads",
                "1",
                "-c:v",
                "libx265",
                "-x265-params",
                "pools=1:frame-threads=1:log-level=error",
                "-pix_fmt",
                "yuv420p",
                fixture.path("sample-h265.mkv").to_str().unwrap(),
            ]);
            fixture.ffmpeg(&[
                "-i",
                fixture.path("sample-h265.mkv").to_str().unwrap(),
                "-c:v",
                "copy",
                "-tag:v",
                "hvc1",
                fixture.path("sample-h265.mp4").to_str().unwrap(),
            ]);
            fixture.ffmpeg(&[
                "-i",
                fixture.path("sample-h265.mkv").to_str().unwrap(),
                "-c:v",
                "copy",
                "-f",
                "mpegts",
                fixture.path("sample-h265.ts").to_str().unwrap(),
            ]);
            fixture.ffmpeg(&[
                "-i",
                fixture.path("sample-h265.mkv").to_str().unwrap(),
                "-c:v",
                "copy",
                "-f",
                "hevc",
                fixture.path("sample.h265").to_str().unwrap(),
            ]);
        }
        Some(fixture)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn ffmpeg(&self, args: &[&str]) {
        let output = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "FFmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for MediaFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn command_available(command: &str, args: &[&str]) -> bool {
    Command::new(command)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn inspect(path: &Path, format: ContainerFormat, expected: &[Codec]) {
    let options = FileSourceOptions {
        format,
        pause_when_idle: false,
        ..Default::default()
    };
    let source = FileSource::open(path, options).unwrap();
    let codecs = source
        .description()
        .tracks
        .iter()
        .map(|track| track.codec)
        .collect::<Vec<_>>();
    assert_eq!(codecs, expected, "unexpected tracks for {}", path.display());
    let mut demuxer = source.into_demuxer();
    demuxer.seek(Duration::ZERO).unwrap();
    let mut seen = HashSet::new();
    let mut samples = 0usize;
    while samples < 256 {
        let Some(sample) = demuxer.next_sample().unwrap() else {
            break;
        };
        assert!(!sample.unit.data.is_empty());
        assert!(sample.unit.track_id < expected.len() as u8);
        if sample.unit.codec == Codec::Aac {
            assert!(
                !(sample.unit.data.len() > 1
                    && sample.unit.data[0] == 0xff
                    && sample.unit.data[1] & 0xf6 == 0xf0),
                "ADTS header was not stripped"
            );
        }
        seen.insert(sample.unit.codec);
        samples += 1;
    }
    assert!(samples > 0, "no samples read from {}", path.display());
    for codec in expected {
        assert!(seen.contains(codec), "missing {codec:?} samples");
    }
}

#[tokio::test]
#[ignore = "requires FFmpeg; committed fixtures cover the default format matrix"]
async fn mainstream_files_demux_and_stream_over_rtsp() {
    let Some(fixture) = MediaFixture::generate() else {
        return;
    };
    inspect(
        &fixture.path("sample.mp4"),
        ContainerFormat::Auto,
        &[Codec::H264, Codec::Aac],
    );
    inspect(
        &fixture.path("sample.mov"),
        ContainerFormat::Auto,
        &[Codec::H264, Codec::Aac],
    );
    inspect(
        &fixture.path("sample-frag.mp4"),
        ContainerFormat::Auto,
        &[Codec::H264, Codec::Aac],
    );
    inspect(
        &fixture.path("sample.mkv"),
        ContainerFormat::Auto,
        &[Codec::H264, Codec::Aac],
    );
    inspect(
        &fixture.path("sample.ts"),
        ContainerFormat::Auto,
        &[Codec::H264, Codec::Aac],
    );
    inspect(
        &fixture.path("sample.m2ts"),
        ContainerFormat::Auto,
        &[Codec::H264, Codec::Aac],
    );
    inspect(
        &fixture.path("sample.h264"),
        ContainerFormat::Auto,
        &[Codec::H264],
    );
    inspect(
        &fixture.path("sample.aac"),
        ContainerFormat::Auto,
        &[Codec::Aac],
    );
    inspect(
        &fixture.path("sample.wav"),
        ContainerFormat::Auto,
        &[Codec::Pcma],
    );
    inspect(
        &fixture.path("sample-alaw.mov"),
        ContainerFormat::Auto,
        &[Codec::Pcma],
    );
    inspect(
        &fixture.path("sample-alaw.mkv"),
        ContainerFormat::Auto,
        &[Codec::Pcma],
    );
    inspect(
        &fixture.path("sample.pcma"),
        ContainerFormat::Auto,
        &[Codec::Pcma],
    );
    inspect(
        &fixture.path("sample.pcmu"),
        ContainerFormat::Auto,
        &[Codec::Pcmu],
    );
    inspect(
        &fixture.path("sample-ulaw.wav"),
        ContainerFormat::Auto,
        &[Codec::Pcmu],
    );
    if fixture.path("sample-h265.mkv").exists() {
        inspect(
            &fixture.path("sample-h265.mkv"),
            ContainerFormat::Auto,
            &[Codec::H265],
        );
        inspect(
            &fixture.path("sample-h265.mp4"),
            ContainerFormat::Auto,
            &[Codec::H265],
        );
        inspect(
            &fixture.path("sample-h265.ts"),
            ContainerFormat::Auto,
            &[Codec::H265],
        );
        inspect(
            &fixture.path("sample.h265"),
            ContainerFormat::Raw(RawFormat::H265),
            &[Codec::H265],
        );
    }

    let options = FileSourceOptions {
        loop_mode: LoopMode::Once,
        pause_when_idle: true,
        ..Default::default()
    };
    let source = FileSource::open(fixture.path("sample.mp4"), options).unwrap();
    let description = source.description().clone();
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(Arc::from("/file"), MountConfig::Local { description })
        .await
        .unwrap();
    let publisher = mount.publisher().unwrap();
    let url = url::Url::parse(&format!("rtsp://{}/file", server.local_addr())).unwrap();
    let mut client = RtspClient::connect(
        url,
        ClientOptions::builder()
            .transport(UpstreamTransport::TcpInterleaved)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let playback = source.play(publisher);
    let mut received = HashSet::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while received.len() < 2 {
            let unit = client.next().await.unwrap().unwrap();
            received.insert(unit.codec);
        }
    })
    .await
    .unwrap();
    assert!(received.contains(&Codec::H264));
    assert!(received.contains(&Codec::Aac));
    let stats = tokio::time::timeout(Duration::from_secs(3), playback.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(stats.accepted_samples > 0);
    assert_eq!(stats.loops_completed, 1);
    server.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires FFmpeg and GStreamer; run in the media interoperability job"]
async fn gstreamer_consumes_h264_from_mp4_file() {
    if !command_available("gst-inspect-1.0", &["rtspsrc"]) {
        return;
    }
    let Some(fixture) = MediaFixture::generate() else {
        return;
    };
    let source = FileSource::open(fixture.path("video.mp4"), FileSourceOptions::default()).unwrap();
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(
            Arc::from("/movie"),
            MountConfig::Local {
                description: source.description().clone(),
            },
        )
        .await
        .unwrap();
    let location = format!("rtsp://{}/movie", server.local_addr());
    let mut child = tokio::process::Command::new("gst-launch-1.0")
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
    let playback = source.play(mount.publisher().unwrap());
    let mut stdout = child.stdout.take().unwrap();
    let mut received = [0u8; 64];
    let count = tokio::time::timeout(Duration::from_secs(5), stdout.read(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert!(count > 0, "GStreamer depayloader produced no MP4 bytes");
    playback.stop().await.unwrap();
    let _ = child.kill().await;
    let _ = child.wait().await;
    server.shutdown().await.unwrap();
}

struct SyntheticDemuxer {
    description: MediaDescription,
    index: u32,
}

impl SyntheticDemuxer {
    fn new() -> Self {
        Self {
            description: MediaDescription {
                session_name: Arc::from("synthetic-file"),
                tracks: vec![TrackDescription {
                    id: 0,
                    kind: MediaKind::Video,
                    codec: Codec::H264,
                    clock_rate: 90_000,
                    channels: None,
                    payload_type: 96,
                    fmtp: None,
                    control: Arc::from("trackID=0"),
                    crypto: None,
                    width: None,
                    height: None,
                    frame_rate: None,
                    direction: None,
                    ptime: None,
                    rtcp: None,
                    bandwidth_as: None,
                }],
                connection: None,
            },
            index: 0,
        }
    }
}

impl Demuxer for SyntheticDemuxer {
    fn description(&self) -> &MediaDescription {
        &self.description
    }
    fn duration(&self) -> Option<Duration> {
        Some(Duration::from_millis(20))
    }
    fn next_sample(&mut self) -> rtflow::Result<Option<DemuxedSample>> {
        if self.index >= 2 {
            return Ok(None);
        }
        let pts = Duration::from_millis(u64::from(self.index) * 10);
        self.index += 1;
        Ok(Some(DemuxedSample {
            unit: AccessUnit {
                track_id: 0,
                codec: Codec::H264,
                pts,
                dts: Some(pts),
                rtp_timestamp: (pts.as_millis() as u32) * 90,
                is_keyframe: true,
                data: Bytes::from_static(&[0x65, 1, 2]),
            },
            duration: Some(Duration::from_millis(10)),
        }))
    }
    fn seek(&mut self, position: Duration) -> rtflow::Result<()> {
        self.index = if position >= Duration::from_millis(10) {
            1
        } else {
            0
        };
        Ok(())
    }
}

#[tokio::test]
async fn file_source_loops_into_pipeline_and_mount_cancel_stops_it() {
    let mut builder = PipelineBuilder::new();
    let element = builder
        .add_element(
            Arc::new(IdentityElement::new("file-input")),
            QueueConfig::default(),
        )
        .unwrap();
    let pipeline = builder.build().await.unwrap();
    pipeline.set_state(PipelineState::Playing).await.unwrap();
    let source = FileSource::from_demuxer(
        "synthetic",
        Box::new(SyntheticDemuxer::new()),
        FileSourceOptions {
            loop_mode: LoopMode::Repeat(1),
            pause_when_idle: false,
            ..Default::default()
        },
    )
    .unwrap();
    let stats = source
        .play_into_pipeline(pipeline.ingress(), element)
        .wait()
        .await
        .unwrap();
    assert_eq!(stats.accepted_samples, 4);
    assert_eq!(stats.loops_completed, 2);
    tokio::time::timeout(Duration::from_secs(1), async {
        while pipeline.snapshot().elements[0].2.processed < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    pipeline.shutdown().await.unwrap();

    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let description = SyntheticDemuxer::new().description;
    let mount = server
        .register_mount(
            Arc::from("/cancel-file"),
            MountConfig::Local { description },
        )
        .await
        .unwrap();
    let source = FileSource::from_demuxer(
        "synthetic",
        Box::new(SyntheticDemuxer::new()),
        FileSourceOptions {
            loop_mode: LoopMode::Forever,
            ..Default::default()
        },
    )
    .unwrap();
    let playback = source.play(mount.publisher().unwrap());
    server.unregister_mount("/cancel-file").await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), playback.wait())
        .await
        .unwrap()
        .unwrap();
    server.shutdown().await.unwrap();
}
