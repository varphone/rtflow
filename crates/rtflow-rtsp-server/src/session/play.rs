use super::*;
use crate::TimelineMode;
use rtflow_media::TrackDescription;

fn timeline_ntp_anchor(state: &State, timeline: TimelineMode) -> Option<u64> {
    let seconds = match timeline {
        TimelineMode::Relative | TimelineMode::Unix => return None,
        TimelineMode::ServerUptime => state.timeline_epoch.elapsed(),
        TimelineMode::Absolute { offset } => offset + state.timeline_epoch.elapsed(),
        // `TimelineMode` is `#[non_exhaustive]`: unknown modes have no anchor.
        _ => return None,
    };
    const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
    let secs = seconds.as_secs().saturating_add(NTP_UNIX_OFFSET);
    let frac = (u64::from(seconds.subsec_nanos()) << 32) / 1_000_000_000;
    Some((secs << 32) | frac)
}

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
    multicast_tracks: &[(Arc<Mount>, TrackDescription)],
    resources: &mut ConnectionResources,
    rtp_sequences: &mut HashMap<u8, Arc<AtomicU16>>,
    vod_cursor: &mut std::time::Duration,
    phase: &mut ConnectionPhase,
    vod_end: &Arc<tokio::sync::Notify>,
) -> Result<()> {
    // Scale/Speed are optional RTSP 1.0 extensions. Only normal speed is
    // supported; answer 551 instead of silently ignoring a request the
    // client expects to change playback.
    for header in ["scale", "speed"] {
        if let Some(value) = req.headers.get(header)
            && value.trim().parse::<f64>().ok() != Some(1.0)
        {
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&simple_response(
                    551,
                    "Option Not Supported",
                    cseq,
                )))
                .await?;
            return Ok(());
        }
    }
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
    let timeline_mode = interleaved
        .iter()
        .map(|(mount, ..)| mount.timeline)
        .chain(setups.iter().map(|(mount, ..)| mount.timeline))
        .chain(multicast_tracks.iter().map(|(mount, _)| mount.timeline))
        .flatten()
        .next()
        .unwrap_or(state.config.timeline);
    let timeline_ntp_anchor = timeline_ntp_anchor(state, timeline_mode);
    let timeline_position = match timeline_mode {
        TimelineMode::Relative => None,
        TimelineMode::ServerUptime => Some(NptPosition::Time(state.timeline_epoch.elapsed())),
        TimelineMode::Unix => Some(NptPosition::Time(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default(),
        )),
        TimelineMode::Absolute { offset } => {
            Some(NptPosition::Time(offset + state.timeline_epoch.elapsed()))
        }
        // `TimelineMode` is `#[non_exhaustive]`.
        _ => None,
    };
    let start = requested_range
        .map(|range| match range.start {
            NptPosition::Time(position) => position,
            NptPosition::Now => *vod_cursor,
        })
        .unwrap_or(*vod_cursor);
    let end = requested_range.and_then(|range| range.end);
    let vod_output = vod_source.as_ref().map(|_| {
        rtflow_mount::FrameBus::new(
            state.config.consumer_queue_capacity,
            state.config.resource_limits.max_queued_bytes_per_session,
        )
    });
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
    if interleaved.is_empty() && setups.is_empty() && multicast_tracks.is_empty() {
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
        tracing::info!(
            session = %session_id,
            uri = %req.uri,
            udp_tracks = setups.len(),
            interleaved_tracks = interleaved.len(),
            multicast_tracks = multicast_tracks.len(),
            vod = vod_source.is_some(),
            "RTSP PLAY started"
        );
        resources.register_play(
            interleaved
                .iter()
                .map(|(mount, _, _, _)| mount)
                .chain(setups.iter().map(|(mount, _, _, _, _)| mount))
                .chain(multicast_tracks.iter().map(|(mount, _)| mount)),
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
                timeline_position
                    .map(|start| NptRange { start, end: None })
                    .or(requested_range)
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
            let rtx = state
                .config
                .rtx
                .then(|| {
                    let description = mount.description_snapshot();
                    rtflow_media::rtx_configs(&description)
                        .get(&track.id)
                        .copied()
                })
                .flatten();
            resources.senders.push(if let Some(output) = &vod_output {
                spawn_interleaved_sender_from(
                    mount.clone(),
                    SenderInput {
                        receiver: output.subscribe(),
                        startup: Vec::new(),
                        sequence,
                        crypto: MediaCrypto::from_track(&track)?,
                    },
                    rtp_channel,
                    rtcp_channel,
                    track,
                    writer.clone(),
                    timeline_ntp_anchor,
                    state.config.rtcp_report_interval,
                    rtx,
                )
            } else {
                let mut input = live_interleaved_inputs.remove(0);
                input.crypto = MediaCrypto::from_track(&track)?;
                spawn_interleaved_sender_from(
                    mount.clone(),
                    input,
                    rtp_channel,
                    rtcp_channel,
                    track,
                    writer.clone(),
                    timeline_ntp_anchor,
                    state.config.rtcp_report_interval,
                    rtx,
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
            let rtx = state
                .config
                .rtx
                .then(|| {
                    let description = mount.description_snapshot();
                    rtflow_media::rtx_configs(&description)
                        .get(&track.id)
                        .copied()
                })
                .flatten();
            resources.senders.push(if let Some(output) = &vod_output {
                spawn_udp_sender_from(
                    mount.clone(),
                    SenderInput {
                        receiver: output.subscribe(),
                        startup: Vec::new(),
                        sequence,
                        crypto: MediaCrypto::default(),
                    },
                    socket,
                    rtp_target,
                    rtcp_target,
                    track,
                    None,
                    state.config.rtcp_report_interval,
                    rtx,
                )
            } else {
                let mut input = live_udp_inputs.remove(0);
                input.crypto = MediaCrypto::from_track(&track)?;
                spawn_udp_sender_from(
                    mount.clone(),
                    input,
                    socket,
                    rtp_target,
                    rtcp_target,
                    track,
                    timeline_ntp_anchor,
                    state.config.rtcp_report_interval,
                    rtx,
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
                vod_end.clone(),
            )?);
        }
    }
    Ok(())
}
