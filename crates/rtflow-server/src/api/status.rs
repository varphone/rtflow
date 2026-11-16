use super::*;

pub(super) fn proxy_json(state: &ApiState) -> Vec<serde_json::Value> {
    state
        .proxies
        .statuses()
        .into_iter()
        .map(|status| {
            json!({
                "path": status.path.as_ref(),
                "state": status.state,
                "subscribers": status.subscribers,
                "bytes": status.bytes,
                "tracks": status.tracks,
                "upstream": status.upstream,
                "attempts": status.attempts,
                "retry_in_ms": status.retry_in.map(|value| value.as_millis() as u64),
            })
        })
        .collect()
}

pub(super) fn srt_status_json(state: &ApiState) -> serde_json::Value {
    let listener: Vec<serde_json::Value> = state
        .srt_listener
        .as_ref()
        .map(|stats| {
            stats
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .map(|entry| {
                    json!({
                        "role": "listener",
                        "peer": entry.peer.to_string(),
                        "stream_id": entry.stream_id,
                        "mount": entry.mount,
                        "encrypted": entry.encrypted,
                        "latency_ms": entry.latency_ms,
                        "rtt_us": entry.rtt_us,
                        "packets": entry.packets_received,
                        "bytes": entry.bytes_received,
                        "delivered": entry.delivered,
                        "skipped": entry.skipped,
                        "losses": entry.loss_events,
                        "retransmitted": entry.retransmitted,
                        "undecryptable": entry.undecryptable,
                        "duration_secs": entry.duration_secs,
                        "srt_version": format!("{:#x}", entry.srt_version),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let pulls: Vec<serde_json::Value> = state
        .srt_pulls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|entry| {
            json!({
                "role": "caller",
                "remote": entry.remote,
                "stream_id": entry.stream_id,
                "mount": entry.mount,
                "connected": entry.connected,
                "encrypted": entry.encrypted,
                "bytes": entry.bytes,
                "rtt_us": entry.rtt_us,
                "skipped": entry.skipped,
                "last_error": entry.last_error,
            })
        })
        .collect();
    let pushes: Vec<serde_json::Value> = state
        .srt_pushes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|entry| {
            json!({
                "role": "sender",
                "mount": entry.mount,
                "remote": entry.remote,
                "stream_id": entry.stream_id,
                "connected": entry.connected,
                "encrypted": entry.encrypted,
                "bytes_sent": entry.bytes_sent,
                "rtt_us": entry.rtt_us,
                "last_error": entry.last_error,
            })
        })
        .collect();
    json!({"listener": listener, "pulls": pulls, "pushes": pushes})
}
