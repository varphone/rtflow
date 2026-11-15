use clap::Parser;
use rtflow::hls::{HlsConfig, create_hls_router};
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow::{MountConfig, RtspServer, ServerConfig};
use rtflow_testkit::example::{CodecArg, init_tracing, mount_path, publisher_description};
use std::{collections::HashSet, net::SocketAddr, time::Duration};

/// Accept RTSP publish and serve HLS playback (M3U8 + fMP4 segments).
///
/// Start the server, then publish via FFmpeg:
///   ffmpeg -re -i INPUT -c copy -f rtsp rtsp://127.0.0.1:8554/live/stream
///
/// Play via FFplay or any HLS player:
///   ffplay http://127.0.0.1:8080/live/stream.m3u8
#[derive(Debug, Parser)]
struct Args {
    /// RTSP listen address.
    #[arg(long, default_value = "0.0.0.0:8554")]
    rtsp_listen: SocketAddr,
    /// HTTP listen address for HLS playback.
    #[arg(long, default_value = "0.0.0.0:8080")]
    http_listen: SocketAddr,
    /// Mount path for the stream.
    #[arg(long, default_value = "/live/stream")]
    mount: String,
    /// Expected codecs from the publisher.
    #[arg(long, value_delimiter = ',', default_values = ["h264", "aac"])]
    codecs: Vec<CodecArg>,
    /// Target segment duration in seconds.
    #[arg(long, default_value_t = 2)]
    segment_duration: u64,
    /// Number of segments to keep in the live window.
    #[arg(long, default_value_t = 5)]
    segment_count: usize,
    /// Allow publisher reconnection after disconnect.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    allow_replace: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "logging")]
    rtflow::logging::init();
    init_tracing();
    let args = Args::parse();
    if args.codecs.is_empty() {
        return Err("at least one codec is required".into());
    }
    if args.codecs.iter().copied().collect::<HashSet<_>>().len() != args.codecs.len() {
        return Err("--codecs must not contain duplicates".into());
    }
    if args.segment_duration == 0 || args.segment_duration > 30 {
        return Err("--segment-duration must be in 1..=30".into());
    }
    if args.segment_count == 0 || args.segment_count > 20 {
        return Err("--segment-count must be in 1..=20".into());
    }
    let mount_path = mount_path(&args.mount)?;

    // RTSP server (publisher ingest)
    let rtsp_server = RtspServer::bind(ServerConfig::new(args.rtsp_listen)).await?;
    let mount = rtsp_server
        .register_mount(
            mount_path.clone(),
            MountConfig::Publisher {
                description: publisher_description("hls-source", &args.codecs),
                allow_replace_after_disconnect: args.allow_replace,
            },
        )
        .await?;

    // HTTP server (HLS playback)
    let mut http_server = HttpServer::bind(HttpConfig::new(args.http_listen)).await?;

    let hls_config = HlsConfig {
        segment_duration: Duration::from_secs(args.segment_duration),
        segment_count: args.segment_count,
        live: true,
    };
    let handlers = create_hls_router(hls_config, &mount);
    let (playlist_handler, segment_handler, init_handler) =
        (handlers.playlist, handlers.segment, handlers.init);

    // Register HLS routes
    let playlist_path = format!("{}.m3u8", args.mount);
    let segment_path = format!("{}/{{seq}}", args.mount);
    let init_path = format!("{}/init.m4s", args.mount);

    http_server
        .add_route(Method::Get, &playlist_path, playlist_handler)
        .await;
    http_server
        .add_route(Method::Get, &init_path, init_handler)
        .await;
    http_server
        .add_route(Method::Get, &segment_path, segment_handler)
        .await;
    http_server.run().await?;

    let rtsp_url = format!(
        "rtsp://127.0.0.1:{}{}",
        rtsp_server.local_addr().port(),
        args.mount
    );
    let hls_url = format!(
        "http://127.0.0.1:{}{}.m3u8",
        http_server.local_addr().port(),
        args.mount
    );

    println!("=== HLS Server ===");
    println!("RTSP ingest: {rtsp_url}");
    println!("HLS playback: {hls_url}");
    println!("Segment duration: {}s", args.segment_duration);
    println!("Segment window: {}", args.segment_count);
    println!();
    println!("Publish via FFmpeg:");
    println!("  ffmpeg -re -i INPUT -c copy -f rtsp {rtsp_url}");
    println!();
    println!("Play via FFplay:");
    println!("  ffplay {hls_url}");
    println!();
    println!("Play in browser (any HLS.js player):");
    println!("  Open {hls_url} in an HLS-compatible player");
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
                    "hls server status"
                );
            }
        }
    }
    http_server.shutdown();
    rtsp_server.shutdown().await?;
    Ok(())
}
