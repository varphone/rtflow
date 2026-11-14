use clap::Parser;
use rtflow::dash::{DashConfig, create_dash_router};
use rtflow::http::{HttpConfig, HttpServer, Method};
use rtflow::{MountConfig, RtspServer, ServerConfig};
use rtflow_testkit::example::{CodecArg, init_tracing, mount_path, publisher_description};
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
    let mut html = String::with_capacity(4096);
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
  #log { margin-top: 12px; padding: 12px; background: #0d1b2a; border-radius: 4px; font-family: monospace; font-size: 12px; white-space: pre-wrap; max-height: 300px; overflow-y: auto; }
  button { margin: 8px 4px; padding: 8px 16px; background: #e94560; color: #fff; border: none; border-radius: 4px; cursor: pointer; }
</style>
</head>
<body>
<h1>rtflow DASH Player</h1>
<video id="video" controls autoplay muted></video>
<div class="info">
  <p>MPD URL: <code>"##);
    html.push_str(mpd_url);
    html.push_str(
        r##"</code></p>
  <p id="status">Loading...</p>
  <button onclick="testNativeMSE()">Test Native MSE (bypass dash.js)</button>
  <button onclick="testReference()">Test Reference (ffmpeg known-good)</button>
</div>
<pre id="log"></pre>
<script>
"##,
    );
    // Embed ffmpeg reference data for diagnostic testing
    html.push_str(include_str!("../assets/ref_data.js"));
    html.push_str(
        r##"
function log(msg) {
  var el = document.getElementById('log');
  el.textContent += msg + '\n';
  el.scrollTop = el.scrollHeight;
  console.log(msg);
}
</script>
<script src="https://cdn.dashjs.org/latest/dash.all.min.js"></script>
<script>
  var url = ""##,
    );
    html.push_str(mpd_url);
    html.push_str(r##"";
  var player = null;
  var retryTimer = null;
  var retryCount = 0;
  var MAX_RETRY = 30;

  function initPlayer() {
    if (player) { try { player.destroy(); } catch(e){} }
    player = dashjs.MediaPlayer().create();
    player.updateSettings({
      streaming: {
        delay: { liveDelay: 4 },
        buffer: { bufferTimeAtTopQuality: 8, bufferTimeAtTopQualityLongForm: 12 },
        liveCatchup: { enabled: true }
      }
    });
    player.initialize(document.querySelector("#video"), url, true);

    player.on(dashjs.MediaPlayer.events.PLAYBACK_STARTED, function() {
      document.getElementById("status").textContent = "Playing (dash.js)";
      log('[dash.js] PLAYBACK_STARTED');
      retryCount = 0;
    });
    player.on(dashjs.MediaPlayer.events.ERROR, function(e) {
      log('[dash.js] ERROR: ' + JSON.stringify(e.error));
      scheduleRetry();
    });
    player.on(dashjs.MediaPlayer.events.PLAYBACK_ENDED, function() {
      log('[dash.js] PLAYBACK_ENDED - stream ended');
      scheduleRetry();
    });

    // Buffer stall detection
    var video = document.querySelector("#video");
    var stallTimer = setInterval(function() {
      if (!player || !video || video.paused || video.ended) return;
      if (video.readyState < 3 && !video.seeking) {
        var buf = video.buffered;
        var ct = video.currentTime;
        var hasAhead = false;
        for (var i = 0; i < buf.length; i++) {
          if (buf.end(i) > ct + 0.5) { hasAhead = true; break; }
        }
        if (hasAhead) {
          log('[stall] Buffer ahead but stuck, seeking forward');
          video.currentTime = ct + 0.1;
        }
      }
    }, 3000);
  }

  function scheduleRetry() {
    if (retryTimer) return;
    if (retryCount >= MAX_RETRY) {
      document.getElementById("status").textContent = "Failed after " + MAX_RETRY + " retries";
      return;
    }
    retryCount++;
    var delay = Math.min(2000 * retryCount, 10000);
    document.getElementById("status").textContent = "Reconnecting in " + (delay/1000) + "s... (retry " + retryCount + ")";
    log('[retry] Scheduling reconnect #' + retryCount + ' in ' + delay + 'ms');
    retryTimer = setTimeout(function() {
      retryTimer = null;
      log('[retry] Attempting reconnect...');
      initPlayer();
    }, delay);
  }

  initPlayer();

  // Native MSE test (bypass dash.js)
  async function testNativeMSE() {
    log('=== Native MSE Test ===');
    var video2 = document.createElement('video');
    video2.controls = true;
    video2.muted = true;
    video2.style.width = '100%';
    video2.style.background = '#000';
    document.body.insertBefore(video2, document.getElementById('log'));

    var codecs = ['avc1.42C00A', 'avc1.42000a', 'avc1.640028'];
    var mime = '';
    for (var i = 0; i < codecs.length; i++) {
      var m = 'video/mp4; codecs="' + codecs[i] + '"';
      log('isTypeSupported("' + m + '") = ' + MediaSource.isTypeSupported(m));
      if (MediaSource.isTypeSupported(m) && !mime) mime = m;
    }
    if (!mime) { log('ERROR: No supported MIME type!'); return; }
    log('Using MIME: ' + mime);

    var ms = new MediaSource();
    video2.src = URL.createObjectURL(ms);
    ms.addEventListener('sourceopen', async function() {
      log('MediaSource opened, readyState=' + ms.readyState);
      try {
        var sb = ms.addSourceBuffer(mime);
        log('SourceBuffer added, mode=' + sb.mode);

        // Fetch init
        var initResp = await fetch(url.replace('manifest.mpd', 'init.mp4'));
        var initData = await initResp.arrayBuffer();
        log('Init segment: ' + initData.byteLength + ' bytes');
        sb.appendBuffer(initData);
        await new Promise(function(res, rej) {
          sb.addEventListener('updateend', res, {once:true});
          sb.addEventListener('error', function(e) { rej(new Error('init append error')); }, {once:true});
        });
        log('Init appended OK');

        // Get MPD to find segment number
        var mpdResp = await fetch(url);
        var mpdText = await mpdResp.text();
        var m = mpdText.match(/startNumber="(\d+)"/);
        var segNum = m ? parseInt(m[1]) : 0;
        log('Fetching segment_' + segNum + '.m4s');

        var segResp = await fetch(url.replace('manifest.mpd', 'segment_' + segNum + '.m4s'));
        if (!segResp.ok) { log('Segment fetch failed: ' + segResp.status); return; }
        var segData = await segResp.arrayBuffer();
        log('Segment: ' + segData.byteLength + ' bytes');

        sb.appendBuffer(segData);
        await new Promise(function(res, rej) {
          sb.addEventListener('updateend', res, {once:true});
          sb.addEventListener('error', function(e) { rej(new Error('seg append error')); }, {once:true});
        });
        log('Segment appended OK');
        log('Buffered: ' + (sb.buffered.length > 0 ? sb.buffered.start(0).toFixed(2) + '-' + sb.buffered.end(0).toFixed(2) : 'empty'));
        video2.play().then(function() { log('Native MSE: Playing!'); }).catch(function(e) { log('Play error: ' + e); });
      } catch(e) {
        log('ERROR: ' + e.message);
      }
    });
    video2.addEventListener('error', function() {
      log('Video2 error: code=' + video2.error.code + ' msg=' + video2.error.message);
    });
  }

  // Test with ffmpeg reference data (known-good init+segment)
  async function testReference() {
    log('=== Reference (ffmpeg) MSE Test ===');
    var video3 = document.createElement('video');
    video3.controls = true;
    video3.muted = true;
    video3.style.width = '100%';
    video3.style.background = '#000';
    document.body.insertBefore(video3, document.getElementById('log'));

    var mime = 'video/mp4; codecs="avc1.42C00A"';
    if (!MediaSource.isTypeSupported(mime)) { log('MIME not supported!'); return; }

    function b64ToBuf(b64) {
      var raw = atob(b64);
      var arr = new Uint8Array(raw.length);
      for (var i = 0; i < raw.length; i++) arr[i] = raw.charCodeAt(i);
      return arr.buffer;
    }

    var ms = new MediaSource();
    video3.src = URL.createObjectURL(ms);
    ms.addEventListener('sourceopen', async function() {
      log('[REF] MediaSource opened');
      try {
        var sb = ms.addSourceBuffer(mime);
        log('[REF] SourceBuffer added');
        var initBuf = b64ToBuf(REF_INIT_B64);
        log('[REF] Appending ffmpeg init: ' + initBuf.byteLength + ' bytes');
        sb.appendBuffer(initBuf);
        await new Promise(function(res, rej) {
          sb.addEventListener('updateend', res, {once:true});
          sb.addEventListener('error', function() { rej(new Error('ref init append error')); }, {once:true});
        });
        log('[REF] Init appended OK!');
        var segBuf = b64ToBuf(REF_SEG_B64);
        log('[REF] Appending ffmpeg segment: ' + segBuf.byteLength + ' bytes');
        sb.appendBuffer(segBuf);
        await new Promise(function(res, rej) {
          sb.addEventListener('updateend', res, {once:true});
          sb.addEventListener('error', function() { rej(new Error('ref seg append error')); }, {once:true});
        });
        log('[REF] Segment appended OK!');
        log('[REF] Buffered: ' + (sb.buffered.length > 0 ? sb.buffered.start(0).toFixed(2) + '-' + sb.buffered.end(0).toFixed(2) : 'empty'));
        video3.play().then(function() { log('[REF] Playing!'); }).catch(function(e) { log('[REF] Play error: ' + e); });
      } catch(e) {
        log('[REF] ERROR: ' + e.message);
      }
    });
    video3.addEventListener('error', function() {
      log('[REF] Video error: code=' + video3.error.code + ' msg=' + video3.error.message);
    });
  }
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
    let server_config = ServerConfig::builder(args.rtsp_listen)
        .session_timeout(std::time::Duration::from_secs(3600))
        .build()
        .map_err(|e| format!("server config: {e}"))?;
    let rtsp_server = RtspServer::bind(server_config).await?;
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
    let handlers = create_dash_router(dash_config, &mount);
    let (mpd_handler, segment_handler, init_handler) =
        (handlers.manifest, handlers.segment, handlers.init);

    // Register DASH routes
    let mpd_path = format!("{}/manifest.mpd", args.mount);
    let init_path = format!("{}/init.mp4", args.mount);
    let segment_path = format!("{}/segment_{{seq}}.m4s", args.mount);
    let player_path = format!("{}/player", args.mount);

    http_server
        .add_route(Method::Get, &mpd_path, mpd_handler)
        .await;
    http_server
        .add_route(Method::Get, &init_path, init_handler)
        .await;
    http_server
        .add_route(Method::Get, &segment_path, segment_handler)
        .await;

    // Embedded dash.js player page
    let http_port = http_server.local_addr().port();
    let mpd_url = format!("http://127.0.0.1:{http_port}{}/manifest.mpd", args.mount);
    let html = dash_player_html(&mpd_url);
    let player_handler: rtflow::http::Handler = Arc::new(move |_req, mut writer| {
        let html = html.clone();
        Box::pin(async move {
            writer
                .write_body(200, "text/html; charset=utf-8", html.as_bytes())
                .await?;
            Ok(writer)
        })
    });
    http_server
        .add_route(Method::Get, &player_path, player_handler)
        .await;
    http_server.run().await?;

    let rtsp_url = format!(
        "rtsp://127.0.0.1:{}{}",
        rtsp_server.local_addr().port(),
        args.mount
    );

    println!("=== DASH Server ===");
    println!("RTSP ingest:    {rtsp_url}");
    println!("DASH manifest:  {mpd_url}");
    println!(
        "Browser player: http://127.0.0.1:{http_port}{}/player",
        args.mount
    );
    println!(
        "Segment window: {} x {}s",
        args.segment_count, args.segment_duration
    );
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
