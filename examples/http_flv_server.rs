mod common;

use clap::Parser;
use common::{CodecArg, init_tracing, mount_path, publisher_description};
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow::http_flv::create_flv_handler;
use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::{collections::HashSet, net::SocketAddr, time::Duration};

/// Accept RTSP publish and serve HTTP-FLV playback.
///
/// Start the server, then publish via FFmpeg:
///   ffmpeg -re -i INPUT -c copy -f rtsp rtsp://127.0.0.1:8554/live/stream
///
/// Play via FFplay:
///   ffplay http://127.0.0.1:8080/live/stream.flv
#[derive(Debug, Parser)]
struct Args {
    /// RTSP listen address.
    #[arg(long, default_value = "0.0.0.0:8554")]
    rtsp_listen: SocketAddr,
    /// HTTP listen address for FLV playback.
    #[arg(long, default_value = "0.0.0.0:8080")]
    http_listen: SocketAddr,
    /// Mount path for the stream.
    #[arg(long, default_value = "/live/stream")]
    mount: String,
    /// Expected codecs from the publisher.
    #[arg(long, value_delimiter = ',', default_values = ["h264", "aac"])]
    codecs: Vec<CodecArg>,
    /// Allow publisher reconnection after disconnect.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    allow_replace: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let args = Args::parse();
    if args.codecs.is_empty() {
        return Err("at least one codec is required".into());
    }
    if args.codecs.iter().copied().collect::<HashSet<_>>().len() != args.codecs.len() {
        return Err("--codecs must not contain duplicates".into());
    }
    let mount_path = mount_path(&args.mount)?;

    // RTSP server (publisher ingest)
    let rtsp_server = RtspServer::bind(ServerConfig::new(args.rtsp_listen)).await?;
    let mount = rtsp_server
        .register_mount(
            mount_path.clone(),
            MountConfig::Publisher {
                description: publisher_description("http-flv-source", &args.codecs),
                allow_replace_after_disconnect: args.allow_replace,
            },
        )
        .await?;

    // HTTP server (FLV playback)
    let mut http_server = HttpServer::bind(HttpConfig::new(args.http_listen)).await?;
    let flv_path = format!("{}.flv", args.mount);
    let handler = create_flv_handler(&mount);
    http_server.add_route(Method::Get, &flv_path, handler).await;
    http_server.run().await?;

    let rtsp_url = format!(
        "rtsp://127.0.0.1:{}{}",
        rtsp_server.local_addr().port(),
        args.mount
    );
    let http_url = format!(
        "http://127.0.0.1:{}{}.flv",
        http_server.local_addr().port(),
        args.mount
    );

    println!("=== HTTP-FLV Server ===");
    println!("RTSP ingest:  {rtsp_url}");
    println!("FLV playback: {http_url}");
    println!();
    println!("Publish via FFmpeg:");
    println!("  ffmpeg -re -i INPUT -c copy -f rtsp {rtsp_url}");
    println!();
    println!("Play via FFplay:");
    println!("  ffplay {http_url}");
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
                    "http-flv server status"
                );
            }
        }
    }
    http_server.shutdown();
    rtsp_server.shutdown().await?;
    Ok(())
}
