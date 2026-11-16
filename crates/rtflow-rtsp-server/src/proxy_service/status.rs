use super::*;

pub(super) fn status_of(path: &Arc<str>, entry: &Arc<Mutex<Entry>>) -> ProxyStatus {
    let entry = entry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (state, subscribers, bytes, tracks) = match &entry.mount {
        Some(mount) => {
            let snapshot = mount.snapshot();
            let state = match entry.upstream.state() {
                ProxyState::Idle => "idle",
                ProxyState::Connecting => "connecting",
                ProxyState::Streaming => "streaming",
                ProxyState::BackingOff { .. } => "backoff",
                ProxyState::Stopped => "stopped",
            };
            (
                state,
                snapshot.subscriber_count.max(snapshot.broadcast_receivers),
                snapshot.stats.published_bytes,
                mount.description().tracks.len(),
            )
        }
        None => ("describing", 0, 0, 0),
    };
    let state = match entry.state.phase {
        Phase::Describing if entry.mount.is_none() => "describing",
        Phase::Backoff => "backoff",
        _ => state,
    };
    ProxyStatus {
        path: path.clone(),
        state,
        subscribers,
        bytes,
        tracks,
        upstream: redacted_upstream(&entry.upstream),
        attempts: entry.state.attempts,
        retry_in: entry
            .state
            .retry_at
            .map(|retry_at| retry_at.saturating_duration_since(Instant::now())),
        last_error: entry.state.last_error.clone(),
    }
}

pub(super) fn redacted_upstream(upstream: &UpstreamConfig) -> String {
    let url = &upstream.url;
    match url.port_or_known_default() {
        Some(port) => format!(
            "{}://{}:{port}",
            url.scheme(),
            url.host_str().unwrap_or("?")
        ),
        None => format!("{}://{}", url.scheme(), url.host_str().unwrap_or("?")),
    }
}

/// Rewrite upstream-absolute control URLs and strip upstream RTCP hints so a
/// downstream client SETUPs against this server, not against the camera.
pub(super) fn sanitize_description(
    mut description: rtflow_media::MediaDescription,
) -> rtflow_media::MediaDescription {
    for track in &mut description.tracks {
        track.rtcp = None;
        if let Ok(url) = url::Url::parse(&track.control) {
            track.control = Arc::from(url.path().trim_start_matches('/').to_string());
        }
    }
    description
}
