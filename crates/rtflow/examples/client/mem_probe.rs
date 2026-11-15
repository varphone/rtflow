//! Memory baseline probe for embedded IPC sizing (docs/audits/embedded-resource.md §3 P0-4).
//!
//! Runs a real RTSP server publishing synthetic H.264, attaches `--clients`
//! consumer sessions (TCP interleaved), and prints VmRSS / VmHWM every
//! `--interval` seconds so you can derive the per-session memory increment:
//!
//! ```bash
//! cargo run --release --example mem_probe -- --clients 4 --duration 30
//! #   VmRSS=...   per-session≈(VmRSS - baseline)/clients
//! ```
//!
//! Acceptance baseline for a 64MB IPC: VmHWM with 2×main + 2×sub sessions
//! should stay well under the product budget (e.g. < 24MB with `--queue 128`).
use rtflow::client::{ClientOptions, RtspClient};
use rtflow::media::{AccessUnit, Codec, MediaDescription, TrackDescription};
use rtflow::transport::UpstreamTransport;
use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::time::Duration;
use url::Url;

#[derive(Debug, clap::Parser)]
struct Args {
    /// Listen address for the probe server.
    #[arg(long, default_value = "127.0.0.1:8554")]
    listen: String,
    /// Number of consumer sessions to attach.
    #[arg(long, default_value_t = 4)]
    clients: usize,
    /// Per-session consumer queue capacity (see memory budget formula).
    #[arg(long, default_value_t = 128)]
    queue: usize,
    /// Print interval in seconds.
    #[arg(long, default_value_t = 5)]
    interval: u64,
    /// Total run duration in seconds (0 = until Ctrl-C).
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// Use ServerConfig::embedded (256 KiB UDP buffers, small queues).
    #[arg(long)]
    embedded: bool,
    /// Number of tracks per mount (1 = video, 2 = video + PCMA).
    #[arg(long, default_value_t = 1)]
    tracks: usize,
    /// Use UDP clients instead of TCP interleaved.
    #[arg(long)]
    udp: bool,
}

fn vm_stat() -> (u64, u64) {
    // (VmRSS KiB, VmHWM KiB) from /proc/self/status; (0,0) if unavailable.
    let mut rss = 0u64;
    let mut hwm = 0u64;
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(v) = line.strip_prefix("VmRSS:") {
                rss = v
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("VmHWM:") {
                hwm = v
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
            }
        }
    }
    (rss, hwm)
}

fn frame(seq: u64) -> AccessUnit {
    // Synthetic ~16KB H.264-ish frame (1080p@4Mbps reference size).
    let mut data = Vec::with_capacity(16 * 1024);
    data.extend_from_slice(if seq.is_multiple_of(30) {
        &[0, 0, 0, 1, 0x65, 0x88, 0x84]
    } else {
        &[0, 0, 0, 1, 0x41, 0x9a]
    });
    while data.len() < 16 * 1024 {
        data.push((seq % 251) as u8);
    }
    AccessUnit {
        track_id: 0,
        codec: Codec::H264,
        pts: Duration::from_micros(seq * 33_333),
        dts: None,
        rtp_timestamp: (seq * 3_000) as u32,
        is_keyframe: seq.is_multiple_of(30),
        data: data.into(),
    }
}

async fn attach_clients(count: usize, url: &str, udp: bool) -> Vec<rtflow::client::ClientSession> {
    let mut clients = Vec::new();
    for _ in 0..count {
        let client = RtspClient::connect(
            Url::parse(url).unwrap(),
            ClientOptions::builder()
                .transport(if udp {
                    UpstreamTransport::Udp
                } else {
                    UpstreamTransport::TcpInterleaved
                })
                .keep_alive_interval(None)
                .build()
                .unwrap(),
        )
        .await
        .expect("client connect");
        clients.push(client);
    }
    clients
}

#[tokio::main]
async fn main() -> rtflow::Result<()> {
    #[cfg(feature = "logging")]
    rtflow::logging::init();
    let args = <Args as clap::Parser>::parse();
    let (base_rss, _) = vm_stat();
    let config = if args.embedded {
        let mut config = ServerConfig::embedded(args.listen.parse().unwrap());
        config.consumer_queue_capacity = args.queue;
        config
    } else {
        ServerConfig::builder(args.listen.parse().unwrap())
            .consumer_queue_capacity(args.queue)
            .build()?
    };
    let server = RtspServer::bind(config).await?;
    let mount = server
        .register_mount(
            "/probe",
            MountConfig::Publisher {
                description: MediaDescription::from_tracks(
                    "probe",
                    if args.tracks >= 2 {
                        vec![
                            TrackDescription::h264_video(0, &[], &[], 1920, 1080, 30),
                            TrackDescription::pcma_audio(1, 8_000, 1),
                        ]
                    } else {
                        vec![TrackDescription::h264_video(0, &[], &[], 1920, 1080, 30)]
                    },
                ),
                allow_replace_after_disconnect: true,
            },
        )
        .await?;
    let publisher = mount.publisher()?;

    // Attach consumer sessions first so no frames are dropped pre-subscribe.
    let clients = attach_clients(
        args.clients,
        &format!("rtsp://{}/probe", server.local_addr()),
        args.udp,
    )
    .await;

    // Publisher: 30fps synthetic stream.
    let publish = tokio::spawn(async move {
        let mut seq = 0u64;
        loop {
            let _ = publisher.try_push(frame(seq));
            if args.tracks >= 2 {
                let mut audio = vec![0xd5u8; 160];
                audio[0] = (seq % 251) as u8;
                let _ = publisher.try_push(AccessUnit {
                    track_id: 1,
                    codec: Codec::Pcma,
                    pts: Duration::from_micros(seq * 33_333),
                    dts: None,
                    rtp_timestamp: (seq * 160) as u32,
                    is_keyframe: true,
                    data: audio.into(),
                });
            }
            seq += 1;
            tokio::time::sleep(Duration::from_millis(33)).await;
        }
    });
    // Consumers: drain to keep queues from filling (real clients consume).
    let mut drains = Vec::new();
    for mut client in clients {
        drains.push(tokio::spawn(async move {
            loop {
                if client.next().await.is_none() {
                    break;
                }
            }
        }));
    }

    println!(
        "baseline VmRSS={}KiB; clients={} queue={}; interval={}s embedded={} tracks={} udp={}",
        base_rss, args.clients, args.queue, args.interval, args.embedded, args.tracks, args.udp
    );
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(args.interval)).await;
        let (rss, hwm) = vm_stat();
        let delta = rss.saturating_sub(base_rss);
        let per_session = if args.clients > 0 {
            delta / args.clients as u64
        } else {
            0
        };
        println!(
            "t={:>3}s VmRSS={:>7}KiB VmHWM={:>7}KiB Δ={:>7}KiB per-session≈{:>6}KiB sessions={}",
            started.elapsed().as_secs(),
            rss,
            hwm,
            delta,
            per_session,
            mount.snapshot().subscriber_count,
        );
        if args.duration > 0 && started.elapsed().as_secs() >= args.duration {
            break;
        }
    }
    publish.abort();
    for d in drains {
        d.abort();
    }
    server.shutdown().await?;
    Ok(())
}
