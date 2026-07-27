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
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::media::{AccessUnit, Codec, MediaDescription};
use crate::mount::{LocalPublisher, MountConfig, MountHandle, MountRegistry};
use crate::{Error, Result};

/// RTMP session configuration.
#[derive(Clone, Debug)]
pub struct RtmpSessionConfig {
    /// Default application name if not specified in connect.
    pub default_app: String,
    /// Chunk size to negotiate (outgoing).
    pub chunk_size: u32,
    /// Window acknowledgement size.
    pub window_ack_size: u32,
}

impl Default for RtmpSessionConfig {
    fn default() -> Self {
        Self {
            default_app: "live".to_string(),
            chunk_size: 4096,
            window_ack_size: 2_500_000,
        }
    }
}

/// Session state.
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
pub async fn serve_rtmp_session(
    mut stream: TcpStream,
    mounts: Arc<MountRegistry>,
    config: RtmpSessionConfig,
    cancel: CancellationToken,
) -> Result<()> {
    let peer = stream.peer_addr().ok();
    debug!(?peer, "RTMP: new connection");

    // Handshake
    if let Err(e) = handshake::server_handshake(&mut stream).await {
        debug!(?peer, "RTMP: handshake failed: {e}");
        return Err(Error::Rtmp(format!("handshake failed: {e}")));
    }
    debug!(?peer, "RTMP: handshake complete");

    let mut dechunker = Dechunker::new();
    let mut chunker = Chunker::new();
    let mut state = SessionState::Init;
    let mut msg_stream_id: u32 = 1;
    let mut read_buf = vec![0u8; 8192];
    let mut subscriber_task: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!(?peer, "RTMP: server shutdown");
                break;
            }
            result = stream.read(&mut read_buf) => {
                let n = match result {
                    Ok(0) => {
                        debug!(?peer, "RTMP: connection closed by peer");
                        break;
                    }
                    Ok(n) => n,
                    Err(e) => {
                        debug!(?peer, "RTMP: read error: {e}");
                        break;
                    }
                };
                dechunker.feed(&read_buf[..n]);
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
                t if t == MessageType::CommandAmf0 as u8 || t == MessageType::CommandAmf3 as u8 =>
                {
                    let payload = if t == MessageType::CommandAmf3 as u8 && !msg.payload.is_empty()
                    {
                        // AMF3 command: skip first byte (0x00)
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
                            &mut chunker,
                            &mut subscriber_task,
                            &cancel,
                        )
                        .await;

                        if let Some(response_msgs) = response {
                            for resp in response_msgs {
                                let encoded = chunker.encode(&resp);
                                if stream.write_all(&encoded).await.is_err() {
                                    return Ok(());
                                }
                            }
                            let _ = stream.flush().await;
                        }
                    }
                }
                t if t == MessageType::Video as u8 => {
                    if let SessionState::Publishing {
                        publisher,
                        video_seq_header,
                        video_codec,
                        has_video,
                        base_timestamp,
                        ..
                    } = &mut state
                    {
                        if let Some(video_data) = flv::parse_video_tag(&msg.payload) {
                            match video_data {
                                VideoData::AvcSequenceHeader(data) => {
                                    *video_seq_header = Some(data);
                                    *video_codec = Some(Codec::H264);
                                    *has_video = true;
                                }
                                VideoData::HevcSequenceHeader(data) => {
                                    *video_seq_header = Some(data);
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
                                    let pts_ms =
                                        (ts_ms as i64 + composition_time as i64).max(0) as u32;

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
                                    let pts_ms =
                                        (ts_ms as i64 + composition_time as i64).max(0) as u32;

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
                }
                t if t == MessageType::Audio as u8 => {
                    if let SessionState::Publishing {
                        publisher,
                        audio_seq_header,
                        has_audio,
                        has_video,
                        base_timestamp,
                        ..
                    } = &mut state
                    {
                        if let Some(audio_data) = flv::parse_audio_tag(&msg.payload) {
                            match audio_data {
                                AudioData::AacSequenceHeader(data) => {
                                    *audio_seq_header = Some(data);
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
                                        rtp_timestamp: ts_ms.wrapping_mul(44100 / 1000),
                                        is_keyframe: true,
                                        data,
                                    };
                                    let _ = publisher.push(unit);
                                }
                            }
                        }
                    }
                }
                t if t == MessageType::DataAmf0 as u8 || t == MessageType::DataAmf3 as u8 => {
                    // onMetaData - we can extract width/height/framerate but it's optional
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

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    cmd: &RtmpCommand,
    state: &mut SessionState,
    mounts: &Arc<MountRegistry>,
    config: &RtmpSessionConfig,
    msg_stream_id: &mut u32,
    chunker: &mut Chunker,
    subscriber_task: &mut Option<tokio::task::JoinHandle<()>>,
    cancel: &CancellationToken,
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
            chunker.set_chunk_size(config.chunk_size);
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
            *msg_stream_id += 1;

            let result_payload = command::create_stream_result(*transaction_id, stream_id as f64);
            Some(vec![RtmpMessage {
                chunk_stream_id: 3,
                timestamp: 0,
                msg_type: MessageType::CommandAmf0 as u8,
                msg_stream_id: 0,
                payload: result_payload,
            }])
        }

        RtmpCommand::ReleaseStream { .. } | RtmpCommand::Fcpublish { .. } => {
            // These are FMS-specific, respond with null result
            Some(vec![])
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
            let mount_handle = match mounts.get_handle(&mount_path) {
                Some(handle) => handle,
                None => {
                    // We need to register via a server-like API.
                    // For now, create directly in the registry.
                    match mounts.register(mount_path.clone().into(), mount_config) {
                        Ok(handle) => handle,
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

            *state = SessionState::Publishing {
                app,
                stream_name: stream_name.clone(),
                publisher,
                mount_handle,
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

            // Spawn subscriber task to forward frames
            // Note: actual frame forwarding would need access to the write side
            // This is a simplified version - in production you'd use a channel
            let cancel_clone = cancel.clone();
            let handle = mount_handle.clone();
            let task = tokio::spawn(async move {
                let mut rx = handle.subscribe();
                loop {
                    tokio::select! {
                        _ = cancel_clone.cancelled() => break,
                        result = rx.recv() => {
                            match result {
                                Ok(_unit) => {
                                    // Frame forwarding handled elsewhere
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

        RtmpCommand::DeleteStream { .. } | RtmpCommand::CloseStream { .. } => {
            // Cleanup handled on disconnect
            Some(vec![])
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
