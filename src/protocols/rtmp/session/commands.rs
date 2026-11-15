use super::*;
use crate::rtmp::amf::AmfValue;

fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left.max(1)
}

#[allow(clippy::too_many_arguments)]
fn video_fmtp(codec: Codec, config: Option<&[u8]>) -> Option<String> {
    let config = config?;
    match codec {
        Codec::H264 => {
            let params = crate::rtmp::flv::extract_sps_pps_from_avcc_config(config)?;
            if params.len() < 2 {
                return None;
            }
            use base64::{Engine, engine::general_purpose::STANDARD};
            Some(format!(
                "profile-level-id=42e01e;sprop-parameter-sets={},{}",
                STANDARD.encode(&params[0]),
                STANDARD.encode(&params[1])
            ))
        }
        Codec::H265 => {
            let (vps, sps, pps) = h265_parameter_sets_from_hvcc(config)?;
            Some(crate::media::bitstream::h265_fmtp(&vps, &sps, &pps))
        }
        _ => None,
    }
}

/// Extract VPS/SPS/PPS from a HEVCDecoderConfigurationRecord (`hvcC`).
fn h265_parameter_sets_from_hvcc(config: &[u8]) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    if config.len() < 23 || config[0] != 1 {
        return None;
    }
    let num_arrays = config[22] as usize;
    let mut pos = 23usize;
    let mut vps = None;
    let mut sps = None;
    let mut pps = None;
    for _ in 0..num_arrays {
        if pos + 3 > config.len() {
            return None;
        }
        let nal_type = config[pos] & 0x3f;
        pos += 1;
        let count = u16::from_be_bytes([config[pos], config[pos + 1]]) as usize;
        pos += 2;
        for _ in 0..count {
            if pos + 2 > config.len() {
                return None;
            }
            let len = u16::from_be_bytes([config[pos], config[pos + 1]]) as usize;
            pos += 2;
            if pos + len > config.len() {
                return None;
            }
            let nal = config[pos..pos + len].to_vec();
            pos += len;
            match nal_type {
                32 => vps = Some(nal),
                33 => sps = Some(nal),
                34 => pps = Some(nal),
                _ => {}
            }
        }
    }
    Some((vps?, sps?, pps?))
}

#[allow(clippy::too_many_arguments)] // protocol metadata fields stay explicit
pub(super) fn sync_description(
    mount_handle: &MountHandle,
    stream_name: &str,
    video_codec: Option<Codec>,
    has_video: bool,
    has_audio: bool,
    width: Option<u32>,
    height: Option<u32>,
    frame_rate: Option<(u32, u32)>,
    audio_sample_rate: Option<u32>,
    audio_channels: Option<u8>,
    video_config: Option<&[u8]>,
) -> crate::Result<()> {
    if !has_video && !has_audio {
        return Ok(());
    }
    let video_id = 0u8;
    let audio_id = if has_video { 1 } else { 0 };
    let mut tracks = Vec::new();
    if has_video {
        tracks.push(crate::media::TrackDescription {
            id: video_id,
            kind: crate::media::MediaKind::Video,
            codec: video_codec.unwrap_or(Codec::H264),
            clock_rate: 90_000,
            channels: None,
            payload_type: 96,
            fmtp: video_fmtp(video_codec.unwrap_or(Codec::H264), video_config).map(Arc::from),
            control: Arc::from("trackID=0"),
            width,
            height,
            frame_rate,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
            rtx: None,
        });
    }
    if has_audio {
        tracks.push(crate::media::TrackDescription {
            id: audio_id,
            kind: crate::media::MediaKind::Audio,
            codec: Codec::Aac,
            clock_rate: audio_sample_rate.unwrap_or(48_000),
            channels: Some(audio_channels.unwrap_or(2).max(1)),
            payload_type: 97,
            fmtp: None,
            control: Arc::from(format!("trackID={audio_id}")),
            width: None,
            height: None,
            frame_rate: None,
            direction: None,
            ptime: None,
            rtcp: None,
            bandwidth_as: None,
            #[cfg(feature = "srtp")]
            crypto: None,
            rtx: None,
        });
    }
    mount_handle
        .inner
        .update_description(crate::media::MediaDescription::new(stream_name, tracks))
}

pub(crate) fn split_stream_token<'a>(
    stream_name: &'a str,
    token: &crate::rtsp::token_auth::TokenConfig,
) -> (&'a str, Option<&'a str>) {
    let Some((name, query)) = stream_name.split_once('?') else {
        return (stream_name, None);
    };
    let value = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == token.param_name).then_some(value)
    });
    (name, value)
}

pub(crate) fn token_allows(
    token: Option<&crate::rtsp::token_auth::TokenConfig>,
    app: &str,
    stream: &str,
    value: Option<&str>,
) -> bool {
    let Some(token) = token else {
        return true;
    };
    let Some(value) = value else {
        return false;
    };
    crate::rtsp::token_auth::verify_token(token, &format!("/{app}/{stream}"), value)
}

pub(crate) fn apply_metadata(state: &mut SessionState, data: &AmfValue) {
    let SessionState::Publishing {
        mount_handle,
        video_codec,
        has_video,
        has_audio,
        width,
        height,
        frame_rate,
        audio_sample_rate,
        audio_channels,
        video_seq_header,
        stream_name,
        ..
    } = state
    else {
        return;
    };
    if let Some(value) = data.get("width").and_then(|v| v.as_number()) {
        *width = Some(value.max(0.0) as u32);
    }
    if let Some(value) = data.get("height").and_then(|v| v.as_number()) {
        *height = Some(value.max(0.0) as u32);
    }
    if let Some(value) = data.get("framerate").and_then(|v| v.as_number())
        && value > 0.0
    {
        let numerator = (value * 1_000.0).round() as u32;
        let divisor = gcd(numerator, 1_000);
        *frame_rate = Some((numerator / divisor, 1_000 / divisor));
    }
    if let Some(value) = data.get("audiosamplerate").and_then(|v| v.as_number())
        && value > 0.0
    {
        *audio_sample_rate = Some(value as u32);
    }
    if let Some(stereo) = data.get("stereo").and_then(|v| v.as_bool()) {
        *audio_channels = Some(if stereo { 2 } else { 1 });
    }
    let _ = sync_description(
        mount_handle,
        stream_name,
        *video_codec,
        *has_video,
        *has_audio,
        *width,
        *height,
        *frame_rate,
        *audio_sample_rate,
        *audio_channels,
        video_seq_header.as_deref(),
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_command(
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

            let (stream_name, token_value) = match config.token.as_ref() {
                Some(token) => split_stream_token(stream_name, token),
                None => (stream_name.as_str(), None),
            };
            if !token_allows(config.token.as_ref(), &app, stream_name, token_value) {
                warn!("RTMP: publish denied by URL token for /{app}/{stream_name}");
                let error_payload = command::error_response(
                    *transaction_id,
                    "NetStream.Publish.Denied",
                    "Authentication failed: invalid token.",
                );
                return Some(vec![RtmpMessage {
                    chunk_stream_id: 5,
                    timestamp: 0,
                    msg_type: MessageType::CommandAmf0 as u8,
                    msg_stream_id: 1,
                    payload: error_payload,
                }]);
            }
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
                session_name: stream_name.into(),
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
                stream_name: stream_name.to_string(),
                publisher,
                mount_handle,
                registered_here,
                video_seq_header: None,
                audio_seq_header: None,
                video_codec: None,
                has_video: false,
                has_audio: false,
                width: None,
                height: None,
                frame_rate: None,
                audio_sample_rate: None,
                audio_channels: None,
                base_timestamp: None,
                drop_streak: 0,
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

            let (stream_name, token_value) = match config.token.as_ref() {
                Some(token) => split_stream_token(stream_name, token),
                None => (stream_name.as_str(), None),
            };
            if !token_allows(config.token.as_ref(), &app, stream_name, token_value) {
                warn!("RTMP: play denied by URL token for /{app}/{stream_name}");
                let error_payload = command::error_response(
                    *transaction_id,
                    "NetStream.Play.Denied",
                    "Authentication failed: invalid token.",
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
                stream_name: stream_name.to_string(),
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
            let description = mount_handle.description();

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
                                handle
                                    .inner
                                    .codec_by_track
                                    .read()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    [*track_id as usize]
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
                let mut awaiting_key = false;
                loop {
                    tokio::select! {
                        _ = cancel_clone.cancelled() => break,
                        result = rx.recv() => {
                            match result {
                                Ok(unit) => {
                                    if startup_keys.contains(&(unit.track_id, unit.rtp_timestamp)) {
                                        continue;
                                    }
                                    if awaiting_key && unit.kind() == MediaKind::Video {
                                        if !unit.is_keyframe {
                                            continue;
                                        }
                                        awaiting_key = false;
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
                                Err(crate::mount::FrameRecvError::Lagged(_)) => {
                                    awaiting_key = true;
                                    continue;
                                }
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

        RtmpCommand::MetaData { data, .. } => {
            if let Some(data) = data {
                apply_metadata(state, data);
            }
            None
        }

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
