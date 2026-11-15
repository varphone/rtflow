//! RTMP session state machine.
//!
//! Handles a single RTMP connection through its lifecycle:
//! Handshake -> Command (connect/createStream) -> Publish or Play.
use super::chunk::{
    Chunker, Dechunker, MessageType, RtmpMessage, set_chunk_size_message,
    set_peer_bandwidth_message, user_control_message, window_ack_size_message,
};
use super::command::{self, RtmpCommand};
use super::flv::{self, AudioData, VideoData};
use super::handshake;
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use rtflow_media::{AccessUnit, Codec, MediaDescription, MediaKind};
use rtflow_mount::{LocalPublisher, MountConfig, MountHandle, MountRegistry, PushResult};
use rtflow_core::error::{Error, Result};

/// Trait for RTMP publish authentication.
///
/// Implementations decide whether a given publish request (app + stream name)
/// is allowed. This enables stream-key validation, IP whitelisting, etc.
pub trait PublishAuthenticator: Send + Sync + std::fmt::Debug {
    /// Check if publishing `stream_name` on `app` is allowed.
    ///
    /// Returns `true` if the publish is authorized.
    fn authorize_publish(&self, app: &str, stream_name: &str) -> bool;

    /// Check if playing `stream_name` on `app` is allowed. Defaults to the
    /// publish policy so a configured key list protects both directions.
    fn authorize_play(&self, app: &str, stream_name: &str) -> bool {
        self.authorize_publish(app, stream_name)
    }
}

/// A simple authenticator that validates stream keys against a static list.
///
/// The key format is `app/stream_name`. If the list is empty, all publishes
/// are allowed (open mode).
#[derive(Clone, Debug, Default)]
pub struct StreamKeyAuthenticator {
    /// Allowed stream keys in the form "app/stream_name".
    /// If empty, all publishes are allowed.
    allowed_keys: Vec<String>,
}

impl StreamKeyAuthenticator {
    /// Create a new authenticator with the given allowed stream keys.
    pub fn new(allowed_keys: Vec<String>) -> Self {
        Self { allowed_keys }
    }

    /// Add an allowed stream key.
    pub fn allow(&mut self, key: impl Into<String>) {
        self.allowed_keys.push(key.into());
    }
}

impl PublishAuthenticator for StreamKeyAuthenticator {
    fn authorize_publish(&self, app: &str, stream_name: &str) -> bool {
        if self.allowed_keys.is_empty() {
            return true; // Open mode
        }
        let key = format!("{app}/{stream_name}");
        self.allowed_keys.contains(&key)
    }
}

/// RTMP session configuration.
#[derive(Clone, Debug)]
pub struct RtmpSessionConfig {
    /// Default application name if not specified in connect.
    pub default_app: String,
    /// Chunk size to negotiate (outgoing).
    pub chunk_size: u32,
    /// Window acknowledgement size.
    pub window_ack_size: u32,
    /// Per-write deadline for the playback path (R21): a frozen peer
    /// must not pin the writer task forever.
    pub write_timeout: Duration,
    /// Optional publish authenticator. If `None`, all publishes are allowed.
    pub authenticator: Option<Arc<dyn PublishAuthenticator>>,
    /// Optional HMAC URL token (OBS stream key: `name?token=...`).
    pub token: Option<rtflow_rtsp::token_auth::TokenConfig>,
}

impl Default for RtmpSessionConfig {
    fn default() -> Self {
        Self {
            default_app: "live".to_string(),
            chunk_size: 4096,
            window_ack_size: 2_500_000,
            write_timeout: Duration::from_secs(30),
            authenticator: None,
            token: None,
        }
    }
}

/// Session state.
///
/// Fields not read yet belong to the experimental RTMP session machine
/// (publish/play wiring is partially implemented); kept so the shape is
/// complete for the remaining work.
#[allow(dead_code)]
enum SessionState {
    /// Waiting for connect command.
    Init,
    /// Connected, waiting for createStream/publish/play.
    Connected { app: String },
    /// Publishing to a mount.
    Publishing {
        app: String,
        stream_name: String,
        publisher: LocalPublisher,
        mount_handle: MountHandle,
        /// Whether this session registered the mount itself (and should
        /// unregister it when the session ends).
        registered_here: bool,
        video_seq_header: Option<Bytes>,
        audio_seq_header: Option<Bytes>,
        video_codec: Option<Codec>,
        has_video: bool,
        has_audio: bool,
        width: Option<u32>,
        height: Option<u32>,
        frame_rate: Option<(u32, u32)>,
        audio_sample_rate: Option<u32>,
        audio_channels: Option<u8>,
        base_timestamp: Option<u32>,
        /// Consecutive publisher-queue drops; a persistently slow
        /// subscriber must not grow the mount buffer forever.
        drop_streak: u32,
    },
    /// Playing from a mount.
    Playing {
        app: String,
        stream_name: String,
        mount_handle: MountHandle,
    },
}

/// Push one frame to the mount. Returns false once the publisher has been
/// continuously dropping for `MAX_DROP_STREAK` frames; continuing would only
/// grow latency for every subscriber.
fn publish_frame(publisher: &LocalPublisher, unit: AccessUnit, drop_streak: &mut u32) -> bool {
    match publisher.push(unit) {
        Ok(PushResult::Dropped) => {
            *drop_streak = drop_streak.saturating_add(1);
            if *drop_streak == 1 || drop_streak.is_multiple_of(30) {
                debug!(
                    streak = *drop_streak,
                    "RTMP: publisher queue dropping frames"
                );
            }
            *drop_streak < 90
        }
        Ok(_) => {
            *drop_streak = 0;
            true
        }
        Err(error) => {
            debug!(?error, "RTMP: publisher push failed");
            false
        }
    }
}

/// Serve a single RTMP connection.
/// Mirrors RTMP publisher/player session counts into the global metrics
/// registry for the lifetime of the session (RAII).
#[cfg(feature = "metrics")]
struct RtmpSessionMetrics {
    metrics: Option<Arc<rtflow_core::metrics::MetricsRegistry>>,
    is_publisher: bool,
}
#[cfg(feature = "metrics")]
impl Drop for RtmpSessionMetrics {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if let Some(metrics) = &self.metrics {
            if self.is_publisher {
                metrics
                    .rtmp_publishers_active
                    .fetch_sub(1, Ordering::Relaxed);
            } else {
                metrics.rtmp_players_active.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

pub async fn serve_rtmp_session(
    mut stream: TcpStream,
    mounts: Arc<MountRegistry>,
    config: RtmpSessionConfig,
    cancel: CancellationToken,
    #[cfg(feature = "metrics")] metrics: Option<Arc<rtflow_core::metrics::MetricsRegistry>>,
) -> Result<()> {
    let peer = stream.peer_addr().ok();
    debug!(?peer, "RTMP: new connection");

    // Handshake with a hard timeout so idle peers cannot hold a
    // connection slot forever (the accept loop is connection-bounded).
    let handshake_timeout = Duration::from_secs(10);
    if let Err(e) =
        tokio::time::timeout(handshake_timeout, handshake::server_handshake(&mut stream)).await
    {
        debug!(?peer, "RTMP: handshake timed out or failed: {e}");
        return Err(Error::Rtmp(format!("handshake failed: {e}")));
    }
    debug!(?peer, "RTMP: handshake complete");

    // Split the stream: read half stays here, write half goes to writer task
    let (mut read_half, write_half) = stream.into_split();

    // Channel for outgoing messages (command responses + media frames)
    let (tx, mut rx) = mpsc::channel::<RtmpMessage>(256);

    // Writer task: encodes messages with Chunker and writes to socket
    let writer_cancel = cancel.clone();
    let (writer_done_tx, mut writer_done_rx) = tokio::sync::oneshot::channel::<()>();
    let writer_task = tokio::spawn(async move {
        let mut write_stream = write_half;
        let mut chunker = Chunker::new();
        chunker.set_chunk_size(config.chunk_size);
        // Reused output buffer: avoids a fresh allocation + full-frame
        // copy per message on the playback hot path.
        let mut out = bytes::BytesMut::new();

        loop {
            tokio::select! {
                _ = writer_cancel.cancelled() => break,
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    chunker.encode_into(&msg, &mut out);
                    let write_result = tokio::time::timeout(
                        config.write_timeout,
                        write_stream.write_all(&out),
                    )
                    .await;
                    match write_result {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) | Err(_) => break,
                    }
                    let _ = write_stream.flush().await;
                }
            }
        }
        let _ = writer_done_tx.send(());
    });

    let mut dechunker = Dechunker::new();
    let mut state = SessionState::Init;
    let mut msg_stream_id: u32 = 1;
    let mut read_buf = vec![0u8; 8192];
    let mut subscriber_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut bytes_received: u64 = 0;
    let mut last_ack_sent: u64 = 0;
    let idle_timeout = Duration::from_secs(30);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!(?peer, "RTMP: server shutdown");
                break;
            }
            _ = &mut writer_done_rx => {
                debug!(?peer, "RTMP: writer stopped");
                break;
            }
            result = async {
                if matches!(state, SessionState::Playing { .. }) {
                    // Playing clients send no data (ACKs only above 2.5MB),
                    // so an idle timeout would disconnect every low-bitrate
                    // stream.
                    read_half.read(&mut read_buf).await
                } else {
                    match tokio::time::timeout(idle_timeout, read_half.read(&mut read_buf)).await {
                        Ok(result) => result,
                        Err(_) => Err(tokio::io::Error::new(
                            tokio::io::ErrorKind::TimedOut,
                            "idle",
                        )),
                    }
                }
            } => {
                let n = match result {
                    Err(e) if e.kind() == tokio::io::ErrorKind::TimedOut => {
                        debug!(?peer, "RTMP: idle timeout");
                        break;
                    }
                    Err(e) => {
                        debug!(?peer, "RTMP: read error: {e}");
                        break;
                    }
                    Ok(0) => {
                        debug!(?peer, "RTMP: connection closed by peer");
                        break;
                    }
                    Ok(n) => n,
                };
                bytes_received += n as u64;
                dechunker.feed(&read_buf[..n]);
                if dechunker.overflowed() {
                    debug!(?peer, "RTMP: dechunker overflow, dropping connection");
                    break;
                }

                // Send ACK if we've received enough data
                if bytes_received - last_ack_sent >= config.window_ack_size as u64 {
                    last_ack_sent = bytes_received;
                    let mut ack_payload = bytes::BytesMut::with_capacity(4);
                    ack_payload.extend_from_slice(&(bytes_received as u32).to_be_bytes());
                    let _ = tx.send(RtmpMessage {
                        chunk_stream_id: 2,
                        timestamp: 0,
                        msg_type: MessageType::Acknowledgement as u8,
                        msg_stream_id: 0,
                        payload: ack_payload.freeze(),
                    }).await;
                }
            }
        }

        // Process all complete messages
        let messages = dechunker.poll_messages();
        let mut overloaded = false;
        for msg in messages {
            // Handle protocol control messages
            if msg.msg_type == MessageType::SetChunkSize as u8 {
                if msg.payload.len() >= 4 {
                    let size = u32::from_be_bytes([
                        msg.payload[0],
                        msg.payload[1],
                        msg.payload[2],
                        msg.payload[3],
                    ]) & 0x7FFF_FFFF;
                    dechunker.set_chunk_size(size);
                    debug!(?peer, "RTMP: peer chunk size = {size}");
                }
                continue;
            }

            if msg.msg_type == MessageType::WindowAckSize as u8
                || msg.msg_type == MessageType::Acknowledgement as u8
            {
                continue;
            }

            match msg.msg_type {
                t if t == MessageType::CommandAmf0 as u8 || t == MessageType::CommandAmf3 as u8 => {
                    let payload = if t == MessageType::CommandAmf3 as u8 && !msg.payload.is_empty()
                    {
                        &msg.payload[1..]
                    } else {
                        &msg.payload[..]
                    };

                    if let Some(cmd) = RtmpCommand::parse(payload) {
                        let response = handle_command(
                            &cmd,
                            &mut state,
                            &mounts,
                            &config,
                            &mut msg_stream_id,
                            &tx,
                            &mut subscriber_task,
                            &cancel,
                            #[cfg(feature = "metrics")]
                            &metrics,
                        )
                        .await;

                        if let Some(response_msgs) = response {
                            for resp in response_msgs {
                                if tx.send(resp).await.is_err() {
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
                t if t == MessageType::Video as u8 => {
                    if let SessionState::Publishing {
                        publisher,
                        mount_handle,
                        stream_name,
                        video_seq_header,
                        video_codec,
                        has_video,
                        has_audio,
                        width,
                        height,
                        frame_rate,
                        audio_sample_rate,
                        audio_channels,
                        base_timestamp,
                        drop_streak,
                        ..
                    } = &mut state
                        && let Some(video_data) = flv::parse_video_tag(&msg.payload)
                    {
                        match video_data {
                            VideoData::AvcSequenceHeader(data) => {
                                *video_seq_header = Some(data.clone());
                                if let Ok(mut headers) = mount_handle.mount().rtmp_seq_headers.lock()
                                {
                                    headers.insert(0, data);
                                }
                                *video_codec = Some(Codec::H264);
                                *has_video = true;
                                let _ = commands::sync_description(
                                    mount_handle,
                                    stream_name,
                                    Some(Codec::H264),
                                    true,
                                    *has_audio,
                                    *width,
                                    *height,
                                    *frame_rate,
                                    *audio_sample_rate,
                                    *audio_channels,
                                    video_seq_header.as_deref(),
                                );
                            }
                            VideoData::HevcSequenceHeader(data) => {
                                *video_seq_header = Some(data.clone());
                                if let Ok(mut headers) = mount_handle.mount().rtmp_seq_headers.lock()
                                {
                                    headers.insert(0, data);
                                }
                                *video_codec = Some(Codec::H265);
                                *has_video = true;
                                let _ = commands::sync_description(
                                    mount_handle,
                                    stream_name,
                                    Some(Codec::H265),
                                    true,
                                    *has_audio,
                                    *width,
                                    *height,
                                    *frame_rate,
                                    *audio_sample_rate,
                                    *audio_channels,
                                    video_seq_header.as_deref(),
                                );
                            }
                            VideoData::AvcNalu {
                                data,
                                is_keyframe,
                                composition_time,
                            } => {
                                let base = *base_timestamp.get_or_insert(msg.timestamp);
                                let ts_ms = msg.timestamp.wrapping_sub(base);
                                let pts_ms = (ts_ms as i64 + composition_time as i64).max(0) as u32;
                                let data = if is_keyframe {
                                    flv::avcc_to_annexb(&data, video_seq_header.as_deref())
                                } else {
                                    flv::avcc_to_annexb(&data, None)
                                };

                                let unit = AccessUnit {
                                    track_id: 0,
                                    codec: Codec::H264,
                                    pts: Duration::from_millis(pts_ms as u64),
                                    dts: Some(Duration::from_millis(ts_ms as u64)),
                                    rtp_timestamp: ts_ms.wrapping_mul(90),
                                    is_keyframe,
                                    data,
                                };
                                if !publish_frame(publisher, unit, drop_streak) {
                                    overloaded = true;
                                    break;
                                }
                            }
                            VideoData::HevcNalu {
                                data,
                                is_keyframe,
                                composition_time,
                            } => {
                                let base = *base_timestamp.get_or_insert(msg.timestamp);
                                let ts_ms = msg.timestamp.wrapping_sub(base);
                                let pts_ms = (ts_ms as i64 + composition_time as i64).max(0) as u32;
                                let data = if is_keyframe {
                                    flv::avcc_to_annexb(&data, video_seq_header.as_deref())
                                } else {
                                    flv::avcc_to_annexb(&data, None)
                                };

                                let unit = AccessUnit {
                                    track_id: 0,
                                    codec: Codec::H265,
                                    pts: Duration::from_millis(pts_ms as u64),
                                    dts: Some(Duration::from_millis(ts_ms as u64)),
                                    rtp_timestamp: ts_ms.wrapping_mul(90),
                                    is_keyframe,
                                    data,
                                };
                                if !publish_frame(publisher, unit, drop_streak) {
                                    overloaded = true;
                                    break;
                                }
                            }
                            VideoData::Vp9SequenceHeader(data) => {
                                let codec = Codec::Vp9;
                                *video_seq_header = Some(data.clone());
                                if let Ok(mut headers) = mount_handle.mount().rtmp_seq_headers.lock()
                                {
                                    headers.insert(0, data);
                                }
                                *video_codec = Some(codec);
                                *has_video = true;
                                let _ = commands::sync_description(
                                    mount_handle,
                                    stream_name,
                                    Some(codec),
                                    true,
                                    *has_audio,
                                    *width,
                                    *height,
                                    *frame_rate,
                                    *audio_sample_rate,
                                    *audio_channels,
                                    video_seq_header.as_deref(),
                                );
                            }
                            VideoData::Av1SequenceHeader(data) => {
                                let codec = Codec::Av1;
                                *video_seq_header = Some(data.clone());
                                if let Ok(mut headers) = mount_handle.mount().rtmp_seq_headers.lock()
                                {
                                    headers.insert(0, data);
                                }
                                *video_codec = Some(codec);
                                *has_video = true;
                                let _ = commands::sync_description(
                                    mount_handle,
                                    stream_name,
                                    Some(codec),
                                    true,
                                    *has_audio,
                                    *width,
                                    *height,
                                    *frame_rate,
                                    *audio_sample_rate,
                                    *audio_channels,
                                    video_seq_header.as_deref(),
                                );
                            }
                            VideoData::Vp9Nalu {
                                data,
                                is_keyframe,
                                composition_time,
                            } => {
                                let codec = Codec::Vp9;
                                let base = *base_timestamp.get_or_insert(msg.timestamp);
                                let ts_ms = msg.timestamp.wrapping_sub(base);
                                let pts_ms = (ts_ms as i64 + composition_time as i64).max(0) as u32;
                                let unit = AccessUnit {
                                    track_id: 0,
                                    codec,
                                    pts: Duration::from_millis(pts_ms as u64),
                                    dts: Some(Duration::from_millis(ts_ms as u64)),
                                    rtp_timestamp: ts_ms.wrapping_mul(90),
                                    is_keyframe,
                                    data,
                                };
                                if !publish_frame(publisher, unit, drop_streak) {
                                    overloaded = true;
                                    break;
                                }
                            }
                            VideoData::Av1Nalu {
                                data,
                                is_keyframe,
                                composition_time,
                            } => {
                                let codec = Codec::Av1;
                                let base = *base_timestamp.get_or_insert(msg.timestamp);
                                let ts_ms = msg.timestamp.wrapping_sub(base);
                                let pts_ms = (ts_ms as i64 + composition_time as i64).max(0) as u32;
                                let unit = AccessUnit {
                                    track_id: 0,
                                    codec,
                                    pts: Duration::from_millis(pts_ms as u64),
                                    dts: Some(Duration::from_millis(ts_ms as u64)),
                                    rtp_timestamp: ts_ms.wrapping_mul(90),
                                    is_keyframe,
                                    data,
                                };
                                if !publish_frame(publisher, unit, drop_streak) {
                                    overloaded = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                t if t == MessageType::Audio as u8 => {
                    if let SessionState::Publishing {
                        publisher,
                        mount_handle,
                        stream_name,
                        audio_seq_header,
                        video_codec,
                        has_audio,
                        has_video,
                        width,
                        height,
                        frame_rate,
                        audio_sample_rate,
                        audio_channels,
                        video_seq_header,
                        base_timestamp,
                        drop_streak,
                        ..
                    } = &mut state
                        && let Some(audio_data) = flv::parse_audio_tag(&msg.payload)
                    {
                        match audio_data {
                            AudioData::AacSequenceHeader(data) => {
                                *audio_seq_header = Some(data.clone());
                                if let Some((rate, channels)) = aac_audio_config(&data) {
                                    *audio_sample_rate = Some(rate);
                                    *audio_channels = Some(channels);
                                }
                                let track_id: u8 = if *has_video { 1 } else { 0 };
                                if let Ok(mut headers) = mount_handle.mount().rtmp_seq_headers.lock()
                                {
                                    headers.insert(track_id, data);
                                }
                                *has_audio = true;
                                let _ = commands::sync_description(
                                    mount_handle,
                                    stream_name,
                                    *video_codec,
                                    *has_video,
                                    true,
                                    *width,
                                    *height,
                                    *frame_rate,
                                    *audio_sample_rate,
                                    *audio_channels,
                                    video_seq_header.as_deref(),
                                );
                            }
                            AudioData::AacRaw(data) => {
                                let base = *base_timestamp.get_or_insert(msg.timestamp);
                                let ts_ms = msg.timestamp.wrapping_sub(base);
                                let track_id: u8 = if *has_video { 1 } else { 0 };

                                let unit = AccessUnit {
                                    track_id,
                                    codec: Codec::Aac,
                                    pts: Duration::from_millis(ts_ms as u64),
                                    dts: None,
                                    // 44.1/48/32kHz from ms: use 64-bit
                                    // intermediate so 44100/1000 is not
                                    // truncated to an integer ×44.
                                    rtp_timestamp: ((ts_ms as u64
                                        * u64::from(audio_sample_rate.unwrap_or(48_000)))
                                        / 1000)
                                        as u32,
                                    is_keyframe: true,
                                    data,
                                };
                                if !publish_frame(publisher, unit, drop_streak) {
                                    overloaded = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                t if t == MessageType::DataAmf0 as u8 || t == MessageType::DataAmf3 as u8 => {
                    let payload = if t == MessageType::DataAmf3 as u8 && !msg.payload.is_empty() {
                        &msg.payload[1..]
                    } else {
                        &msg.payload[..]
                    };
                    if let Some(RtmpCommand::MetaData {
                        data: Some(data), ..
                    }) = RtmpCommand::parse(payload)
                    {
                        commands::apply_metadata(&mut state, &data);
                    }
                }
                _ => {}
            }
        }
        if overloaded {
            warn!("RTMP: publisher persistently overloaded, closing session");
            break;
        }
    }

    // Cleanup
    if let Some(task) = subscriber_task {
        task.abort();
    }
    drop(tx); // close channel so writer task exits
    let _ = writer_task.await;

    // If this session registered its own publisher mount and no other
    // publisher remains, unregister it so distinct stream paths do not
    // permanently consume registry slots.
    let registered_path = match &state {
        SessionState::Publishing {
            mount_handle,
            registered_here: true,
            ..
        } => Some(Arc::from(mount_handle.path.as_str())),
        _ => None,
    };
    drop(state); // releases the LocalPublisher
    if let Some(path) = registered_path
        && let Some(handle) = mounts.get_handle(&path)
        && handle
            .mount()
            .publishers
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
    {
        mounts.unregister(&path);
        debug!(?peer, "RTMP: unregistered publisher mount {path}");
    }

    Ok(())
}

fn aac_audio_config(data: &[u8]) -> Option<(u32, u8)> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let first = *data.first()?;
    let second = *data.get(1)?;
    let index = (((first & 0x07) as usize) << 1) | ((second as usize) >> 7);
    let rate = *RATES.get(index)?;
    let channels = (second >> 3) & 0x0F;
    Some((rate, channels.max(1)))
}

mod commands;
#[cfg(test)]
mod tests;

use commands::handle_command;
