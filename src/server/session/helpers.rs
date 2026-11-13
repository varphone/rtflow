use super::*;

pub(super) type ConnWriter =
    Arc<Mutex<tokio::io::WriteHalf<crate::runtime::io::ControlConnection>>>;

#[derive(Default)]
pub(super) struct ConnectionResources {
    pub(super) senders: Vec<tokio::task::JoinHandle<()>>,
    pub(super) recording_mounts: Vec<Arc<Mount>>,
    pub(super) claims: Vec<Arc<Mount>>,
    pub(super) vod: Option<VodPlayback>,
    /// Mounts with an active PLAY in this session (deduplicated), for the
    /// unique-client counter.
    pub(super) played_mounts: Vec<Arc<Mount>>,
}
impl ConnectionResources {
    pub(super) fn abort_senders(&mut self) {
        for task in self.senders.drain(..) {
            task.abort();
        }
    }

    pub(super) fn stop_vod(&mut self) -> Option<std::time::Duration> {
        self.vod.take().map(VodPlayback::stop)
    }

    /// Register each distinct mount as having one active play session.
    /// Idempotent across repeated PLAY requests.
    pub(super) fn register_play<'a>(&mut self, mounts: impl Iterator<Item = &'a Arc<Mount>>) {
        for mount in mounts {
            if !self
                .played_mounts
                .iter()
                .any(|seen| Arc::ptr_eq(seen, mount))
            {
                mount.active_sessions.fetch_add(1, Ordering::Relaxed);
                self.played_mounts.push(mount.clone());
            }
        }
    }

    pub(super) fn cleanup(&mut self) {
        self.abort_senders();
        self.stop_vod();
        release_recording_mounts(&mut self.recording_mounts);
        for mount in self.played_mounts.drain(..) {
            mount.active_sessions.fetch_sub(1, Ordering::Relaxed);
        }
        // Always release publisher claims when the session ends: the flag
        // guards concurrent take-over while a session is alive, it must not
        // outlive the connection (otherwise a Publisher mount with
        // allow_replace_after_disconnect=false is locked forever).
        release_remote_publishers(&mut self.claims, true);
    }
}
impl Drop for ConnectionResources {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub(super) fn claim_remote_publishers(candidates: Vec<Arc<Mount>>) -> Result<Vec<Arc<Mount>>> {
    let mut claims: Vec<Arc<Mount>> = Vec::new();
    for mount in candidates {
        if !matches!(mount.config, MountConfig::Publisher { .. })
            || claims.iter().any(|current| Arc::ptr_eq(current, &mount))
        {
            continue;
        }
        // A non-RTSP publisher (RTMP/SRT via MountHandle::publisher) already
        // holds the mount: refuse to claim on top of it.
        if mount.publishers.load(Ordering::Acquire) > 0 {
            release_remote_publishers(&mut claims, true);
            return Err(Error::Conflict(
                "publisher is already active on this mount".into(),
            ));
        }
        if mount
            .publisher_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            release_remote_publishers(&mut claims, true);
            return Err(Error::Conflict("publisher is already claimed".into()));
        }
        claims.push(mount);
    }
    Ok(claims)
}

pub(super) fn release_remote_publishers(mounts: &mut Vec<Arc<Mount>>, force: bool) {
    for mount in mounts.drain(..) {
        let replace = matches!(
            mount.config,
            MountConfig::Publisher {
                allow_replace_after_disconnect: true,
                ..
            }
        );
        if force || replace {
            mount.publisher_claimed.store(false, Ordering::Release);
        }
    }
}

pub(super) fn release_recording_mounts(mounts: &mut Vec<Arc<Mount>>) {
    for mount in mounts.drain(..) {
        mount.publishers.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) async fn authorize_request(
    req: &crate::rtsp::Request,
    peer: SocketAddr,
    state: &State,
    cseq: String,
) -> Option<Response> {
    let registered = registered_mount_for_uri(state, &req.uri).await;
    let path = registered
        .as_ref()
        .map(|(path, _)| path.clone())
        .unwrap_or_else(|| uri_path(&req.uri));
    let publisher_mount = registered
        .as_ref()
        .is_some_and(|(_, mount)| matches!(mount.config, MountConfig::Publisher { .. }));
    let role = if matches!(req.method, Method::Announce | Method::Record)
        || (req.method == Method::Setup && publisher_mount)
    {
        AccessRole::Publish
    } else {
        AccessRole::Read
    };
    let decision = state
        .config
        .authenticator
        .authorize(AuthRequest {
            method: req.method,
            path: &path,
            peer_addr: peer,
            role,
            authorization: req.headers.get("authorization").map(String::as_str),
        })
        .await;
    if !matches!(decision, AuthDecision::Allow) {
        emit(
            &state.config.event_sink,
            FrameworkEvent::Security(SecurityEvent::AuthenticationFailed {
                peer,
                mount: Some(MountId::from(path.as_str())),
            }),
        );
        let delay = state.config.security.authentication_failure_delay;
        if delay != std::time::Duration::ZERO {
            // The delay must not outlive the session: server shutdown cancels
            // the session token, so wait on both.
            tokio::select! {
                _ = state.cancel.cancelled() => {}
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }
    match decision {
        AuthDecision::Allow => None,
        AuthDecision::Challenge { realm } => {
            let mut response = simple_response(401, "Unauthorized", cseq);
            response.headers.insert(
                "www-authenticate".into(),
                format!("Basic realm=\"{realm}\""),
            );
            Some(response)
        }
        AuthDecision::DigestChallenge { realm, nonce } => {
            let mut response = simple_response(401, "Unauthorized", cseq);
            response.headers.insert(
                "www-authenticate".into(),
                format!("Digest realm=\"{realm}\", nonce=\"{nonce}\", algorithm=MD5, qop=\"auth\""),
            );
            Some(response)
        }
        AuthDecision::Deny => Some(simple_response(403, "Forbidden", cseq)),
    }
}
pub(super) fn live_sender_input(
    mount: &Arc<Mount>,
    track_id: u8,
    sequence: Arc<AtomicU16>,
) -> SenderInput {
    let (receiver, startup) = mount.subscribe_with_startup(track_id);
    SenderInput {
        receiver,
        startup,
        sequence,
    }
}

pub(super) fn rtp_info_entry(
    mount: &Arc<Mount>,
    play_uri: &str,
    track: &crate::media::TrackDescription,
    input: &SenderInput,
) -> Option<String> {
    // Preferred base: the first keyframe of the startup snapshot. If the
    // client's PLAY arrives before the first GOP lands in the cache (a
    // fresh publisher + immediate SETUP→PLAY), fall back to the latest
    // keyframe the publisher pushed — the NPT offset is then a fraction
    // of a second instead of missing entirely (VLC shows no progress bar
    // without RTP-Info).
    let first = input.startup.iter().find(|unit| {
        unit.track_id == track.id
            && unit.codec == track.codec
            && (track.kind != crate::media::MediaKind::Video || unit.is_keyframe)
    });
    let bootstrap_frame = mount
        .bootstrap
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(track.id as usize)
        .cloned()
        .flatten();
    let first = first.or(bootstrap_frame.as_ref());
    let first = match first {
        Some(unit) => unit,
        None => {
            tracing::debug!(
                "rtp-info: no base frame for track {} (startup={} frames, codec={:?})",
                track.id,
                input.startup.len(),
                track.codec,
            );
            return None;
        }
    };
    // `url` is optional (RFC 2326 §12.33): when the PLAY request used a
    // relative URI (ffmpeg sends "/live/") control_uri cannot resolve an
    // absolute url — omit it instead of dropping RTP-Info entirely.
    // Clients pair the entry with the session/track by seq+rtptime.
    let url = sdp::control_uri(play_uri, &track.control).ok();
    let prefix = url.map(|url| format!("url={url};")).unwrap_or_default();
    Some(format!(
        "{prefix}seq={};rtptime={}",
        input.sequence.load(Ordering::Relaxed),
        first.rtp_timestamp
    ))
}

pub(super) fn session_header(session_id: &str, timeout: std::time::Duration) -> String {
    format!("{session_id};timeout={}", timeout.as_secs().max(1))
}

pub(super) fn simple_response(status: u16, reason: &str, cseq: String) -> Response {
    let mut headers = BTreeMap::new();
    if !cseq.is_empty() {
        headers.insert("cseq".into(), cseq);
    }
    Response {
        status,
        reason: reason.into(),
        headers,
        body: vec![],
    }
}
pub(super) async fn handle_request(req: crate::rtsp::Request, state: &State) -> Response {
    let path = uri_path(&req.uri);
    let cseq = req.headers.get("cseq").cloned().unwrap_or_default();
    let mut headers = BTreeMap::new();
    if !cseq.is_empty() {
        headers.insert("cseq".into(), cseq);
    }
    let mount = state.mounts.get(path.as_str());
    if !matches!(req.method, Method::Options) && mount.is_none() {
        return Response {
            status: 404,
            reason: "Not Found".into(),
            headers,
            body: vec![],
        };
    }
    match req.method {
        Method::Options => {
            headers.insert(
                "public".into(),
                "OPTIONS, DESCRIBE, ANNOUNCE, SETUP, PLAY, PAUSE, RECORD, TEARDOWN, GET_PARAMETER"
                    .into(),
            );
            Response {
                status: 200,
                reason: "OK".into(),
                headers,
                body: vec![],
            }
        }
        Method::Describe => {
            let Some(mount) = mount else {
                return Response {
                    status: 404,
                    reason: "Not Found".into(),
                    headers,
                    body: vec![],
                };
            };
            let Ok(mut d) = description_for(&mount.config) else {
                return Response {
                    status: 503,
                    reason: "Service Unavailable".into(),
                    headers,
                    body: vec![],
                };
            };
            // Rewrite absolute control URLs (e.g. proxied upstream SDP)
            // into path-relative forms: a downstream client must SETUP
            // against *this* server, not against the upstream host.
            for track in &mut d.tracks {
                if let Ok(url) = url::Url::parse(&track.control) {
                    let path = url.path().trim_start_matches('/');
                    track.control = Arc::from(path.to_string());
                }
                // Merge the fmtp auto-injected from the first keyframe
                // (hardware encoders with separated SPS/PPS) when the
                // description itself has no fmtp.
                if track.fmtp.is_none() {
                    let fmtp = mount
                        .announce_fmtp
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&track.id)
                        .cloned();
                    if let Some(fmtp) = fmtp {
                        track.fmtp = Some(fmtp);
                    }
                }
            }
            // Content-Base must be a clean request path: the raw URI can
            // carry query strings or (with broken clients) userinfo, which
            // would leak credentials into every relative-URL response.
            let raw = req.uri.split(['?', '#']).next().unwrap_or(&req.uri);
            let base_path = if raw.contains("://") {
                url::Url::parse(raw)
                    .map(|url| url.path().to_string())
                    .unwrap_or_else(|_| raw.to_string())
            } else {
                raw.to_string()
            };
            let base_path = if base_path.is_empty() {
                "/".to_string()
            } else {
                base_path
            };
            headers.insert("content-type".into(), "application/sdp".into());
            headers.insert(
                "content-base".into(),
                format!("{}/", base_path.trim_end_matches('/')),
            );
            Response {
                status: 200,
                reason: "OK".into(),
                headers,
                body: sdp::encode(&d).into_bytes(),
            }
        }
        Method::SetParameter | Method::Unsupported => Response {
            status: 501,
            reason: "Not Implemented".into(),
            headers,
            body: vec![],
        },
        _ => Response {
            status: 200,
            reason: "OK".into(),
            headers,
            body: vec![],
        },
    }
}
pub(super) fn announced_fmtp(
    expected: &MediaDescription,
    announced: &MediaDescription,
) -> Result<HashMap<u8, Arc<str>>> {
    if expected.tracks.len() != announced.tracks.len() {
        return Err(Error::Rtsp(
            "ANNOUNCE track count does not match mount".into(),
        ));
    }

    expected
        .tracks
        .iter()
        .zip(announced.tracks.iter())
        .map(|(expected, announced)| {
            if expected.kind != announced.kind
                || expected.codec != announced.codec
                || expected.clock_rate != announced.clock_rate
                || expected.channels != announced.channels
            {
                return Err(Error::Rtsp(format!(
                    "ANNOUNCE track {} does not match mount",
                    expected.id
                )));
            }
            Ok(announced
                .fmtp
                .as_ref()
                .map(|value| (expected.id, value.clone())))
        })
        .filter_map(|result| result.transpose())
        .collect()
}

pub(super) fn track_for_uri(
    description: &MediaDescription,
    uri: &str,
) -> Result<crate::media::TrackDescription> {
    let requested = url::Url::parse(uri).ok();
    description
        .tracks
        .iter()
        .find(|track| {
            if let Ok(control) = url::Url::parse(&track.control) {
                return requested.as_ref() == Some(&control);
            }
            let requested_path = requested.as_ref().map(url::Url::path).unwrap_or(uri);
            let control = track.control.trim_start_matches('/');
            requested_path == track.control.as_ref()
                || requested_path.ends_with(&format!("/{control}"))
        })
        .cloned()
        .ok_or_else(|| Error::Rtsp(format!("no track matches SETUP URI {uri}")))
}

pub(super) async fn registered_mount_for_uri(
    state: &State,
    uri: &str,
) -> Option<(String, Arc<Mount>)> {
    let requested = uri_path(uri);
    state
        .mounts
        .find_longest_prefix(&requested)
        .map(|(path, mount)| (path.to_string(), mount))
}
pub(super) fn uri_path(uri: &str) -> String {
    url::Url::parse(uri)
        .map(|u| u.path().to_owned())
        .unwrap_or_else(|_| uri.to_owned())
}
