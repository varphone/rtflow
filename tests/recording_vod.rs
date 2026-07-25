mod support;

use rtsp::{
    Codec, Demuxer, MountConfig, RecordOutcome, RecordingCatalog, RecordingConfig,
    RecordingDemuxer, RecordingVodSource, RtspServer, SegmentRecorder, ServerConfig,
    client::{ClientOptions, RtspClient},
    protocol::range::{NptPosition, NptRange},
    proxy::UpstreamTransport,
};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn temporary_directory() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rtsp-recording-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test]
async fn segmented_recording_recovers_seeks_and_streams_as_vod() {
    let directory = temporary_directory();
    let description = support::description("archive", &[Codec::H264]);
    let config = RecordingConfig {
        directory: directory.clone(),
        segment_duration: Duration::from_millis(100),
        max_segment_bytes: 1024 * 1024,
        max_disk_bytes: 8 * 1024 * 1024,
        max_segments: 8,
        queue_capacity: 8,
        sync_on_finalize: false,
    };
    let recorder = SegmentRecorder::start(config, description.clone())
        .await
        .unwrap();
    let sink = recorder.sink();
    for sequence in [0, 6, 12] {
        assert_eq!(
            sink.try_record(support::unit(0, Codec::H264, sequence)),
            RecordOutcome::Accepted
        );
    }
    let report = recorder.shutdown().await.unwrap();
    assert_eq!(report.accepted_units, 3);
    assert_eq!(report.finalized_segments, 3);

    std::fs::write(directory.join("interrupted.partial"), b"incomplete").unwrap();
    assert_eq!(RecordingCatalog::recover(&directory).unwrap(), 1);
    let catalog = RecordingCatalog::scan(&directory, 1024 * 1024).unwrap();
    assert_eq!(catalog.description, description);
    assert_eq!(catalog.segments.len(), 3);

    let mut seekable = RecordingDemuxer::open(&directory, 1024 * 1024).unwrap();
    seekable.seek(Duration::from_millis(100)).unwrap();
    let sought = seekable.next_sample().unwrap().unwrap();
    assert_eq!(sought.unit.pts, Duration::from_millis(120));

    let source = Arc::new(RecordingVodSource::open(&directory, 1024 * 1024).unwrap());
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let mount = server
        .register_mount(Arc::from("/archive"), MountConfig::Vod { source })
        .await
        .unwrap();
    let url = url::Url::parse(&format!("rtsp://{}/archive", server.local_addr())).unwrap();
    let options = ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .play_range(NptRange {
            start: NptPosition::Time(Duration::from_millis(100)),
            end: None,
        })
        .build()
        .unwrap();
    let mut client = RtspClient::connect(url, options).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(frame.codec, Codec::H264);
    assert_eq!(frame.rtp_timestamp, 10_800);
    client.pause().await.unwrap();
    client.play().await.unwrap();
    let resumed = tokio::time::timeout(Duration::from_secs(1), client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(resumed.rtp_timestamp, 21_600);

    let default_options = ClientOptions::builder()
        .transport(UpstreamTransport::TcpInterleaved)
        .build()
        .unwrap();
    let default_url = url::Url::parse(&format!("rtsp://{}/archive", server.local_addr())).unwrap();
    let mut independent = RtspClient::connect(default_url, default_options)
        .await
        .unwrap();
    let beginning = tokio::time::timeout(Duration::from_secs(1), independent.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(beginning.rtp_timestamp, 0);
    drop((client, independent, mount));
    server.shutdown().await.unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}
