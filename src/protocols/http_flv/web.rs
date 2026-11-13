//! Browser player page for live mounts (mpegts.js / hls.js).
//!
//! Serves a self-contained HTML page that plays every mount registered in
//! the shared [`MountRegistry`] via three mainstream browser protocols:
//!
//! - **WS-FLV** — WebSocket + FLV, lowest latency, played by mpegts.js
//!   (the maintained flv.js fork). The WS-FLV listener usually runs on its
//!   own port, passed in [`WebPlayerConfig::ws_flv_port`].
//! - **HTTP-FLV** — chunked HTTP + FLV, same mpegts.js player, no extra
//!   port needed (same origin as this page).
//! - **HLS** — `.m3u8` + fMP4, maximum compatibility (hls.js, native
//!   Safari HEVC support). Requires the HLS handlers registered on the
//!   same HTTP server under `{mount}.m3u8` / `{mount}/init.m4s` /
//!   `{mount}/{seq}`.
//!
//! The player auto-reconnects with exponential backoff (1 s → 30 s), which
//! is what IPC cameras need: the encoder (or its RTSP publish) may restart
//! at any time and viewers must recover without a page reload.
//!
//! ```text
//! cargo run --example ipc_camera -- --web
//! # open http://127.0.0.1:8080/ in a browser
//! ```

use crate::MountRegistry;
use crate::http::{Handler, ResponseWriter};
use crate::media::{Codec, MediaKind};
use std::sync::Arc;

const PLAYER_HTML: &str = include_str!("player.html");

/// Default playback mode of the generated page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebPlayerMode {
    /// WebSocket + FLV (lowest latency; needs the WS-FLV listener).
    WsFlv,
    /// Chunked HTTP + FLV (same origin as the page).
    HttpFlv,
    /// HLS `.m3u8` (best compatibility).
    Hls,
}

impl WebPlayerMode {
    fn as_str(self) -> &'static str {
        match self {
            WebPlayerMode::WsFlv => "ws",
            WebPlayerMode::HttpFlv => "http",
            WebPlayerMode::Hls => "hls",
        }
    }
}

/// Player page configuration.
#[derive(Clone, Debug)]
pub struct WebPlayerConfig {
    /// Port of the WS-FLV listener. `None` hides the WS-FLV mode button.
    pub ws_flv_port: Option<u16>,
    /// Mode selected when the page opens.
    pub default_mode: WebPlayerMode,
}

impl Default for WebPlayerConfig {
    fn default() -> Self {
        Self {
            ws_flv_port: None,
            default_mode: WebPlayerMode::WsFlv,
        }
    }
}

/// Codec label shown in the page (browser-side compatibility hints rely
/// on the "H.265" spelling).
fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::H265 => "H.265",
        Codec::Aac => "AAC",
        Codec::Pcma | Codec::Pcmu => "G.711",
        Codec::Opus => "Opus",
        Codec::Vp8 => "VP8",
        Codec::Vp9 => "VP9",
        Codec::Mp3 => "MP3",
        Codec::G726_16 => "G.726-16",
        Codec::G726_24 => "G.726-24",
        Codec::G726_32 => "G.726-32",
        Codec::G726_40 => "G.726-40",
        Codec::Av1 => "AV1",
        Codec::Jpeg => "MJPEG",
        Codec::H263 => "H.263",
        Codec::Mpeg4 => "MPEG-4",
    }
}

/// Escape a string for embedding in a JavaScript string literal.
///
/// Besides quotes/backslashes/control chars, `<` is escaped (`\u003c`) so a
/// mount path can never smuggle `</script>` out of the injected JSON into
/// the HTML document; U+2028/U+2029 are escaped too (they terminate string
/// literals in ES2019 and earlier).
fn js_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\u003c"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Build a JSON array of the mounts currently registered, e.g.
/// `[{"path":"/cam/main","name":"main","video":"H.264","audio":"AAC",...}]`.
fn mounts_json(registry: &MountRegistry) -> Vec<u8> {
    let mut paths: Vec<String> = registry
        .paths()
        .into_iter()
        .map(|p| p.to_string())
        .collect();
    paths.sort();

    let mut out = String::from("[");
    for (i, path) in paths.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let handle = registry.get_handle(path);
        let video = handle.as_ref().and_then(|h| {
            h.description
                .tracks
                .iter()
                .find(|t| t.kind == MediaKind::Video)
        });
        let audio = handle.as_ref().and_then(|h| {
            h.description
                .tracks
                .iter()
                .find(|t| t.kind == MediaKind::Audio)
        });
        let name = path.rsplit('/').next().unwrap_or(path);
        out.push_str(&format!(
            "{{\"path\":\"{}\",\"name\":\"{}\",\"video\":\"{}\",\"audio\":\"{}\",\
             \"width\":{},\"height\":{}}}",
            js_escape(path),
            js_escape(name),
            video.map(|t| codec_label(t.codec)).unwrap_or(""),
            audio.map(|t| codec_label(t.codec)).unwrap_or(""),
            video.and_then(|t| t.width).map_or(-1, |w| w as i64),
            video.and_then(|t| t.height).map_or(-1, |h| h as i64),
        ));
    }
    out.push(']');
    out.into_bytes()
}

/// Create a handler that serves the browser player page for every mount in
/// `registry`. Register it on an [`crate::http::HttpServer`] route, e.g. `"/"`.
pub fn create_web_player_handler(registry: Arc<MountRegistry>, config: WebPlayerConfig) -> Handler {
    Arc::new(move |_req, mut writer: ResponseWriter<_>| {
        let registry = registry.clone();
        let config = config.clone();
        Box::pin(async move {
            // Without a WS-FLV listener the ws mode is useless; fall back
            // to HTTP-FLV so the page opens into a working player.
            let default_mode =
                if config.default_mode == WebPlayerMode::WsFlv && config.ws_flv_port.is_none() {
                    WebPlayerMode::HttpFlv
                } else {
                    config.default_mode
                };
            let mounts = mounts_json(&registry);
            let html = PLAYER_HTML
                .replace("__MOUNTS_JSON__", &String::from_utf8_lossy(&mounts))
                .replace(
                    "__WS_FLV_PORT__",
                    &config
                        .ws_flv_port
                        .map_or_else(String::new, |p| p.to_string()),
                )
                .replace("__DEFAULT_MODE__", default_mode.as_str());
            writer
                .write_body(200, "text/html; charset=utf-8", html.as_bytes())
                .await?;
            Ok(writer)
        })
    })
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use crate::media::{MediaDescription, TrackDescription};
    use crate::server::ServerConfig;
    use crate::{MountConfig, RtspServer};
    use std::sync::Arc;

    async fn registry_with_mounts() -> (RtspServer, Arc<MountRegistry>) {
        let server = RtspServer::bind(
            ServerConfig::builder("127.0.0.1:0".parse().unwrap())
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
        let registry = server.registry();
        let _mount = server
            .register_mount(
                "/cam/main",
                MountConfig::Publisher {
                    description: MediaDescription::from_tracks(
                        "ipc",
                        vec![
                            TrackDescription::h264_video(0, &[], &[], 1920, 1080, 30),
                            TrackDescription::aac_audio(1, 48_000, 2),
                        ],
                    ),
                    allow_replace_after_disconnect: true,
                },
            )
            .await
            .unwrap();
        (server, registry)
    }

    #[tokio::test]
    async fn mounts_json_lists_mounts_with_codecs() {
        let (server, registry) = registry_with_mounts().await;
        let json = String::from_utf8(mounts_json(&registry)).unwrap();
        assert!(
            json.contains(r#""path":"/cam/main""#),
            "mount path in JSON: {json}"
        );
        assert!(json.contains(r#""video":"H.264""#), "video codec: {json}");
        assert!(json.contains(r#""audio":"AAC""#), "audio codec: {json}");
        assert!(
            json.contains(r#""width":1920,"height":1080"#),
            "resolution: {json}"
        );
        server.shutdown().await.unwrap();
    }

    #[test]
    fn js_escape_quotes_and_newlines() {
        assert_eq!(js_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(js_escape("x\ny"), "x\\ny");
        assert_eq!(js_escape("plain"), "plain");
        // `<` must never survive into the document (script-closing XSS).
        assert_eq!(js_escape("</script>"), "\\u003c/script>");
        assert_eq!(js_escape("a\u{2028}b"), "a\\u2028b");
    }

    #[tokio::test]
    async fn page_html_injects_mounts_and_ws_port() {
        use crate::http::{HttpConfig, HttpServer, Method};
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let (server, registry) = registry_with_mounts().await;
        let mut http_server = HttpServer::bind(HttpConfig::new("127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let handler = create_web_player_handler(
            registry,
            WebPlayerConfig {
                ws_flv_port: Some(8088),
                ..Default::default()
            },
        );
        http_server.add_route(Method::Get, "/", handler).await;
        let addr = http_server.local_addr();
        tokio::spawn(async move { http_server.run().await });

        // Give the accept loop a moment to start.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("HTTP/1.1 200"), "status line in {body}");
        assert!(body.contains("text/html"), "content type in {body}");
        assert!(
            body.contains(r#""path":"/cam/main""#),
            "mounts injected: {body}"
        );
        assert!(
            body.contains("const WS_FLV_PORT = \"8088\""),
            "ws port injected"
        );
        assert!(
            body.contains("const DEFAULT_MODE = \"ws\""),
            "default mode injected"
        );
        assert!(body.contains("mpegts.js"), "player library referenced");
        server.shutdown().await.unwrap();
    }
}
