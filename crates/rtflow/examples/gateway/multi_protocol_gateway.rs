use clap::Parser;
use rtflow::dash::{DashConfig, create_dash_router};
use rtflow::gateway::Gateway;
use rtflow::hls::{HlsConfig, create_hls_router};
use rtflow::http::auth_middleware::with_token_auth_bypass;
use rtflow::http::{HttpConfig, Method};
use rtflow::http_flv::create_flv_handler;
use rtflow::token_auth::{TokenConfig, generate_token};
use rtflow::webrtc::{WebRtcConfig, create_webrtc_router};
use rtflow::{MountConfig, ServerConfig};
use rtflow_testkit::example::{CodecArg, init_tracing, mount_path, publisher_description};
use std::{collections::HashSet, net::SocketAddr, sync::Arc, time::Duration};

/// Multi-protocol streaming gateway.
///
/// Runs RTSP, RTMP, SRT, HLS, DASH, HTTP-FLV, and WebRTC (WHIP/WHEP)
/// servers in a single process sharing one mount registry.
///
/// A stream published via any ingest protocol (RTSP/RTMP/SRT) is
/// immediately available for playback via all output protocols.
///
/// Quick start:
///   cargo run --example multi_protocol_gateway --all-features
///   ffmpeg -re -i INPUT -c copy -f flv rtmp://127.0.0.1:1935/live/stream
///   # Then play via any protocol listed in the startup output.
#[derive(Debug, Parser)]
struct Args {
    /// RTSP listen address.
    #[arg(long, default_value = "0.0.0.0:8554")]
    rtsp_listen: SocketAddr,
    /// RTMP listen address.
    #[arg(long, default_value = "0.0.0.0:1935")]
    rtmp_listen: SocketAddr,
    /// SRT listen address (UDP).
    #[arg(long, default_value = "0.0.0.0:9000")]
    srt_listen: SocketAddr,
    /// HTTP listen address (HLS/DASH/FLV/WebRTC).
    #[arg(long, default_value = "0.0.0.0:3000")]
    http_listen: SocketAddr,
    /// Mount path for the stream.
    #[arg(long, default_value = "/live/stream")]
    mount: String,
    /// Expected codecs from the publisher.
    #[arg(long, value_delimiter = ',', default_values = ["h264", "aac"])]
    codecs: Vec<CodecArg>,
    /// Public IP for WebRTC ICE candidates.
    #[arg(long, default_value = "127.0.0.1")]
    public_ip: String,
    /// HLS/DASH segment duration in seconds.
    #[arg(long, default_value_t = 2)]
    segment_duration: u64,
    /// HMAC secret for HTTP token authentication.
    /// When set, all HTTP playback endpoints require ?token=xxx.
    #[arg(long)]
    auth_secret: Option<String>,
    /// Token TTL in seconds (default: 3600 = 1 hour).
    #[arg(long, default_value_t = 3600)]
    auth_ttl: u64,
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
    let mount_path = mount_path(&args.mount)?;

    // Create gateway with shared mount registry
    let mut gateway = Gateway::new(100);

    // Start RTSP server
    gateway
        .start_rtsp(ServerConfig::new(args.rtsp_listen))
        .await?;
    let rtsp_addr = gateway.rtsp().unwrap().local_addr();

    // Start RTMP server
    let rtmp_addr = gateway.start_rtmp(args.rtmp_listen).await?;

    // Start SRT server
    let srt_addr = gateway.start_srt(args.srt_listen).await?;

    // Start HTTP server
    gateway
        .start_http(HttpConfig::new(args.http_listen))
        .await?;
    let http_server = gateway.http().unwrap();
    let http_addr = http_server.local_addr();

    // Register publisher mount (shared across all protocols)
    let rtsp_server = gateway.rtsp().unwrap();
    let mount = rtsp_server
        .register_mount(
            mount_path.clone(),
            MountConfig::Publisher {
                description: publisher_description("gateway", &args.codecs),
                allow_replace_after_disconnect: true,
            },
        )
        .await?;

    // --- HTTP routes: HLS ---
    let hls_config = HlsConfig {
        segment_duration: Duration::from_secs(args.segment_duration),
        segment_count: 5,
        live: true,
    };
    let handlers = create_hls_router(hls_config, &mount);
    let (hls_playlist, hls_segment, hls_init) =
        (handlers.playlist, handlers.segment, handlers.init);

    // Token auth configuration (if --auth-secret is provided)
    let token_config = args.auth_secret.as_ref().map(|secret| TokenConfig {
        secret: secret.as_bytes().to_vec(),
        ttl: Duration::from_secs(args.auth_ttl),
        param_name: "token".to_string(),
    });

    // Helper: optionally wrap a handler with token auth
    let bypass: Vec<String> = vec!["/health".into(), "/metrics".into(), "/stats".into()];
    macro_rules! maybe_auth {
        ($handler:expr) => {
            match &token_config {
                Some(cfg) => with_token_auth_bypass($handler, cfg.clone(), bypass.clone()),
                None => $handler,
            }
        };
    }

    http_server
        .add_route(
            Method::Get,
            &format!("{}.m3u8", args.mount),
            maybe_auth!(hls_playlist),
        )
        .await;
    http_server
        .add_route(
            Method::Get,
            &format!("{}/init.m4s", args.mount),
            maybe_auth!(hls_init),
        )
        .await;
    http_server
        .add_route(
            Method::Get,
            &format!("{}/{{seq}}", args.mount),
            maybe_auth!(hls_segment),
        )
        .await;

    // --- HTTP routes: DASH ---
    let dash_config = DashConfig {
        segment_duration: Duration::from_secs(args.segment_duration),
        segment_count: 5,
        ..Default::default()
    };
    let handlers = create_dash_router(dash_config, &mount);
    let (dash_mpd, dash_segment, dash_init) = (handlers.manifest, handlers.segment, handlers.init);
    http_server
        .add_route(
            Method::Get,
            &format!("{}/manifest.mpd", args.mount),
            maybe_auth!(dash_mpd),
        )
        .await;
    http_server
        .add_route(
            Method::Get,
            &format!("{}/init.mp4", args.mount),
            maybe_auth!(dash_init),
        )
        .await;
    http_server
        .add_route(
            Method::Get,
            &format!("{}/segment_{{seq}}.m4s", args.mount),
            maybe_auth!(dash_segment),
        )
        .await;

    // --- HTTP routes: HTTP-FLV ---
    let flv_handler = create_flv_handler(&mount);
    http_server
        .add_route(
            Method::Get,
            &format!("{}.flv", args.mount),
            maybe_auth!(flv_handler),
        )
        .await;

    // --- HTTP routes: WebRTC (WHIP/WHEP) ---
    let webrtc_config = WebRtcConfig {
        session_timeout: Duration::from_secs(60),
        ..Default::default()
    };
    let webrtc_mounts = Arc::new(rtflow::mount::MountRegistry::new(10));
    let handlers = create_webrtc_router(webrtc_config, webrtc_mounts);
    let (whip_handler, whep_handler) = (handlers.whip, handlers.whep);
    let whip_path = format!("/whip{}", args.mount);
    let whep_path = format!("/whep{}", args.mount);
    http_server
        .add_route(Method::Post, &whip_path, whip_handler.clone())
        .await;
    http_server
        .add_route(Method::Delete, &whip_path, whip_handler)
        .await;
    http_server
        .add_route(Method::Post, &whep_path, whep_handler)
        .await;

    // Start HTTP accept loop
    let http_server = gateway; // move gateway into http run
    // Note: we need to run the HTTP server in background
    // Actually, gateway.http() already started it. Let's just print info and wait.

    let http_port = http_addr.port();
    let rtsp_url = format!("rtsp://127.0.0.1:{}{}", rtsp_addr.port(), args.mount);
    let rtmp_url = format!("rtmp://127.0.0.1:{}{}", rtmp_addr.port(), args.mount);
    let srt_streamid = format!("#!::r={}", args.mount);

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║         rtflow Multi-Protocol Streaming Gateway         ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║                                                         ║");
    println!("║  INGEST (publish):                                      ║");
    println!("║    RTSP: {rtsp_url}");
    println!("║    RTMP: {rtmp_url}");
    println!(
        "║    SRT:  srt://127.0.0.1:{}?streamid={srt_streamid}",
        srt_addr.port()
    );
    println!("║                                                         ║");
    println!("║  PLAYBACK:                                              ║");
    println!("║    RTSP:     {rtsp_url}");
    println!("║    RTMP:     {rtmp_url}");
    if let Some(ref cfg) = token_config {
        let hls_path = format!("{}.m3u8", args.mount);
        let dash_path = format!("{}/manifest.mpd", args.mount);
        let flv_path = format!("{}.flv", args.mount);
        let hls_token = generate_token(cfg, &hls_path);
        let dash_token = generate_token(cfg, &dash_path);
        let flv_token = generate_token(cfg, &flv_path);
        println!("║    HLS:      http://127.0.0.1:{http_port}{hls_path}?token={hls_token}");
        println!("║    DASH:     http://127.0.0.1:{http_port}{dash_path}?token={dash_token}");
        println!("║    HTTP-FLV: http://127.0.0.1:{http_port}{flv_path}?token={flv_token}");
        println!(
            "║    WebRTC:   http://127.0.0.1:{http_port}/whep{}",
            args.mount
        );
        println!("║                                                         ║");
        println!("║  🔒 Token auth ENABLED (TTL: {}s)", args.auth_ttl);
    } else {
        println!(
            "║    HLS:      http://127.0.0.1:{http_port}{}.m3u8",
            args.mount
        );
        println!(
            "║    DASH:     http://127.0.0.1:{http_port}{}/manifest.mpd",
            args.mount
        );
        println!(
            "║    HTTP-FLV: http://127.0.0.1:{http_port}{}.flv",
            args.mount
        );
        println!(
            "║    WebRTC:   http://127.0.0.1:{http_port}/whep{}",
            args.mount
        );
    }
    println!("║                                                         ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();
    println!("Quick start with FFmpeg:");
    println!("  ffmpeg -re -f lavfi -i testsrc=size=1280x720:rate=25 \\");
    println!("         -f lavfi -i sine=frequency=1000:sample_rate=48000 \\");
    println!("         -c:v libx264 -preset ultrafast -tune zerolatency -g 50 \\");
    println!("         -c:a aac -b:a 128k \\");
    println!("         -f flv {rtmp_url}");
    println!();
    println!("Press Ctrl+C to stop.");

    let mut status = tokio::time::interval(Duration::from_secs(5));
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
                    "gateway status"
                );
            }
        }
    }

    http_server.shutdown().await?;
    Ok(())
}
