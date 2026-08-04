//! IPC camera reference: publish main + sub streams (and optional audio)
//! from in-process encoded frames — the exact shape a HiSilicon HiMPP
//! integration takes.
//!
//! Replace the synthetic frame generator below with your VENC callback:
//!   - `venc_data`        → `AccessUnit::video(track, codec, pts, rtp_ts, is_idr, data)`
//!   - `venc_pts` (µs)    → `clock.timestamp(Duration::from_micros(pts))`
//!   - `venc_frame_type`  → `is_keyframe` (IDR frame)
//!
//! Parameter sets (SPS/PPS) are emitted **separated** like most HiMPP
//! VENC configurations: the first IDR carries them, later IDRs do not.
//! The framework extracts them once and fills `a=fmtp` automatically
//! (`sprop-parameter-sets=...`), so players can decode from the start.
//!
//! Power hint: when `subscriber_count == 0` the example stops pushing —
//! IPC firmware typically gates VENC on demand the same way.
//!
//! Usage:
//!   cargo run --example ipc_camera -- --audio aac
//!   vlc rtsp://127.0.0.1:8554/cam/main   (and /cam/sub)
use clap::{Parser, ValueEnum};
use rtflow::media::bitstream::{h264_parameter_sets, is_idr};
use rtflow::media::clock::RtpClock;
use rtflow::media::{AccessUnit, Codec, MediaDescription, TrackDescription};
use rtflow::server::ServerConfig;
use rtflow::{MountConfig, RtspServer};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum AudioMode {
    None,
    Aac,
    Pcma,
    Pcmu,
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value = "0.0.0.0:8554")]
    listen: SocketAddr,
    /// Base mount path; main/sub streams are exposed at {mount}/main and
    /// {mount}/sub.
    #[arg(long, default_value = "/cam")]
    mount: String,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    #[arg(long, default_value_t = 60)]
    gop: u32,
    #[arg(long, default_value_t = 1920)]
    main_width: u32,
    #[arg(long, default_value_t = 1080)]
    main_height: u32,
    #[arg(long, default_value_t = 1280)]
    sub_width: u32,
    #[arg(long, default_value_t = 720)]
    sub_height: u32,
    #[arg(long, value_enum, default_value_t = AudioMode::Aac)]
    audio: AudioMode,
    /// Per-session consumer queue capacity (frames). Memory budget per
    /// session ≈ capacity × average frame size; e.g. 1080p@4Mbps ≈ 16KB/
    /// frame → 128 frames ≈ 2MB. Embedded IPC: 64–128; desktop: 256.
    #[arg(long, default_value_t = 128)]
    queue: usize,}

/// Minimal synthetic H.264 access unit. The first IDR includes SPS/PPS
/// (NAL 7/8); later IDRs are slice-only, like separated VENC output.
fn synthetic_h264_frame(index: u64, gop: u32) -> (Vec<u8>, bool) {
    let is_idr = index % u64::from(gop) == 0;
    let mut data = Vec::new();
    if is_idr {
        if index == 0 {
            // SPS/PPS only on the very first IDR — the framework extracts
            // them into the SDP fmtp automatically.
            data.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1f, 0x8c, 0x8d, 0x40]);
            data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80]);
        }
        data.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]);
    } else {
        data.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9a]);
    }
    (data, is_idr)
}

/// Synthetic AAC frame (1024 samples at the stream sample rate).
fn synthetic_aac_frame(_index: u64) -> Vec<u8> {
    vec![0xff, 0xf1, 0x50, 0x80, 0x00, 0x1f, 0xfc, 0x00] // ADTS header + payload
}

#[derive(Clone, Copy)]
struct StreamConfig {
    name: &'static str,
    width: u32,
    height: u32,
    audio: Option<AudioMode>,
}

async fn publish_stream(
    mount: &rtflow::server::MountHandle,
    config: &StreamConfig,
    fps: u32,
    gop: u32,
    stop: Arc<AtomicBool>,
) -> rtflow::Result<()> {
    let publisher = mount.publisher()?;
    tracing::info!(url = format!("rtsp://{}{}", mount.path, config.name), "stream ready");
    let mut video_clock = RtpClock::new(90_000);
    let mut index: u64 = 0;
    let frame_interval = Duration::from_micros(1_000_000 / u64::from(fps));
    let mut next_frame = tokio::time::Instant::now();

    // Push the very first IDR (with SPS/PPS) unconditionally: the
    // framework extracts the parameter sets into the SDP fmtp, so any
    // client's DESCRIBE — which happens before its PLAY — already sees
    // sprop-parameter-sets. Real firmware does the same by keeping VENC
    // running from power-on.
    {
        let (data, is_idr) = synthetic_h264_frame(0, gop);
        let _ = publisher.try_push(AccessUnit::video(
            0,
            Codec::H264,
            Duration::ZERO,
            video_clock.timestamp(Duration::ZERO),
            is_idr,
            data.into(),
        ));
        index += 1;
    }

    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep_until(next_frame).await;
        next_frame += frame_interval;

        // Power hint: no subscribers → skip pushing (gate VENC in real HW).
        if mount.snapshot().subscriber_count == 0 {
            continue;
        }

        let pts = Duration::from_micros(index * 1_000_000 / u64::from(fps));
        let (data, is_idr) = synthetic_h264_frame(index, gop);
        let _ = publisher.try_push(AccessUnit::video(
            0,
            Codec::H264,
            pts,
            video_clock.timestamp(pts),
            is_idr,
            data.into(),
        ));

        if let Some(mode) = config.audio {
            // 20 ms blocks: AAC (1024 samples) or G.711 (160 samples) at
            // their respective rates — one block per video frame.
            let (clock_rate, codec) = match mode {
                AudioMode::Aac => (48_000u32, Codec::Aac),
                AudioMode::Pcma => (8_000, Codec::Pcma),
                AudioMode::Pcmu => (8_000, Codec::Pcmu),
                AudioMode::None => unreachable!(),
            };
            let mut audio_clock = RtpClock::new(clock_rate);
            let audio_pts = Duration::from_micros(index * 20_000);
            let _ = publisher.try_push(AccessUnit::audio(
                1,
                codec,
                audio_pts,
                audio_clock.timestamp(audio_pts),
                synthetic_aac_frame(index).into(),
            ));
        }
        index += 1;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> rtflow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .try_init()
        .ok();
    let args = Args::parse();
    let server = RtspServer::bind(
        ServerConfig::builder(args.listen)
            .consumer_queue_capacity(args.queue)
            .build()?,
    )
    .await?;

    // Compile-time check of the bitstream helpers used by the example.
    let _ = h264_parameter_sets;
    let _ = is_idr;

    let stream_configs = [
        StreamConfig {
            name: "main",
            width: args.main_width,
            height: args.main_height,
            audio: Some(args.audio),
        },
        StreamConfig {
            name: "sub",
            width: args.sub_width,
            height: args.sub_height,
            audio: None,
        },
    ];
    let mut mounts = Vec::new();
    for config in &stream_configs {
        let mut tracks = vec![TrackDescription::h264_video(
            0,
            &[],
            &[],
            config.width,
            config.height,
            args.fps,
        )];
        if let Some(mode) = config.audio {
            let track = match mode {
                AudioMode::Aac => TrackDescription::aac_audio(1, 48_000, 2),
                AudioMode::Pcma => TrackDescription::pcma_audio(1, 8_000, 1),
                AudioMode::Pcmu => TrackDescription::pcmu_audio(1, 8_000, 1),
                AudioMode::None => unreachable!(),
            };
            tracks.push(track);
        }
        let path = format!("{}/{}", args.mount, config.name);
        let mount = server
            .register_mount(
                path.as_str(),
                MountConfig::Publisher {
                    description: MediaDescription::from_tracks("ipc", tracks),
                    allow_replace_after_disconnect: true,
                },
            )
            .await?;
        tracing::info!(url = format!("rtsp://{}{}", server.local_addr(), path), "stream ready");
        mounts.push(mount);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for (mount, config) in mounts.iter().zip(stream_configs.iter()) {
        let stop = stop.clone();
        let mount = mount.clone();
        let config = config.clone();
        handles.push(tokio::spawn(async move {
            publish_stream(&mount, &config, args.fps, args.gop, stop).await
        }));
    }

    tokio::signal::ctrl_c().await?;
    stop.store(true, Ordering::Relaxed);
    for handle in handles {
        let _ = handle.await;
    }
    server.shutdown().await?;
    Ok(())
}
