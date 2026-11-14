//! WS-FLV: WebSocket transport for live FLV (flv.js `ws://host/path.flv`).
//!
//! Low-latency browser playback: a WebSocket frame per FLV chunk removes
//! the HTTP chunked-encoding overhead of HTTP-FLV and avoids connection
//! reuse stalls. RFC 6455 server side only — unmasked binary frames out,
//! masked control frames (ping/close) in. The path convention matches
//! HTTP-FLV: mount `/live/stream` is served at `/live/stream.flv`.
//!
//! ```text
//! flv.js: flvjs.createPlayer({ type: 'flv', isLive: true,
//!            url: 'ws://host/live/stream.flv' })
//! ```

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use sha1::{Digest as _, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use super::handler::h265_vcl_nal_type;
use super::mux::FlvMuxer;
use crate::media::{AccessUnit, Codec};
use crate::{MountRegistry, Result};

/// RFC 6455 magic GUID appended to `Sec-WebSocket-Key` before hashing.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Upper bound for the HTTP upgrade request.
const MAX_HANDSHAKE_BYTES: usize = 8 * 1024;
/// Client-to-server frames are only control frames in WS-FLV; cap them hard
/// instead of preallocating a multi-megabyte buffer per viewer.
const MAX_FRAME_PAYLOAD: u64 = 64 * 1024;
/// Control frames must be <=125 bytes (RFC 6455 §5.5).
const MAX_CONTROL_PAYLOAD: u64 = 125;
/// Bound a stalled viewer's write instead of pinning a session forever.
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// WS-FLV server configuration.
#[derive(Clone, Debug)]
pub struct WsFlvConfig {
    /// Bind address (`0.0.0.0:8088`).
    pub bind_addr: SocketAddr,
    /// Maximum concurrent viewers (excess connections are closed).
    pub max_connections: usize,
    /// Timeout for the HTTP upgrade handshake.
    pub handshake_timeout: Duration,
    /// Heartbeat interval: when no client frame arrives within this
    /// period the server sends a ping; a second silent interval closes
    /// the connection (~2× this value to reap dead peers). `None`
    /// disables heartbeats (connections then live until TCP fails).
    ///
    /// Default 30 s → dead viewers are reaped within ~60 s, which keeps
    /// embedded gateways from leaking sockets/fds on flaky Wi-Fi links.
    pub ping_interval: Option<Duration>,
}

impl Default for WsFlvConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:8088".parse().unwrap(),
            max_connections: 64,
            handshake_timeout: Duration::from_secs(5),
            ping_interval: Some(Duration::from_secs(30)),
        }
    }
}

/// Active WS-FLV viewer gauge (shared across servers for observability).
static ACTIVE_VIEWERS: AtomicUsize = AtomicUsize::new(0);

/// WS-FLV server: bind first (port may be 0), then `run()` the accept loop.
pub struct WsFlvServer {
    listener: TcpListener,
    config: WsFlvConfig,
    mounts: Arc<MountRegistry>,
}

impl WsFlvServer {
    /// Bind the listener; returns immediately (accept loop starts on `run`).
    pub async fn bind(config: WsFlvConfig, mounts: Arc<MountRegistry>) -> Result<Self> {
        let listener = TcpListener::bind(config.bind_addr).await?;
        Ok(Self {
            listener,
            config,
            mounts,
        })
    }

    /// Actual bound address (useful when `bind_addr` port is 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().unwrap_or(self.config.bind_addr)
    }

    /// Accept and serve viewers until cancelled/closed.
    pub async fn run(self) -> Result<()> {
        tracing::info!("WS-FLV listening on {}", self.local_addr());
        let Self {
            listener,
            config,
            mounts,
        } = self;
        loop {
            let (stream, peer) = listener.accept().await?;
            // Low-latency streaming: disable Nagle so each WS-FLV frame is
            // sent immediately (tokio defaults to true, but be explicit).
            let _ = stream.set_nodelay(true);
            if ACTIVE_VIEWERS.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                ACTIVE_VIEWERS.fetch_sub(1, Ordering::AcqRel);
                tracing::debug!("ws-flv {peer}: connection limit reached");
                drop(stream);
                continue;
            }
            let mounts = mounts.clone();
            let config = config.clone();
            tokio::spawn(async move {
                let outcome = handle_connection(stream, mounts, config).await;
                ACTIVE_VIEWERS.fetch_sub(1, Ordering::AcqRel);
                if let Err(e) = outcome {
                    tracing::debug!("ws-flv {peer}: {e}");
                }
            });
        }
    }
}

/// Convenience: bind + run (spawn this on a task).
pub async fn serve(config: WsFlvConfig, mounts: Arc<MountRegistry>) -> Result<()> {
    WsFlvServer::bind(config, mounts).await?.run().await
}

/// Number of connected WS-FLV viewers (for metrics/monitoring).
pub fn active_viewers() -> usize {
    ACTIVE_VIEWERS.load(Ordering::Relaxed)
}

async fn handle_connection(
    stream: TcpStream,
    mounts: Arc<MountRegistry>,
    config: WsFlvConfig,
) -> Result<()> {
    let (mount_path, stream) = handshake(stream, config.handshake_timeout).await?;
    let mount = mounts
        .get_handle(&mount_path)
        .ok_or_else(|| crate::Error::Media("ws-flv: mount not found".into()))?;
    let description = mount.description();
    let (mut rx, startup) = mount.subscribe_with_startup();
    let fmtp_source = mount;

    let video_codec = description
        .tracks
        .iter()
        .find(|t| t.kind == crate::media::MediaKind::Video)
        .map(|t| t.codec);
    let mut video_seq_pending = video_codec.is_some();
    let mut audio_seq_pending = description.tracks.iter().any(|t| t.codec == Codec::Aac);
    let mut fmtp_snapshot = fmtp_source.announce_fmtp();

    let mut muxer = FlvMuxer::new(&description);
    configure_sequence_headers(
        &mut muxer,
        &description,
        &fmtp_snapshot,
        &fmtp_source,
        &mut video_seq_pending,
        &mut audio_seq_pending,
    );

    // Cached audio may predate the cached video keyframe; start both tracks
    // at that keyframe so normalized timestamps stay monotonic.
    let startup_keyframe_pts = startup
        .iter()
        .find(|unit| matches!(unit.codec, Codec::H264 | Codec::H265) && unit.is_keyframe)
        .map(|unit| unit.pts);

    let (read_half, write_half) = stream.into_split();
    let write_half = Arc::new(Mutex::new(write_half));

    // Reader task: answer pings with pongs, run the heartbeat (server ping
    // + dead-peer reaping), exit on close/disconnect.
    let done = Arc::new(AtomicBool::new(false));
    let reader_task = {
        let write_half = write_half.clone();
        let done = done.clone();
        let ping_interval = config.ping_interval;
        tokio::spawn(async move {
            let _ = read_control_loop(read_half, write_half, ping_interval).await;
            done.store(true, Ordering::Relaxed);
        })
    };

    // Send the FLV header + onMetaData, then stream tags.
    {
        let mut guard = write_half.lock().await;
        write_ws_frame(&mut *guard, 0x2, &muxer.flv_header()).await?;
        if let Some(metadata) = muxer.metadata_tag() {
            write_ws_frame(&mut *guard, 0x2, &metadata).await?;
        }
        guard.flush().await?;
    }

    let startup: Vec<std::sync::Arc<AccessUnit>> = startup_keyframe_pts
        .map(|keyframe_pts| {
            startup
                .into_iter()
                .filter(|unit| unit.pts >= keyframe_pts)
                .collect()
        })
        .unwrap_or_default();
    let startup_keys = crate::mount::startup_dedup_keys(&startup);
    let mut startup = VecDeque::from(startup);
    let mut awaiting_video_keyframe = video_codec.is_some();
    let mut dropping_h265_cra_leading_pictures = false;

    let mut send_close = false;
    loop {
        // Snapshot frames bypass the dedup check (see handler.rs).
        let in_startup = !startup.is_empty();
        let unit = if let Some(unit) = startup.pop_front() {
            unit
        } else {
            if done.load(Ordering::Relaxed) {
                break; // client closed
            }
            match tokio::select! {
                biased;
                _ = tokio::time::sleep(Duration::from_millis(200)) => continue,
                unit = rx.recv() => unit,
            } {
                Ok(unit) => unit,
                Err(crate::mount::FrameRecvError::Lagged(n)) => {
                    tracing::debug!("ws-flv lagged {n}; waiting for a keyframe");
                    muxer.reset_after_discontinuity();
                    video_seq_pending = video_codec.is_some();
                    audio_seq_pending = description.tracks.iter().any(|t| t.codec == Codec::Aac);
                    awaiting_video_keyframe = video_codec.is_some();
                    dropping_h265_cra_leading_pictures = false;
                    continue;
                }
                Err(crate::mount::FrameRecvError::Closed) => {
                    // Mount unloaded: tell the client before closing the
                    // TCP connection (RFC 6455 §5.5.1).
                    send_close = true;
                    break;
                }
            }
        };
        // Skip broadcast replays of frames the startup snapshot delivered.
        if !in_startup && startup_keys.contains(&(unit.track_id, unit.rtp_timestamp)) {
            continue;
        }
        let is_video = matches!(unit.codec, Codec::H264 | Codec::H265);
        if awaiting_video_keyframe && is_video {
            if !unit.is_keyframe {
                continue;
            }
            awaiting_video_keyframe = false;
            dropping_h265_cra_leading_pictures = false;
        }
        if dropping_h265_cra_leading_pictures && is_video {
            match h265_vcl_nal_type(&unit.data) {
                Some(8 | 9) => continue,
                Some(_) => dropping_h265_cra_leading_pictures = false,
                None => continue,
            }
        }
        let starts_h265_cra = unit.is_keyframe
            && unit.codec == Codec::H265
            && super::handler::h265_random_access_is_cra(&unit.data);
        if video_seq_pending || audio_seq_pending {
            configure_sequence_headers(
                &mut muxer,
                &description,
                &fmtp_snapshot,
                &fmtp_source,
                &mut video_seq_pending,
                &mut audio_seq_pending,
            );
            if video_seq_pending || audio_seq_pending {
                fmtp_snapshot = fmtp_source.announce_fmtp();
            }
        }
        if video_seq_pending && unit.is_keyframe && is_video {
            let seq = match unit.codec {
                Codec::H264 => FlvMuxer::build_avc_sequence_header(&unit.data),
                Codec::H265 => FlvMuxer::build_h265_sequence_header(&unit.data),
                _ => None,
            };
            if let Some(seq) = seq {
                muxer.set_video_sequence_header(seq);
                video_seq_pending = false;
            }
        }
        if audio_seq_pending && unit.codec == Codec::Aac {
            audio_seq_pending = false;
        }

        let tags = muxer.encode(&unit);
        if starts_h265_cra {
            dropping_h265_cra_leading_pictures = true;
        }
        if !tags.is_empty() {
            let mut guard = write_half.lock().await;
            write_ws_frame(&mut *guard, 0x2, &tags).await?;
            guard.flush().await?;
        }
    }
    // Best-effort close frame on server-initiated shutdown so well-behaved
    // clients see a clean close instead of a bare FIN.
    if send_close {
        let mut guard = write_half.lock().await;
        let _ = write_ws_frame(&mut *guard, 0x8, &[]).await;
        let _ = guard.flush().await;
    }
    reader_task.abort();
    let _ = reader_task.await;
    Ok(())
}

/// HTTP upgrade handshake: read the request, validate the WS headers,
/// answer 101 with the computed accept key, return (mount_path, stream).
///
/// Request-line/header parsing is delegated to the shared HTTP parser
/// (`crate::http::request::parse_request`) so WS-FLV honours the same
/// framing rules (header size limit, content-length validation, method
/// whitelist) as every other HTTP endpoint.
async fn handshake(mut stream: TcpStream, timeout: Duration) -> Result<(String, TcpStream)> {
    use crate::http::{Method, ParseError, parse_request};

    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let request = loop {
        let n = tokio::time::timeout(timeout, stream.read(&mut tmp))
            .await
            .map_err(|_| crate::Error::Http("ws-flv: handshake timeout".into()))??;
        if n == 0 {
            return Err(crate::Error::Http("ws-flv: closed during handshake".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
        match parse_request(&buf, MAX_HANDSHAKE_BYTES, 0) {
            Ok(Some((request, _))) => break request,
            // parse_request never yields Ok(None) today; keep the branch
            // for forward compatibility with incremental parsing.
            Ok(None) => unreachable!("parse_request returns Ok(None)"),
            Err(ParseError::Incomplete) => continue,
            Err(_) => {
                return Err(crate::Error::Http("ws-flv: invalid handshake".into()));
            }
        }
    };

    if request.method != Method::Get {
        return Err(crate::Error::Http("ws-flv: method must be GET".into()));
    }
    if !request.path.starts_with('/') {
        return Err(crate::Error::Http("ws-flv: invalid path".into()));
    }

    let mut sec_key: Option<String> = None;
    let mut upgrade_ok = false;
    let mut version_ok = false;
    for (key, value) in &request.headers {
        if key.eq_ignore_ascii_case("sec-websocket-key") {
            sec_key = Some(value.clone());
        }
        if key.eq_ignore_ascii_case("upgrade") && value.to_ascii_lowercase().contains("websocket") {
            upgrade_ok = true;
        }
        if key.eq_ignore_ascii_case("sec-websocket-version") && value.contains("13") {
            version_ok = true;
        }
    }
    let sec_key =
        sec_key.ok_or_else(|| crate::Error::Http("ws-flv: missing Sec-WebSocket-Key".into()))?;
    if !upgrade_ok {
        return Err(crate::Error::Http(
            "ws-flv: missing Upgrade: websocket".into(),
        ));
    }
    if !version_ok {
        return Err(crate::Error::Http(
            "ws-flv: unsupported Sec-WebSocket-Version".into(),
        ));
    }

    // Accept key: base64(SHA1(key + GUID)) per RFC 6455 §4.2.2.
    let mut hasher = Sha1::new();
    hasher.update(sec_key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    let accept = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());

    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\
         \r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;

    // Path convention: /live/stream.flv -> mount /live/stream.
    let mount_path = request
        .path
        .strip_suffix(".flv")
        .unwrap_or(&request.path)
        .to_string();
    Ok((mount_path, stream))
}

/// Write one server frame: FIN + opcode, unmasked (RFC 6455 §5.1).
/// The frame header is built on a stack array (no per-frame allocation)
/// and sent together with the payload in a single vectored syscall.
async fn write_ws_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    opcode: u8,
    payload: &[u8],
) -> Result<()> {
    let mut hdr = [0u8; 14];
    hdr[0] = 0x80 | opcode;
    let len = payload.len();
    let hdr_len = if len < 126 {
        hdr[1] = len as u8;
        2
    } else if len <= 0xffff {
        hdr[1] = 126;
        hdr[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        4
    } else {
        hdr[1] = 127;
        hdr[2..10].copy_from_slice(&(len as u64).to_be_bytes());
        10
    };
    let bufs = [
        std::io::IoSlice::new(&hdr[..hdr_len]),
        std::io::IoSlice::new(payload),
    ];
    // tokio's write_vectored already loops until every byte is written
    // (unlike std), so the returned amount is always the full length.
    tokio::time::timeout(WS_WRITE_TIMEOUT, w.write_vectored(&bufs))
        .await
        .map_err(|_| crate::Error::Http("ws-flv: write timeout".into()))??;
    Ok(())
}

/// Read one client frame: FIN+opcode byte, length (7/16/64-bit), mask key
/// and unmasked payload. Returns (opcode, payload).
async fn read_client_frame(r: &mut (impl AsyncRead + Unpin)) -> Result<(u8, Vec<u8>)> {
    let mut hdr = [0u8; 2];
    r.read_exact(&mut hdr).await?;
    let opcode = hdr[0] & 0x0f;
    let masked = hdr[1] & 0x80 != 0;
    let mut len = (hdr[1] & 0x7f) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b).await?;
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b).await?;
        len = u64::from_be_bytes(b);
    }
    if len > MAX_FRAME_PAYLOAD {
        return Err(crate::Error::Http("ws-flv: frame too large".into()));
    }
    if opcode >= 0x8 && len > MAX_CONTROL_PAYLOAD {
        return Err(crate::Error::Http("ws-flv: control frame too large".into()));
    }
    // Mask key precedes the payload (RFC 6455 §5.2); client frames
    // are always masked.
    let mut mask = [0u8; 4];
    if masked {
        r.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    if masked {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
    }
    Ok((opcode, payload))
}

/// Read client frames: answer pings with pongs, return on close/disconnect.
/// With heartbeats enabled, a silent interval triggers a server ping and a
/// second silent interval closes the connection (dead-peer reaping).
async fn read_control_loop(
    mut r: impl AsyncRead + Unpin,
    w: Arc<Mutex<impl AsyncWrite + Unpin>>,
    ping_interval: Option<Duration>,
) -> Result<()> {
    // Set when the previous interval ended without any client frame; the
    // next silent interval means the ping went unanswered -> dead peer.
    let mut ping_pending = false;
    loop {
        let frame = match ping_interval {
            Some(interval) => match tokio::time::timeout(interval, read_client_frame(&mut r)).await
            {
                Ok(result) => result?,
                Err(_elapsed) => {
                    if ping_pending {
                        return Err(crate::Error::Http(
                            "ws-flv: client unresponsive to heartbeat".into(),
                        ));
                    }
                    ping_pending = true;
                    // Heartbeat: ping the peer; any client frame (pong,
                    // ping, close, ...) within the next interval proves
                    // the peer is alive.
                    let mut guard = w.lock().await;
                    write_ws_frame(&mut *guard, 0x9, b"rtflow-ping").await?;
                    guard.flush().await?;
                    continue;
                }
            },
            None => read_client_frame(&mut r).await?,
        };
        // Any client activity answers the heartbeat.
        ping_pending = false;
        let (opcode, payload) = frame;
        match opcode {
            0x9 => {
                // Ping -> pong with the same payload.
                let mut guard = w.lock().await;
                write_ws_frame(&mut *guard, 0xA, &payload).await?;
                guard.flush().await?;
            }
            0x8 => {
                // Close -> echo the close frame (RFC 6455 §5.5.1) so the
                // client sees a clean close, then end the read loop.
                let mut guard = w.lock().await;
                write_ws_frame(&mut *guard, 0x8, &payload).await?;
                guard.flush().await?;
                return Ok(());
            }
            _ => {} // Ignore pong/text/binary from the client.
        }
    }
}

// Re-export the sequence-header configuration shared with the HTTP handler.
pub(crate) use super::handler::configure_sequence_headers;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_rfc6455_example() {
        // RFC 6455 §1.3 example: key "dGhlIHNhbXBsZSBub25jZQ=="
        // -> accept "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        let mut hasher = Sha1::new();
        hasher.update(b"dGhlIHNhbXBsZSBub25jZQ==");
        hasher.update(WS_GUID.as_bytes());
        let accept = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[tokio::test]
    async fn ws_frame_roundtrip() {
        // Server frame: FIN + binary, unmasked, small/medium lengths.
        let mut buf = Vec::new();
        write_ws_frame(&mut buf, 0x2, b"hi").await.unwrap();
        assert_eq!(buf, [0x82, 0x02, b'h', b'i']);
        buf.clear();
        let payload = vec![0xabu8; 200];
        write_ws_frame(&mut buf, 0x2, &payload).await.unwrap();
        assert_eq!(buf[0], 0x82);
        assert_eq!(buf[1], 126);
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 200);
        assert_eq!(&buf[4..], &payload[..]);
    }
}
