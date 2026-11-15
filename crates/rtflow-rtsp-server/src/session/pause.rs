use super::*;

/// PAUSE: stop active senders and release publisher claims.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_pause(
    writer: &ConnWriter,
    cseq: String,
    resources: &mut ConnectionResources,
    depacketizers: &mut HashMap<u8, Depacketizer>,
    vod_cursor: &mut std::time::Duration,
    phase: &mut ConnectionPhase,
) -> Result<()> {
    *phase = ConnectionPhase::Paused;
    release_recording_mounts(&mut resources.recording_mounts);
    // Release publisher claims too: a paused session is not
    // publishing, and keeping the claim would make a later
    // RECORD on the same session (or another) fail with 409.
    release_remote_publishers(&mut resources.claims, true);
    depacketizers.clear();
    for task in resources.senders.drain(..) {
        task.abort();
        // Wait for the old sender to actually stop before a new
        // one starts, so the two never interleave on the wire.
        let _ = task.await;
    }
    if let Some(position) = resources.stop_vod() {
        *vod_cursor = position;
    }
    writer
        .lock()
        .await
        .write_all(&rtsp::encode_response(&simple_response(200, "OK", cseq)))
        .await?;
    Ok(())
}
