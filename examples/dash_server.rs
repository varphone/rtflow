mod common;

use clap::Parser;
use common::{CodecArg, init_tracing, mount_path, publisher_description};
use rtflow::dash::{create_dash_handlers, DashConfig};
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow::{MountConfig, RtspServer, ServerConfig};
use std::{collections::HashSet, net::SocketAddr, sync::Arc, time::Duration};

/// Accept RTSP publish and serve MPEG-DASH playback (MPD + fMP4 segments).
///
/// Start the server, then publish via FFmpeg:
///   ffmpeg -re -i INPUT -c copy -f rtsp rtsp://127.0.0.1:8554/live/stream
///
/// Play in browser (dash.js):
///   Open http://127.0.0.1:8080/live/stream/player
///
/// Play via FFplay:
///   ffplay http://127.0.0.1:8080/live/stream/manifest.mpd
#[derive(Debug, Parser)]
struct Args {
    /// RTSP listen address.
    #[arg(long, default_value = "0.0.0.0:8554")]
    rtsp_listen: SocketAddr,
    /// HTTP listen address for DASH playback.
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

/// Embedded dash.js HTML player page.
fn dash_player_html(mpd_url: &str) -> String {
    let mut html = String::with_capacity(2048);
    html.push_str(r##"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>rtflow DASH Player</title>
<style>
  body { font-family: system-ui, sans-serif; max-width: 960px; margin: 40px auto; padding: 0 20px; background: #1a1a2e; color: #eee; }
  h1 { color: #0f3460; background: #e94560; display: inline-block; padding: 8px 16px; border-radius: 4px; }
  video { width: 100%; border-radius: 8px; background: #000; }
  .info { margin-top: 16px; padding: 12px; background: #16213e; border-radius: 4px; font-size: 14px; }
  .info code { color: #e94560; }
</style>
</head>
<body>
<h1>rtflow DASH Player</h1>
<video id="video" controls autoplay muted></video>
<div class="info">
  <p>MPD URL: <code>"##);
    html.push_str(mpd_url);
    html.push_str(r##"</code></p>
  <p id="status">Loading...</p>
</div>
<script src="https://cdn.dashjs.org/latest/dash.all.min.js"></script>
<script>
  var url = ""##);
    html.push_str(mpd_url);
    html.push_str(r##"";
  var player = dashjs.MediaPlayer().create();
  player.initialize(document.querySelector("#video"), url, true);
  player.on(dashjs.MediaPlayer.events.PLAYBACK_STARTED, function() {
    document.getElementById("status").textContent = "Playing";
  });
  player.on(dashjs.MediaPlayer.events.ERROR, function(e) {
    document.getElementById("status").textContent = "Error: " + JSON.stringify(e.error);
  });
</script>
</body>
</html>"##);
    html
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
                description: publisher_description("dash-source", &args.codecs),
                allow_replace_after_disconnect: args.allow_replace,
            },
        )
        .await?;

    // HTTP server (DASH playback)
    let mut http_server = HttpServer::bind(HttpConfig::new(args.http_listen)).await?;

    let dash_config = DashConfig {
        segment_duration: Duration::from_secs(args.segment_duration),
        segment_count: args.segment_count,
        ..Default::default()
    };
    let (mpd_handler, segment_handler, init_handler) = create_dash_handlers(dash_config, &mount);

    // Register DASH routes
    let mpd_path = format!("{}/manifest.mpd", args.mount);
    let init_path = format!("{}/init.mp4", args.mount);
    let segment_path = format!("{}/segment_{{seq}}.m4s", args.mount);
    let player_path = format!("{}/player", args.mount);

    http_server.add_route(Method::Get, &mpd_path, mpd_handler).await;
    http_server.add_route(Method::Get, &init_path, init_handler).await;
    http_server.add_route(Method::Get, &segment_path, segment_handler).await;

    // Embedded dash.js player page
    let http_port = http_server.local_addr().port();
    let mpd_url = format!("http://127.0.0.1:{http_port}{}/manifest.mpd", args.mount);
    let html = dash_player_html(&mpd_url);
    let player_handler: rtflow::http::Handler = Arc::new(move |_req, mut writer| {
        let html = html.clone();
        Box::pin(async move {
            writer.write_body(200, "text/html; charset=utf-8", html.as_bytes()).await?;
            Ok(writer)
        })
    });
    http_server.add_route(Method::Get, &player_path, player_handler).await;
    http_server.run().await?;

    let rtsp_url = format!(
        "rtsp://127.0.0.1:{}{}",
        rtsp_server.local_addr().port(),
        args.mount
    );

    println!("=== DASH Server ===");
    println!("RTSP ingest:    {rtsp_url}");
    println!("DASH manifest:  {mpd_url}");
    println!("Browser player: http://127.0.0.1:{http_port}{}/player", args.mount);
    println!("Segment window: {} x {}s", args.segment_count, args.segment_duration);
    println!();
    println!("Publish via FFmpeg:");
    println!("  ffmpeg -re -i INPUT -c copy -f rtsp {rtsp_url}");
    println!();
    println!("Play via FFplay:");
    println!("  ffplay {mpd_url}");
    println!();
    println!("Play in browser:");
    println!("  Open http://127.0.0.1:{http_port}{}/player", args.mount);
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
                    "dash server status"
                );
            }
        }
    }
    http_server.shutdown();
    rtsp_server.shutdown().await?;
    Ok(())
}
