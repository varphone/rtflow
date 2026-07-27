mod common;

use clap::Parser;
use common::{CodecArg, init_tracing, mount_path, publisher_description};
use rtflow::mount::MountRegistry;
use rtflow::rtmp::RtmpServer;
use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::{net::SocketAddr, sync::Arc, time::Duration};

/// RTMP ingest server with optional RTSP cross-protocol playback.
///
/// Accepts RTMP publish (FFmpeg/OBS) and serves both RTMP play
/// and RTSP play from the same shared mount.
///
/// Publish via FFmpeg:
///   ffmpeg -re -i INPUT -c:v libx264 -c:a aac -f flv rtmp://127.0.0.1:1935/live/stream
///
/// Play via FFplay (RTMP):
///   ffplay rtmp://127.0.0.1:1935/live/stream
///
/// Play via FFplay (RTSP cross-protocol):
///   ffplay rtsp://127.0.0.1:8554/live/stream
#[derive(Debug, Parser)]
struct Args {
    /// RTMP listen address.
    #[arg(long, default_value = "0.0.0.0:1935")]
    rtmp_listen: SocketAddr,
    /// RTSP listen address for cross-protocol playback.
    #[arg(long, default_value = "0.0.0.0:8554")]
    rtsp_listen: SocketAddr,
    /// Mount path for the stream.
    #[arg(long, default_value = "/live/stream")]
    mount: String,
    /// Expected codecs from the publisher.
    #[arg(long, value_delimiter = ',', default_values = ["h264", "aac"])]
    codecs: Vec<CodecArg>,
    /// Maximum concurrent RTMP connections.
    #[arg(long, default_value_t = 100)]
    max_connections: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    if args.codecs.is_empty() {
        return Err("at least one codec is required".into());
    }
    let _mount_path = mount_path(&args.mount)?;

    // Shared mount registry across RTMP and RTSP
    let mounts = Arc::new(MountRegistry::new(args.max_connections));

    // RTMP server (ingest + play)
    let rtmp_server = RtmpServer::bind(args.rtmp_listen, mounts.clone()).await?;
    let rtmp_addr = rtmp_server.local_addr()?;
    let rtmp_cancel = rtmp_server.cancellation_token();
    tokio::spawn(rtmp_server.run());

    // RTSP server (cross-protocol playback)
    let rtsp_server = RtspServer::bind(ServerConfig::new(args.rtsp_listen)).await?;
    let rtsp_addr = rtsp_server.local_addr();

    // Register a publisher mount on the RTSP server so RTSP clients can play
    let mount = rtsp_server
        .register_mount(
            mount_path(&args.mount)?,
            MountConfig::Publisher {
                description: publisher_description("rtmp-source", &args.codecs),
                allow_replace_after_disconnect: true,
            },
        )
        .await?;

    let rtmp_url = format!("rtmp://127.0.0.1:{}{}", rtmp_addr.port(), args.mount);
    let rtsp_url = format!("rtsp://127.0.0.1:{}{}", rtsp_addr.port(), args.mount);

    println!("=== RTMP Server ===");
    println!("RTMP listen:  {}", rtmp_addr);
    println!("RTSP listen:  {}", rtsp_addr);
    println!();
    println!("Publish via FFmpeg:");
    println!("  ffmpeg -re -i INPUT -c:v libx264 -preset ultrafast -tune zerolatency -c:a aac -f flv {rtmp_url}");
    println!();
    println!("Publish via OBS:");
    println!("  Settings -> Stream -> Service: Custom");
    println!("  Server: rtmp://127.0.0.1:{}", rtmp_addr.port());
    println!("  Stream Key: {}", args.mount.trim_start_matches('/'));
    println!();
    println!("Play via FFplay (RTMP):");
    println!("  ffplay {rtmp_url}");
    println!();
    println!("Play via FFplay (RTSP cross-protocol):");
    println!("  ffplay {rtsp_url}");
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
                    "rtmp server status"
                );
            }
        }
    }

    rtmp_cancel.cancel();
    rtsp_server.shutdown().await?;
    Ok(())
}
