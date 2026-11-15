use clap::Parser;
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow_testkit::example::{CodecArg, init_tracing, mount_path, publisher_description};

use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::{collections::HashSet, net::SocketAddr, sync::Arc, time::Duration};

/// WebRTC WHIP/WHEP server with browser-based publish and playback.
///
/// Provides WHIP (ingest) and WHEP (playback) endpoints over HTTP,
/// plus an embedded browser player using native WebRTC APIs.
///
/// Publish from browser:
///   Open http://127.0.0.1:8080/publisher
///
/// Play from browser:
///   Open http://127.0.0.1:8080/player
///
/// Publish via FFmpeg (experimental WHIP support):
///   NOTE: WHIP/WHEP is a placeholder (no ICE/DTLS/SRTP media pipeline);
///   routes answer 501 and the pages explain the limitation.
#[derive(Debug, Parser)]
struct Args {
    /// HTTP listen address for WHIP/WHEP.
    #[arg(long, default_value = "0.0.0.0:8080")]
    http_listen: SocketAddr,
    /// RTSP listen address (optional ingest source).
    #[arg(long, default_value = "0.0.0.0:8554")]
    rtsp_listen: SocketAddr,
    /// Mount path for the stream.
    #[arg(long, default_value = "/live/stream")]
    mount: String,
    /// Expected codecs.
    #[arg(long, value_delimiter = ',', default_values = ["h264"])]
    codecs: Vec<CodecArg>,
    /// Public IP address for ICE candidates.
    #[arg(long, default_value = "127.0.0.1")]
    public_ip: String,
    /// Session timeout in seconds.
    #[arg(long, default_value_t = 60)]
    session_timeout: u64,
}

/// Embedded WHEP player HTML page.
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
    let _mount_path = mount_path(&args.mount)?;

    // RTSP server (optional ingest source)
    let rtsp_server = RtspServer::bind(ServerConfig::new(args.rtsp_listen)).await?;
    let mount = rtsp_server
        .register_mount(
            mount_path(&args.mount)?,
            MountConfig::Publisher {
                description: publisher_description("webrtc-source", &args.codecs),
                allow_replace_after_disconnect: true,
            },
        )
        .await?;

    // HTTP server (WHIP/WHEP + player pages)
    let mut http_server = HttpServer::bind(HttpConfig::new(args.http_listen)).await?;
    let http_port = http_server.local_addr().port();

    // WHIP/WHEP are NOT implemented (no ICE/DTLS/SRTP media pipeline):
    // per project policy these routes must not lure browsers into a
    // signaling exchange that yields no media. Routes answer 501 and the
    // player/publisher pages explain the limitation.
    let unavailable: rtflow::http::Handler = Arc::new(|_req, mut writer| {
        Box::pin(async move {
            writer
                .write_body(
                    501,
                    "text/plain",
                    b"WHIP/WHEP is not implemented (no ICE/DTLS/SRTP media pipeline)",
                )
                .await?;
            Ok(writer)
        })
    });
    let whip_path = format!("/whip{}", args.mount);
    let whep_path = format!("/whep{}", args.mount);
    for path in [whip_path.as_str(), whep_path.as_str()] {
        http_server
            .add_route(Method::Post, path, unavailable.clone())
            .await;
        http_server
            .add_route(Method::Delete, path, unavailable.clone())
            .await;
        http_server
            .add_route(Method::Get, path, unavailable.clone())
            .await;
    }

    let notice = b"<!doctype html><html><body><h1>WebRTC unavailable</h1><p>WHIP/WHEP is an experimental placeholder in this build: the ICE/DTLS/SRTP media pipeline is not implemented, so no media would flow. Use the RTSP endpoint instead.</p></body></html>";
    let notice_handler: rtflow::http::Handler = Arc::new(move |_req, mut writer| {
        Box::pin(async move {
            writer
                .write_body(200, "text/html; charset=utf-8", notice)
                .await?;
            Ok(writer)
        })
    });
    http_server
        .add_route(Method::Get, "/player", notice_handler.clone())
        .await;
    http_server
        .add_route(Method::Get, "/publisher", notice_handler)
        .await;

    http_server.run().await?;

    let rtsp_url = format!(
        "rtsp://127.0.0.1:{}{}",
        rtsp_server.local_addr().port(),
        args.mount
    );

    println!("=== WebRTC Server (WHIP/WHEP placeholder) ===");
    println!("HTTP listen:    0.0.0.0:{http_port}");
    println!("RTSP ingest:    {rtsp_url}");
    println!();
    println!("WHIP/WHEP: NOT IMPLEMENTED (no ICE/DTLS/SRTP media pipeline);");
    println!("  routes answer 501, /player and /publisher show a notice.");
    println!();
    println!("Use the RTSP endpoint for real media:");
    println!("  ffmpeg -re -i INPUT -c copy -f rtsp {rtsp_url}");
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
                    "webrtc server status"
                );
            }
        }
    }
    http_server.shutdown();
    rtsp_server.shutdown().await?;
    Ok(())
}
