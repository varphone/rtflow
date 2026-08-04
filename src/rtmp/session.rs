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

use crate::media::{AccessUnit, Codec, MediaDescription, MediaKind};
use crate::mount::{LocalPublisher, MountConfig, MountHandle, MountRegistry};
use crate::{Error, Result};

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
}

impl Default for RtmpSessionConfig {
    fn default() -> Self {
        Self {
            default_app: "live".to_string(),
            chunk_size: 4096,
            window_ack_size: 2_500_000,
            write_timeout: Duration::from_secs(30),
            authenticator: None,
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
        base_timestamp: Option<u32>,
    },
    /// Playing from a mount.
    Playing {
        app: String,
        stream_name: String,
        mount_handle: MountHandle,
    },
}

/// Serve a single RTMP connection.
/// Mirrors RTMP publisher/player session counts into the global metrics
/// registry for the lifetime of the session (RAII).
#[cfg(feature = "metrics")]
struct RtmpSessionMetrics {
    metrics: Option<Arc<crate::metrics::MetricsRegistry>>,
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
    #[cfg(feature = "metrics")] metrics: Option<Arc<crate::metrics::MetricsRegistry>>,
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
                        video_seq_header,
                        video_codec,
                        has_video,
                        base_timestamp,
                        ..
                    } = &mut state
                        && let Some(video_data) = flv::parse_video_tag(&msg.payload)
                    {
                        match video_data {
                            VideoData::AvcSequenceHeader(data) => {
                                *video_seq_header = Some(data.clone());
                                if let Ok(mut headers) = mount_handle.inner.rtmp_seq_headers.lock()
                                {
                                    headers.insert(0, data);
                                }
                                *video_codec = Some(Codec::H264);
                                *has_video = true;
                            }
                            VideoData::HevcSequenceHeader(data) => {
                                *video_seq_header = Some(data.clone());
                                if let Ok(mut headers) = mount_handle.inner.rtmp_seq_headers.lock()
                                {
                                    headers.insert(0, data);
                                }
                                *video_codec = Some(Codec::H265);
                                *has_video = true;
                            }
                            VideoData::AvcNalu {
                                data,
                                is_keyframe,
                                composition_time,
                            } => {
                                let base = *base_timestamp.get_or_insert(msg.timestamp);
                                let ts_ms = msg.timestamp.wrapping_sub(base);
                                let pts_ms = (ts_ms as i64 + composition_time as i64).max(0) as u32;

                                let unit = AccessUnit {
                                    track_id: 0,
                                    codec: Codec::H264,
                                    pts: Duration::from_millis(pts_ms as u64),
                                    dts: Some(Duration::from_millis(ts_ms as u64)),
                                    rtp_timestamp: ts_ms.wrapping_mul(90),
                                    is_keyframe,
                                    data,
                                };
                                let _ = publisher.push(unit);
                            }
                            VideoData::HevcNalu {
                                data,
                                is_keyframe,
                                composition_time,
                            } => {
                                let base = *base_timestamp.get_or_insert(msg.timestamp);
                                let ts_ms = msg.timestamp.wrapping_sub(base);
                                let pts_ms = (ts_ms as i64 + composition_time as i64).max(0) as u32;

                                let unit = AccessUnit {
                                    track_id: 0,
                                    codec: Codec::H265,
                                    pts: Duration::from_millis(pts_ms as u64),
                                    dts: Some(Duration::from_millis(ts_ms as u64)),
                                    rtp_timestamp: ts_ms.wrapping_mul(90),
                                    is_keyframe,
                                    data,
                                };
                                let _ = publisher.push(unit);
                            }
                        }
                    }
                }
                t if t == MessageType::Audio as u8 => {
                    if let SessionState::Publishing {
                        publisher,
                        mount_handle,
                        audio_seq_header,
                        has_audio,
                        has_video,
                        base_timestamp,
                        ..
                    } = &mut state
                        && let Some(audio_data) = flv::parse_audio_tag(&msg.payload)
                    {
                        match audio_data {
                            AudioData::AacSequenceHeader(data) => {
                                *audio_seq_header = Some(data.clone());
                                let track_id: u8 = if *has_video { 1 } else { 0 };
                                if let Ok(mut headers) = mount_handle.inner.rtmp_seq_headers.lock()
                                {
                                    headers.insert(track_id, data);
                                }
                                *has_audio = true;
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
                                    rtp_timestamp: ((ts_ms as u64 * 44100) / 1000) as u32,
                                    is_keyframe: true,
                                    data,
                                };
                                let _ = publisher.push(unit);
                            }
                        }
                    }
                }
                t if t == MessageType::DataAmf0 as u8 || t == MessageType::DataAmf3 as u8 => {
                    debug!(?peer, "RTMP: received metadata");
                }
                _ => {}
            }
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
        } => Some(mount_handle.path.clone()),
        _ => None,
    };
    drop(state); // releases the LocalPublisher
    if let Some(path) = registered_path
        && let Some(handle) = mounts.get_handle(&path)
        && handle
            .inner
            .publishers
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
    {
        mounts.unregister(&path);
        debug!(?peer, "RTMP: unregistered publisher mount {path}");
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    cmd: &RtmpCommand,
    state: &mut SessionState,
    mounts: &Arc<MountRegistry>,
    config: &RtmpSessionConfig,
    msg_stream_id: &mut u32,
    tx: &mpsc::Sender<RtmpMessage>,
    subscriber_task: &mut Option<tokio::task::JoinHandle<()>>,
    cancel: &CancellationToken,
    #[cfg(feature = "metrics")] metrics: &Option<Arc<crate::metrics::MetricsRegistry>>,
) -> Option<Vec<RtmpMessage>> {
    match cmd {
        RtmpCommand::Connect {
            transaction_id,
            app,
            ..
        } => {
            let app = if app.is_empty() {
                config.default_app.clone()
            } else {
                app.clone()
            };

            *state = SessionState::Connected { app: app.clone() };

            // Send protocol control messages + connect result
            let mut responses = Vec::new();

            // Set chunk size
            responses.push(set_chunk_size_message(config.chunk_size));

            // Window ack size
            responses.push(window_ack_size_message(config.window_ack_size));

            // Set peer bandwidth
            responses.push(set_peer_bandwidth_message(config.window_ack_size, 2));

            // Connect result on chunk stream 3, msg stream 0
            let result_payload = command::connect_result(*transaction_id);
            responses.push(RtmpMessage {
                chunk_stream_id: 3,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 0,
                payload: result_payload,
            });

            info!("RTMP: connect app={app}");
            Some(responses)
        }

        RtmpCommand::CreateStream { transaction_id } => {
            let stream_id = *msg_stream_id;
            *msg_stream_id = msg_stream_id.wrapping_add(1);

            let result_payload = command::create_stream_result(*transaction_id, stream_id as f64);
            Some(vec![RtmpMessage {
                chunk_stream_id: 3,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 0,
                payload: result_payload,
            }])
        }

        RtmpCommand::ReleaseStream { transaction_id, .. }
        | RtmpCommand::Fcpublish { transaction_id, .. } => {
            // FMS-era housekeeping commands: acknowledge them with a null
            // _result, otherwise strict SDKs wait on the transaction forever.
            Some(vec![RtmpMessage {
                chunk_stream_id: 5,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 1,
                payload: command::null_result(*transaction_id),
            }])
        }

        RtmpCommand::Publish {
            transaction_id,
            stream_name,
            ..
        } => {
            let app = match &*state {
                SessionState::Connected { app } => app.clone(),
                _ => String::new(),
            };

            // Authenticate publish request
            if let Some(ref auth) = config.authenticator
                && !auth.authorize_publish(&app, stream_name)
            {
                warn!("RTMP: publish denied for /{app}/{stream_name}");
                let error_payload = command::error_response(
                    *transaction_id,
                    "NetStream.Publish.Denied",
                    "Authentication failed: publish not authorized.",
                );
                return Some(vec![RtmpMessage {
                    chunk_stream_id: 5,
                    timestamp: 0,
                    msg_type: MessageType::CommandAmf0 as u8,
                    msg_stream_id: 1,
                    payload: error_payload,
                }]);
            }

            let mount_path = format!("/{app}/{stream_name}");
            info!("RTMP: publish {mount_path}");

            // Create a publisher mount with a placeholder description.
            // The actual tracks will be determined from sequence headers.
            let description = MediaDescription {
                session_name: stream_name.as_str().into(),
                tracks: Vec::new(),
                #[cfg(feature = "multicast")]
                connection: None,
            };

            let mount_config = MountConfig::Publisher {
                description,
                allow_replace_after_disconnect: true,
            };

            // Register or get existing mount
            let (mount_handle, registered_here) = match mounts.get_handle(&mount_path) {
                Some(handle) => (handle, false),
                None => {
                    // We need to register via a server-like API.
                    // For now, create directly in the registry.
                    match mounts.register(mount_path.clone().into(), mount_config) {
                        Ok(handle) => (handle, true),
                        Err(e) => {
                            warn!("RTMP: failed to register mount {mount_path}: {e}");
                            let error_payload = command::error_response(
                                *transaction_id,
                                "NetStream.Publish.BadName",
                                &format!("Failed to create mount: {e}"),
                            );
                            return Some(vec![RtmpMessage {
                                chunk_stream_id: 5,
                                timestamp: 0,
                                msg_type: MessageType::CommandAmf0 as u8,
                                msg_stream_id: 1,
                                payload: error_payload,
                            }]);
                        }
                    }
                }
            };

            let publisher = match mount_handle.publisher() {
                Ok(pub_) => pub_,
                Err(e) => {
                    warn!("RTMP: failed to get publisher for {mount_path}: {e}");
                    let error_payload = command::error_response(
                        *transaction_id,
                        "NetStream.Publish.BadName",
                        &format!("Stream already published: {e}"),
                    );
                    return Some(vec![RtmpMessage {
                        chunk_stream_id: 5,
                        timestamp: 0,
                        msg_type: MessageType::CommandAmf0 as u8,
                        msg_stream_id: 1,
                        payload: error_payload,
                    }]);
                }
            };

            #[cfg(feature = "metrics")]
            if let Some(metrics) = &metrics {
                use std::sync::atomic::Ordering;
                metrics
                    .rtmp_publishers_active
                    .fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(feature = "metrics")]
            let _session_metrics = RtmpSessionMetrics {
                metrics: metrics.clone(),
                is_publisher: true,
            };

            *state = SessionState::Publishing {
                app,
                stream_name: stream_name.clone(),
                publisher,
                mount_handle,
                registered_here,
                video_seq_header: None,
                audio_seq_header: None,
                video_codec: None,
                has_video: false,
                has_audio: false,
                base_timestamp: None,
            };

            // StreamBegin user control
            let mut responses = vec![user_control_message(0, 1)]; // StreamBegin, stream 1

            // onStatus: NetStream.Publish.Start
            let status_payload = command::on_status(
                "status",
                "NetStream.Publish.Start",
                &format!("{stream_name} is now published."),
            );
            responses.push(RtmpMessage {
                chunk_stream_id: 5,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 1,
                payload: status_payload,
            });

            Some(responses)
        }

        RtmpCommand::Play {
            transaction_id,
            stream_name,
            ..
        } => {
            let app = match &*state {
                SessionState::Connected { app } => app.clone(),
                _ => String::new(),
            };

            let mount_path = format!("/{app}/{stream_name}");
            info!("RTMP: play {mount_path}");

            // Authenticate play (same policy as publish by default)
            if let Some(ref auth) = config.authenticator
                && !auth.authorize_play(&app, stream_name)
            {
                warn!("RTMP: play denied for /{app}/{stream_name}");
                let error_payload = command::error_response(
                    *transaction_id,
                    "NetStream.Play.Denied",
                    "Authentication failed: play not authorized.",
                );
                return Some(vec![RtmpMessage {
                    chunk_stream_id: 5,
                    timestamp: 0,
                    msg_type: MessageType::CommandAmf0 as u8,
                    msg_stream_id: 1,
                    payload: error_payload,
                }]);
            }

            let mount_handle = match mounts.get_handle(&mount_path) {
                Some(handle) => handle,
                None => {
                    let error_payload = command::error_response(
                        *transaction_id,
                        "NetStream.Play.StreamNotFound",
                        &format!("No such stream: {mount_path}"),
                    );
                    return Some(vec![RtmpMessage {
                        chunk_stream_id: 5,
                        timestamp: 0,
                        msg_type: MessageType::CommandAmf0 as u8,
                        msg_stream_id: 1,
                        payload: error_payload,
                    }]);
                }
            };

            #[cfg(feature = "metrics")]
            if let Some(metrics) = &metrics {
                use std::sync::atomic::Ordering;
                metrics.rtmp_players_active.fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(feature = "metrics")]
            let _session_metrics = RtmpSessionMetrics {
                metrics: metrics.clone(),
                is_publisher: false,
            };

            *state = SessionState::Playing {
                app,
                stream_name: stream_name.clone(),
                mount_handle: mount_handle.clone(),
            };

            // StreamBegin
            let mut responses = vec![user_control_message(0, 1)];

            // onStatus: NetStream.Play.Reset
            let reset_payload = command::on_status(
                "status",
                "NetStream.Play.Reset",
                &format!("Playing and resetting {stream_name}."),
            );
            responses.push(RtmpMessage {
                chunk_stream_id: 5,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 1,
                payload: reset_payload,
            });

            // onStatus: NetStream.Play.Start
            let start_payload = command::on_status(
                "status",
                "NetStream.Play.Start",
                &format!("Started playing {stream_name}."),
            );
            responses.push(RtmpMessage {
                chunk_stream_id: 5,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 1,
                payload: start_payload,
            });

            // Spawn subscriber task that forwards frames as FLV tags
            let cancel_clone = cancel.clone();
            let tx_clone = tx.clone();
            let handle = mount_handle.clone();
            let description = mount_handle.description.clone();

            let has_video = description
                .tracks
                .iter()
                .any(|t| t.kind == MediaKind::Video);
            let has_audio = description
                .tracks
                .iter()
                .any(|t| t.kind == MediaKind::Audio);

            let task = tokio::spawn(async move {
                // Send onMetaData
                let meta_payload = command::on_metadata(has_video, has_audio, 0.0);
                let _ = tx_clone
                    .send(RtmpMessage {
                        chunk_stream_id: 4,
                        timestamp: 0,
                        msg_type: MessageType::DataAmf0 as u8,
                        msg_stream_id: 1,
                        payload: meta_payload,
                    })
                    .await;

                // Subscribe with startup cache (GOP cache for fast start)
                let (mut rx, startup) = handle.subscribe_with_startup();
                // Skip broadcast replays of frames the startup snapshot
                // already delivered.
                let startup_keys = crate::mount::startup_dedup_keys(&startup);

                // Send cached sequence headers (AVC/HEVC/AAC config) before the
                // GOP so the player can initialize its decoder immediately
                // (previously play never emitted sequence headers at all).
                {
                    // Copy headers out under the lock; the MutexGuard is not
                    // Send and must not live across the send().await below.
                    let cached: Vec<(u8, Bytes, Codec)> = {
                        let headers = handle
                            .inner
                            .rtmp_seq_headers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        headers
                            .iter()
                            .filter_map(|(track_id, header)| {
                                handle.inner.codec_by_track[*track_id as usize]
                                    .map(|codec| (*track_id, header.clone(), codec))
                            })
                            .collect()
                    };
                    for (track_id, header, codec) in cached {
                        let kind = match codec {
                            crate::media::Codec::Aac
                            | crate::media::Codec::Pcma
                            | crate::media::Codec::Pcmu
                            | crate::media::Codec::Opus => crate::media::MediaKind::Audio,
                            _ => crate::media::MediaKind::Video,
                        };
                        let unit = AccessUnit {
                            track_id,
                            codec,
                            pts: Duration::ZERO,
                            dts: None,
                            rtp_timestamp: 0,
                            is_keyframe: true,
                            data: header,
                        };
                        let (msg_type, payload) = if kind == crate::media::MediaKind::Video {
                            (MessageType::Video as u8, flv::build_video_tag(&unit, true))
                        } else {
                            (MessageType::Audio as u8, flv::build_audio_tag(&unit, true))
                        };
                        let _ = tx_clone
                            .send(RtmpMessage {
                                chunk_stream_id: 5,
                                timestamp: 0,
                                msg_type,
                                msg_stream_id: 1,
                                payload,
                            })
                            .await;
                    }
                }
                // Send startup frames (sequence headers + cached GOP)
                let mut base_ts: Option<u32> = None;
                for unit in &startup {
                    // FLV tag timestamp is the DTS (decode order); PTS goes
                    // into the tag's composite-time offset. Using PTS for
                    // both double-counts the offset on B-frame streams.
                    let ts = unit.dts.unwrap_or(unit.pts).as_millis() as u32;
                    let base = *base_ts.get_or_insert(ts);
                    let rel_ts = ts.wrapping_sub(base);

                    let (msg_type, payload) = if unit.kind() == MediaKind::Video {
                        (MessageType::Video as u8, flv::build_video_tag(unit, false))
                    } else {
                        (MessageType::Audio as u8, flv::build_audio_tag(unit, false))
                    };

                    if payload.is_empty() {
                        continue;
                    }
                    let _ = tx_clone
                        .send(RtmpMessage {
                            chunk_stream_id: if unit.kind() == MediaKind::Video {
                                6
                            } else {
                                7
                            },
                            timestamp: rel_ts,
                            msg_type,
                            msg_stream_id: 1,
                            payload,
                        })
                        .await;
                }

                // Forward live frames
                loop {
                    tokio::select! {
                        _ = cancel_clone.cancelled() => break,
                        result = rx.recv() => {
                            match result {
                                Ok(unit) => {
                                    if startup_keys.contains(&(unit.track_id, unit.rtp_timestamp)) {
                                        continue;
                                    }
                                    let ts = unit.dts.unwrap_or(unit.pts).as_millis() as u32;
                                    let base = *base_ts.get_or_insert(ts);
                                    let rel_ts = ts.wrapping_sub(base);

                                    let (msg_type, payload) = if unit.kind() == MediaKind::Video {
                                        (MessageType::Video as u8, flv::build_video_tag(&unit, false))
                                    } else {
                                        (MessageType::Audio as u8, flv::build_audio_tag(&unit, false))
                                    };

                                    if payload.is_empty() {
                                        continue;
                                    }
                                    if tx_clone.send(RtmpMessage {
                                        chunk_stream_id: if unit.kind() == MediaKind::Video { 6 } else { 7 },
                                        timestamp: rel_ts,
                                        msg_type,
                                        msg_stream_id: 1,
                                        payload,
                                    }).await.is_err() {
                                        break; // client disconnected
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(_) => break,
                            }
                        }
                    }
                }
            });
            *subscriber_task = Some(task);

            Some(responses)
        }

        RtmpCommand::DeleteStream { transaction_id, .. }
        | RtmpCommand::CloseStream { transaction_id, .. } => {
            // A player that calls deleteStream/closeStream expects media to
            // stop; previously playback continued until TCP disconnect.
            if let Some(task) = subscriber_task.take() {
                task.abort();
            }
            if let SessionState::Playing { app, .. } = state {
                *state = SessionState::Connected { app: app.clone() };
            }
            Some(vec![RtmpMessage {
                chunk_stream_id: 5,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 1,
                payload: command::null_result(*transaction_id),
            }])
        }

        RtmpCommand::MetaData { .. } => None,

        RtmpCommand::GetStreamLength { transaction_id, .. } => {
            // Return 0 for live streams
            let result = command::null_result(*transaction_id);
            Some(vec![RtmpMessage {
                chunk_stream_id: 3,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 0,
                payload: result,
            }])
        }

        RtmpCommand::Generic { name, .. } => {
            debug!("RTMP: unhandled command: {name}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_key_auth_open_mode() {
        let auth = StreamKeyAuthenticator::default();
        // Empty list = allow all
        assert!(auth.authorize_publish("live", "stream1"));
        assert!(auth.authorize_publish("any", "thing"));
    }

    #[test]
    fn stream_key_auth_restricts() {
        let auth =
            StreamKeyAuthenticator::new(vec!["live/stream1".to_string(), "live/cam2".to_string()]);
        assert!(auth.authorize_publish("live", "stream1"));
        assert!(auth.authorize_publish("live", "cam2"));
        assert!(!auth.authorize_publish("live", "unknown"));
        assert!(!auth.authorize_publish("other", "stream1"));
    }

    #[test]
    fn stream_key_auth_allow_method() {
        let mut auth = StreamKeyAuthenticator::default();
        auth.allow("live/test");
        assert!(auth.authorize_publish("live", "test"));
        assert!(!auth.authorize_publish("live", "other"));
    }
}
