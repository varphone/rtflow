//! Local end-to-end latency/throughput smoke test for the receive path.
//!
//! Not a micro-benchmark: it pins a bounded, repeatable ceiling so a future
//! change cannot silently add tens of milliseconds to live delivery.
use rtflow::{
    Codec, MountConfig, RtspServer, ServerConfig,
    client::{ClientOptions, RtspClient},
    transport::UpstreamTransport,
};
use rtflow_testkit::description;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::task::JoinSet;

async fn measure(transport: UpstreamTransport) -> (Duration, Duration, f64) {
    let server = RtspServer::bind(ServerConfig::new("127.0.0.1:0".parse().unwrap()))
        .await
        .unwrap();
    let media = description("perf", &[Codec::H264]);
    let mount = server
        .register_mount(
            Arc::from("/perf"),
            MountConfig::Local { description: media },
        )
        .await
        .unwrap();
    let mut client = RtspClient::connect(
        &format!("rtsp://{}/perf", server.local_addr()),
        ClientOptions::builder()
            .transport(transport)
            .output_queue_capacity(256)
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    let publisher = mount.publisher().unwrap();
    let sent: Arc<Mutex<HashMap<u32, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    let sent_by_pusher = sent.clone();
    let mut pusher = JoinSet::new();
    pusher.spawn(async move {
        for sequence in 0..220u32 {
            let unit = rtflow_testkit::unit(0, Codec::H264, sequence);
            sent_by_pusher
                .lock()
                .unwrap()
                .insert(unit.rtp_timestamp, Instant::now());
            if publisher.push(unit).is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });

    let mut latencies = Vec::with_capacity(160);
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while latencies.len() < 160 {
        let unit = tokio::time::timeout_at(deadline, client.next())
            .await
            .expect("receive timeout")
            .expect("session ended")
            .expect("media error");
        if let Some(at) = sent.lock().unwrap().remove(&unit.rtp_timestamp) {
            latencies.push(at.elapsed());
        }
    }
    pusher.abort_all();
    while pusher.join_next().await.is_some() {}
    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    let p99 = latencies[latencies.len() * 99 / 100];
    let rate = latencies.len() as f64 / started.elapsed().as_secs_f64().max(1e-6);
    client.teardown().await.unwrap();
    server.shutdown().await.unwrap();
    (p50, p99, rate)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_receive_latency_is_bounded() {
    for transport in [UpstreamTransport::Udp, UpstreamTransport::TcpInterleaved] {
        let (p50, p99, rate) = measure(transport).await;
        eprintln!("{transport:?}: p50={p50:?} p99={p99:?} app_rate={rate:.0}/s");
        assert!(
            p50 < Duration::from_millis(150),
            "{transport:?} p50 {p50:?} too high"
        );
        assert!(
            p99 < Duration::from_millis(500),
            "{transport:?} p99 {p99:?} too high"
        );
        assert!(rate > 20.0, "{transport:?} delivery rate too low");
    }
}
