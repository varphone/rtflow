use super::*;
use rtflow_media::TrackDescription;

/// RECORD: claim publisher mounts and start receive tasks.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_record(
    _req: &rtsp::Request,
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
    depacketizers: &mut HashMap<u8, Depacketizer>,
    ssrcs: &mut HashMap<u8, u32>,
    vod_cursor: &mut std::time::Duration,
    phase: &mut ConnectionPhase,
    publisher_activity: &Arc<std::sync::atomic::AtomicU64>,
) -> Result<()> {
    let candidates = setups
        .iter()
        .map(|(mount, ..)| mount.clone())
        .chain(interleaved.iter().map(|(mount, ..)| mount.clone()))
        .collect::<Vec<_>>();
    match claim_remote_publishers(candidates) {
        Ok(claims) => resources.claims = claims,
        Err(_) => {
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&simple_response(
                    409, "Conflict", cseq,
                )))
                .await?;
            return Ok(());
        }
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
    for (mount, socket, rtp_target, rtcp_target, track) in setups.iter().cloned() {
        if matches!(mount.config, MountConfig::Publisher { .. }) {
            resources.senders.push(spawn_udp_receiver(
                mount,
                socket,
                rtp_target,
                rtcp_target,
                track.clone(),
                MediaCrypto::from_track(&track)?,
                state.config.rtcp_report_interval,
                publisher_activity.clone(),
            ));
        }
    }
    let mut has_interleaved_receiver = false;
    ssrcs.clear();
    for (mount, rtp_channel, _rtcp_channel, track) in interleaved.iter().cloned() {
        if matches!(mount.config, MountConfig::Publisher { .. }) {
            has_interleaved_receiver = true;
            depacketizers.insert(
                rtp_channel,
                Depacketizer::new(track.codec, track.id, track.clock_rate),
            );
            if !resources
                .recording_mounts
                .iter()
                .any(|current| Arc::ptr_eq(current, &mount))
            {
                mount.publishers.fetch_add(1, Ordering::AcqRel);
                resources.recording_mounts.push(mount);
            }
        }
    }
    if resources.senders.is_empty() && !has_interleaved_receiver {
        release_remote_publishers(&mut resources.claims, true);
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
        *phase = ConnectionPhase::Recording;
        let mut response = simple_response(200, "OK", cseq);
        response.headers.insert(
            "session".into(),
            session_header(session_id, state.config.session_timeout),
        );
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&response))
            .await?;
    }
    Ok(())
}
