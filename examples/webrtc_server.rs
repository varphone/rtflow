mod common;

use clap::Parser;
use common::{CodecArg, init_tracing, mount_path, publisher_description};
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow::webrtc::{create_webrtc_handlers, WebRtcConfig};
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
///   ffmpeg -re -i INPUT -c:v libx264 -c:a libopus -f whip http://127.0.0.1:8080/whip/live/stream
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
fn whep_player_html(whep_url: &str) -> String {
    let mut html = String::with_capacity(3072);
    html.push_str(r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>rtflow WebRTC Player</title>
<style>
  body { font-family: system-ui, sans-serif; max-width: 960px; margin: 40px auto; padding: 0 20px; background: #1a1a2e; color: #eee; }
  h1 { color: #0f3460; background: #e94560; display: inline-block; padding: 8px 16px; border-radius: 4px; }
  video { width: 100%; border-radius: 8px; background: #000; }
  .info { margin-top: 16px; padding: 12px; background: #16213e; border-radius: 4px; font-size: 14px; }
  .info code { color: #e94560; }
  #status { margin-top: 8px; padding: 8px; border-radius: 4px; }
  .connected { background: #1b4332; color: #95d5b2; }
  .error { background: #3d0000; color: #ff6b6b; }
  .waiting { background: #1a1a2e; color: #aaa; }
</style>
</head>
<body>
<h1>rtflow WebRTC Player (WHEP)</h1>
<video id="video" controls autoplay muted playsinline></video>
<div class="info">
  <p>WHEP endpoint: <code>"#);
    html.push_str(whep_url);
    html.push_str(r#"</code></p>
  <div id="status" class="waiting">Connecting...</div>
</div>
<script>
async function startPlayback() {
  const statusEl = document.getElementById('status');
  const video = document.getElementById('video');
  const pc = new RTCPeerConnection({ iceServers: [] });

  pc.addTransceiver('video', { direction: 'recvonly' });
  pc.addTransceiver('audio', { direction: 'recvonly' });

  pc.ontrack = (event) => {
    video.srcObject = event.streams[0];
    statusEl.textContent = 'Playing';
    statusEl.className = 'connected';
  };

  pc.oniceconnectionstatechange = () => {
    statusEl.textContent = 'ICE: ' + pc.iceConnectionState;
    if (pc.iceConnectionState === 'connected') statusEl.className = 'connected';
    if (pc.iceConnectionState === 'failed') statusEl.className = 'error';
  };

  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);

  try {
    const resp = await fetch('"#);
    html.push_str(whep_url);
    html.push_str(r#"', {
      method: 'POST',
      headers: { 'Content-Type': 'application/sdp' },
      body: offer.sdp,
    });
    if (!resp.ok) throw new Error('HTTP ' + resp.status);
    const answerSdp = await resp.text();
    await pc.setRemoteDescription({ type: 'answer', sdp: answerSdp });
    statusEl.textContent = 'Connected, waiting for media...';
  } catch (e) {
    statusEl.textContent = 'Error: ' + e.message;
    statusEl.className = 'error';
  }
}
startPlayback();
</script>
</body>
</html>"#);
    html
}

/// Embedded WHIP publisher HTML page.
fn whip_publisher_html(whip_url: &str) -> String {
    let mut html = String::with_capacity(3072);
    html.push_str(r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>rtflow WebRTC Publisher</title>
<style>
  body { font-family: system-ui, sans-serif; max-width: 960px; margin: 40px auto; padding: 0 20px; background: #1a1a2e; color: #eee; }
  h1 { color: #0f3460; background: #e94560; display: inline-block; padding: 8px 16px; border-radius: 4px; }
  video { width: 100%; border-radius: 8px; background: #000; }
  button { margin-top: 12px; padding: 10px 24px; font-size: 16px; cursor: pointer; border: none; border-radius: 4px; background: #e94560; color: #fff; }
  button:hover { background: #c73e54; }
  .info { margin-top: 16px; padding: 12px; background: #16213e; border-radius: 4px; font-size: 14px; }
  #status { margin-top: 8px; }
</style>
</head>
<body>
<h1>rtflow WebRTC Publisher (WHIP)</h1>
<video id="preview" autoplay muted playsinline></video>
<button id="startBtn" onclick="startPublish()">Start Publishing</button>
<div class="info">
  <p>WHIP endpoint: <code>"#);
    html.push_str(whip_url);
    html.push_str(r#"</code></p>
  <p id="status">Click "Start Publishing" to begin.</p>
</div>
<script>
let pc = null;
async function startPublish() {
  const statusEl = document.getElementById('status');
  const preview = document.getElementById('preview');
  document.getElementById('startBtn').disabled = true;

  try {
    const stream = await navigator.mediaDevices.getUserMedia({ video: true, audio: true });
    preview.srcObject = stream;

    pc = new RTCPeerConnection({ iceServers: [] });
    stream.getTracks().forEach(t => pc.addTrack(t, stream));

    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);

    const resp = await fetch('"#);
    html.push_str(whip_url);
    html.push_str(r#"', {
      method: 'POST',
      headers: { 'Content-Type': 'application/sdp' },
      body: offer.sdp,
    });
    if (!resp.ok) throw new Error('HTTP ' + resp.status);
    const answerSdp = await resp.text();
    await pc.setRemoteDescription({ type: 'answer', sdp: answerSdp });
    statusEl.textContent = 'Publishing...';
  } catch (e) {
    statusEl.textContent = 'Error: ' + e.message;
    document.getElementById('startBtn').disabled = false;
  }
}
</script>
</body>
</html>"#);
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

    // WebRTC handlers
    let config = WebRtcConfig {
        session_timeout: Duration::from_secs(args.session_timeout),
        ..Default::default()
    };
    let mounts = Arc::new(rtflow::mount::MountRegistry::new(10));
    let (whip_handler, whep_handler) = create_webrtc_handlers(config, mounts);

    // Register WHIP/WHEP routes
    let whip_path = format!("/whip{}", args.mount);
    let whep_path = format!("/whep{}", args.mount);
    http_server.add_route(Method::Post, &whip_path, whip_handler.clone()).await;
    http_server.add_route(Method::Delete, &whip_path, whip_handler).await;
    http_server.add_route(Method::Post, &whep_path, whep_handler).await;

    // Player pages
    let whep_url = format!("http://{}:{}/whep{}", args.public_ip, http_port, args.mount);
    let whip_url = format!("http://{}:{}/whip{}", args.public_ip, http_port, args.mount);

    let player_html = whep_player_html(&whep_url);
    let player_handler: rtflow::http::Handler = Arc::new(move |_req, mut writer| {
        let html = player_html.clone();
        Box::pin(async move {
            writer.write_body(200, "text/html; charset=utf-8", html.as_bytes()).await?;
            Ok(writer)
        })
    });
    http_server.add_route(Method::Get, "/player", player_handler).await;

    let publisher_html = whip_publisher_html(&whip_url);
    let publisher_handler: rtflow::http::Handler = Arc::new(move |_req, mut writer| {
        let html = publisher_html.clone();
        Box::pin(async move {
            writer.write_body(200, "text/html; charset=utf-8", html.as_bytes()).await?;
            Ok(writer)
        })
    });
    http_server.add_route(Method::Get, "/publisher", publisher_handler).await;

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
    println!("Browser playback (WHEP):");
    println!("  Open http://{}:{}/player", args.public_ip, http_port);
    println!();
    println!("Browser publish (WHIP):");
    println!("  Open http://{}:{}/publisher", args.public_ip, http_port);
    println!();
    println!("Publish via RTSP (for WHEP playback):");
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
