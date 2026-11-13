use clap::Parser;
use rtflow::mount::MountRegistry;
use rtflow::srt::SrtServer;
use rtflow::{MountConfig, RtspServer, ServerConfig};
use rtflow_testkit::example::{CodecArg, init_tracing, mount_path, publisher_description};
use std::{net::SocketAddr, sync::Arc, time::Duration};

/// SRT ingest server with RTSP cross-protocol playback.
///
/// Accepts SRT caller connections (FFmpeg/OBS) carrying MPEG-TS,
/// demuxes to AccessUnits, and makes the stream available via RTSP.
///
/// Publish via FFmpeg:
///   ffmpeg -re -i INPUT -c copy -f mpegts "srt://127.0.0.1:9000?streamid=#!::r=/live/stream"
///
/// Play via FFplay (RTSP):
///   ffplay rtsp://127.0.0.1:8554/live/stream
#[derive(Debug, Parser)]
struct Args {
    /// SRT listen address (UDP).
    #[arg(long, default_value = "0.0.0.0:9000")]
    srt_listen: SocketAddr,
    /// RTSP listen address for playback.
    #[arg(long, default_value = "0.0.0.0:8554")]
    rtsp_listen: SocketAddr,
    /// Mount path for the stream.
    #[arg(long, default_value = "/live/stream")]
    mount: String,
    /// Expected codecs from the publisher.
    #[arg(long, value_delimiter = ',', default_values = ["h264", "aac"])]
    codecs: Vec<CodecArg>,
    /// TSBPD delay in milliseconds.
    #[arg(long, default_value_t = 120)]
    tsbpd_delay_ms: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    if args.codecs.is_empty() {
        return Err("at least one codec is required".into());
    }
    let _mount_path = mount_path(&args.mount)?;

    // Shared mount registry
    let mounts = Arc::new(MountRegistry::new(10));

    // SRT server (ingest)
    let srt_config = rtflow::srt::SrtConfig {
        bind_addr: args.srt_listen,
        tsbpd_delay_ms: args.tsbpd_delay_ms,
        ..Default::default()
    };
    let srt_server = SrtServer::with_config(srt_config, mounts.clone()).await?;
    let srt_addr = srt_server.local_addr()?;
    let srt_cancel = srt_server.cancellation_token();
    tokio::spawn(srt_server.run());

    // RTSP server (playback)
    let rtsp_server = RtspServer::bind(ServerConfig::new(args.rtsp_listen)).await?;
    let rtsp_addr = rtsp_server.local_addr();

    // Register publisher mount
    let mount = rtsp_server
        .register_mount(
            mount_path(&args.mount)?,
            MountConfig::Publisher {
                description: publisher_description("srt-source", &args.codecs),
                allow_replace_after_disconnect: true,
            },
        )
        .await?;

    let srt_streamid = format!("#!::r={}", args.mount);
    let rtsp_url = format!("rtsp://127.0.0.1:{}{}", rtsp_addr.port(), args.mount);

    println!("=== SRT Server ===");
    println!("SRT listen:   {} (UDP)", srt_addr);
    println!("RTSP listen:  {}", rtsp_addr);
    println!("TSBPD delay:  {}ms", args.tsbpd_delay_ms);
    println!();
    println!("Publish via FFmpeg:");
    println!(
        "  ffmpeg -re -i INPUT -c:v libx264 -preset ultrafast -tune zerolatency -c:a aac -f mpegts \"srt://127.0.0.1:{}?streamid={srt_streamid}\"",
        srt_addr.port()
    );
    println!();
    println!("Publish via OBS:");
    println!("  Settings -> Stream -> Service: Custom");
    println!("  Server: srt://127.0.0.1:{}", srt_addr.port());
    println!("  Stream Key: {srt_streamid}");
    println!();
    println!("Play via FFplay (RTSP):");
    println!("  ffplay {rtsp_url}");
    println!();
    println!("Play via FFplay (low-latency UDP):");
    println!("  ffplay -rtsp_transport udp {rtsp_url}");
    println!();
    println!("Press Ctrl+C to stop.");

    let mut status = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                break;
            }
            _ = status.tick() => {
                let snapshot = mount.snapshot();
                tracing::info!(
                    publisher = snapshot.publisher_connected,
                    subscribers = snapshot.subscriber_count,
                    accepted = snapshot.stats.accepted_frames,
                    "srt server status"
                );
            }
        }
    }

    srt_cancel.cancel();
    rtsp_server.shutdown().await?;
    Ok(())
}
