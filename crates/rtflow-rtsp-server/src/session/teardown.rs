use super::*;

/// TEARDOWN: release every resource owned by the session.
pub(super) async fn handle_teardown(
    writer: &ConnWriter,
    cseq: String,
    resources: &mut ConnectionResources,
    depacketizers: &mut HashMap<u8, Depacketizer>,
) -> Result<()> {
    release_recording_mounts(&mut resources.recording_mounts);
    release_remote_publishers(&mut resources.claims, true);
    depacketizers.clear();
    for task in resources.senders.drain(..) {
        task.abort();
        // Wait for the old sender to actually stop before a new
        // one starts, so the two never interleave on the wire.
        let _ = task.await;
    }
    writer
        .lock()
        .await
        .write_all(&rtsp::encode_response(&simple_response(200, "OK", cseq)))
        .await?;
    Ok(())
}
