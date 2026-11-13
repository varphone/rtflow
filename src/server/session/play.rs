use super::*;
use crate::media::TrackDescription;

/// PLAY: start senders for the negotiated tracks (live or VOD).
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_play(
    req: &rtsp::Request,
    state: &Arc<State>,
    writer: &ConnWriter,
    session_id: &str,
    cseq: String,
    setups: &[(
        Arc<Mount>,
        UdpPair,
        SocketAddr,
        SocketAddr,
        TrackDescription,
    )],
    interleaved: &[(Arc<Mount>, u8, u8, TrackDescription)],
    resources: &mut ConnectionResources,
    rtp_sequences: &mut HashMap<u8, Arc<AtomicU16>>,
    vod_cursor: &mut std::time::Duration,
    phase: &mut ConnectionPhase,
) -> Result<()> {
    #[cfg(feature = "file-storage")]
    let vod_source = interleaved
        .iter()
        .map(|(mount, ..)| mount)
        .chain(setups.iter().map(|(mount, ..)| mount))
        .find_map(|mount| match &mount.config {
            MountConfig::Vod { source } => Some(source.clone()),
            _ => None,
        });
    let requested_range = match req.headers.get("range") {
        Some(value) => match NptRange::parse(value) {
            Ok(range) => Some(range),
            Err(_) => {
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(
                        457,
                        "Invalid Range",
                        cseq,
                    )))
                    .await?;
                return Ok(());
            }
        },
        None => None,
    };
    let live_range_supported = requested_range.is_none_or(|range| {
                    matches!(range.start, NptPosition::Now)
                        || matches!(range.start, NptPosition::Time(position) if position == std::time::Duration::ZERO)
                }) && requested_range.is_none_or(|range| range.end.is_none());
    if vod_source.is_none() && !live_range_supported {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                457,
                "Invalid Range",
                cseq,
            )))
            .await?;
        return Ok(());
    }
    for task in resources.senders.drain(..) {
        task.abort();
        // Wait for the old sender to actually stop before a new
        // one starts, so the two never interleave on the wire.
        let _ = task.await;
    }
    if let Some(position) = resources.stop_vod() {
        *vod_cursor = position;
    }
    let start = requested_range
        .map(|range| match range.start {
            NptPosition::Time(position) => position,
            NptPosition::Now => *vod_cursor,
        })
        .unwrap_or(*vod_cursor);
    let end = requested_range.and_then(|range| range.end);
    let vod_output = vod_source
        .as_ref()
        .map(|_| tokio::sync::broadcast::channel(state.config.consumer_queue_capacity).0);
    let mut live_interleaved_inputs = Vec::new();
    let mut live_udp_inputs = Vec::new();
    let mut rtp_info = Vec::new();
    if vod_output.is_none() {
        for (mount, _, _, track) in interleaved {
            let sequence = rtp_sequences
                .entry(track.id)
                .or_insert_with(|| Arc::new(AtomicU16::new(0)))
                .clone();
            let input = live_sender_input(mount, track.id, sequence);
            if let Some(entry) = rtp_info_entry(mount, &req.uri, track, &input) {
                rtp_info.push(entry);
            }
            live_interleaved_inputs.push(input);
        }
        for (mount, _, _, _, track) in setups {
            let sequence = rtp_sequences
                .entry(track.id)
                .or_insert_with(|| Arc::new(AtomicU16::new(0)))
                .clone();
            let input = live_sender_input(mount, track.id, sequence);
            if let Some(entry) = rtp_info_entry(mount, &req.uri, track, &input) {
                rtp_info.push(entry);
            }
            live_udp_inputs.push(input);
        }
    }
    if interleaved.is_empty() && setups.is_empty() {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                455,
                "Method Not Valid in This State",
                cseq,
            )))
            .await?;
    } else {
        *phase = ConnectionPhase::Playing;
        resources.register_play(
            interleaved
                .iter()
                .map(|(mount, _, _, _)| mount)
                .chain(setups.iter().map(|(mount, _, _, _, _)| mount)),
        );
        let mut response = simple_response(200, "OK", cseq);
        response.headers.insert(
            "session".into(),
            session_header(session_id, state.config.session_timeout),
        );
        if vod_source.is_some() {
            response.headers.insert(
                "range".into(),
                NptRange {
                    start: NptPosition::Time(start),
                    end,
                }
                .render(),
            );
        } else {
            // A live presentation has no finite endpoint, but PLAY still
            // confirms the effective Normal Play Time range. This lets
            // RTSP clients distinguish a live stream from a response
            // that omitted range semantics altogether.
            response.headers.insert(
                "range".into(),
                requested_range
                    .unwrap_or(NptRange {
                        start: NptPosition::Now,
                        end: None,
                    })
                    .render(),
            );
        }
        if !rtp_info.is_empty() {
            response
                .headers
                .insert("rtp-info".into(), rtp_info.join(","));
        }
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&response))
            .await?;
        for (mount, rtp_channel, rtcp_channel, track) in interleaved.iter().cloned() {
            let sequence = rtp_sequences
                .entry(track.id)
                .or_insert_with(|| Arc::new(AtomicU16::new(0)))
                .clone();
            resources.senders.push(if let Some(output) = &vod_output {
                spawn_interleaved_sender_from(
                    mount.clone(),
                    SenderInput {
                        receiver: output.subscribe(),
                        startup: Vec::new(),
                        sequence,
                    },
                    rtp_channel,
                    rtcp_channel,
                    track,
                    writer.clone(),
                )
            } else {
                spawn_interleaved_sender_from(
                    mount.clone(),
                    live_interleaved_inputs.remove(0),
                    rtp_channel,
                    rtcp_channel,
                    track,
                    writer.clone(),
                )
            });
            start_proxy_if_idle(
                mount,
                state.cancel.clone(),
                state.supervisor.clone(),
                state.config.no_subscriber_grace,
            );
        }
        for (mount, socket, rtp_target, rtcp_target, track) in setups.iter().cloned() {
            let sequence = rtp_sequences
                .entry(track.id)
                .or_insert_with(|| Arc::new(AtomicU16::new(0)))
                .clone();
            resources.senders.push(if let Some(output) = &vod_output {
                spawn_udp_sender_from(
                    mount.clone(),
                    SenderInput {
                        receiver: output.subscribe(),
                        startup: Vec::new(),
                        sequence,
                    },
                    socket,
                    rtp_target,
                    rtcp_target,
                    track,
                )
            } else {
                spawn_udp_sender_from(
                    mount.clone(),
                    live_udp_inputs.remove(0),
                    socket,
                    rtp_target,
                    rtcp_target,
                    track,
                )
            });
            start_proxy_if_idle(
                mount,
                state.cancel.clone(),
                state.supervisor.clone(),
                state.config.no_subscriber_grace,
            );
        }
        if let (Some(source), Some(output)) = (vod_source, vod_output) {
            resources.vod = Some(spawn_vod_playback(
                &state.supervisor,
                source,
                output,
                start,
                end,
                state.config.consumer_queue_capacity,
            )?);
        }
    }
    Ok(())
}
