use clap::Parser;
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow_testkit::example::{CodecArg, init_tracing, mount_path, publisher_description};

use rtflow::webrtc::{WebRtcConfig, create_webrtc_handlers};
use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::{collections::HashSet, net::SocketAddr, sync::Arc, time::Duration};

/// WebRTC WHIP/WHEP server with browser-based publish and playback.
///
/// Provides WHIP (ingest) and WHEP (playback) endpoints over HTTP,
/// plus an embedded browser player using native WebRTC APIs.
///
/// Publish from browser:
///   Open http://127.0.0.1:3000/publisher
///
/// Play from browser:
///   Open http://127.0.0.1:3000/player
///
/// Publish via WHIP:
///   POST /whip/live/stream with an SDP offer.
///
/// Play via WHEP:
///   POST /whep/live/stream with an SDP offer.
#[derive(Debug, Parser)]
struct Args {
    /// HTTP listen address for WHIP/WHEP.
    #[arg(long, default_value = "0.0.0.0:3000")]
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

    // WHIP (ingest) and WHEP (playback) share the RTSP mount registry.
    let (whip, whep) = create_webrtc_handlers(WebRtcConfig::default(), rtsp_server.registry());
    for (path, handler) in [("/whip/*", whip), ("/whep/*", whep)] {
        for method in [Method::Post, Method::Delete, Method::Options] {
            http_server.add_route(method, path, handler.clone()).await;
        }
    }

    let page = b"<!doctype html><html><body><h1>WebRTC WHIP/WHEP</h1><p>WHEP playback: <code>POST /whep/live/stream</code> with an SDP offer.</p><p>WHIP publish: <code>POST /whip/live/stream</code> with an SDP offer.</p></body></html>";
    let page_handler: rtflow::http::Handler = Arc::new(move |_req, mut writer| {
        Box::pin(async move {
            writer
                .write_body(200, "text/html; charset=utf-8", page)
                .await?;
            Ok(writer)
        })
    });
    http_server
        .add_route(Method::Get, "/player", page_handler.clone())
        .await;
    http_server
        .add_route(Method::Get, "/publisher", page_handler)
        .await;

    http_server.run().await?;

    let rtsp_url = format!(
        "rtsp://127.0.0.1:{}{}",
        rtsp_server.local_addr().port(),
        args.mount
    );

    println!("=== WebRTC Server (WHIP/WHEP) ===");
    println!("HTTP listen:    0.0.0.0:{http_port}");
    println!("RTSP ingest:    {rtsp_url}");
    println!();
    println!(
        "WHEP playback:  POST http://127.0.0.1:{http_port}/whep{}",
        args.mount
    );
    println!(
        "WHIP publish:   POST http://127.0.0.1:{http_port}/whip{}",
        args.mount
    );
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
