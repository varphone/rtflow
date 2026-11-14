use super::*;

pub(crate) fn rtp_info_entry(
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
    let first = input
        .startup
        .iter()
        .find(|unit| {
            unit.track_id == track.id
                && unit.codec == track.codec
                && (track.kind != crate::media::MediaKind::Video || unit.is_keyframe)
        })
        .map(|unit| &**unit);
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

pub(crate) fn session_header(session_id: &str, timeout: std::time::Duration) -> String {
    format!("{session_id};timeout={}", timeout.as_secs().max(1))
}

pub(crate) fn simple_response(status: u16, reason: &str, cseq: String) -> Response {
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
/// Derive an absolute Content-Base from the request URI (or the accepted
/// connection address when the URI is relative). RFC 2326 §12.5 requires an
/// absolute URI here; clients resolve track `a=control` values against it.
fn content_base(
    req: &crate::rtsp::Request,
    local_addr: std::net::SocketAddr,
    scheme: &str,
) -> String {
    let raw = req.uri.split(['?', '#']).next().unwrap_or(&req.uri);
    let path = if raw.contains("://") {
        url::Url::parse(raw)
            .map(|url| url.path().to_string())
            .unwrap_or_else(|_| raw.to_string())
    } else {
        raw.to_string()
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };
    let path = format!("{}/", path.trim_end_matches('/'));
    if let Ok(url) = url::Url::parse(&req.uri)
        && let Some(host) = url.host_str()
    {
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        let port = url
            .port()
            .map(|value| format!(":{value}"))
            .unwrap_or_default();
        return format!("{}://{host}{port}{path}", url.scheme());
    }
    let host = if local_addr.is_ipv6() {
        format!("[{}]", local_addr.ip())
    } else {
        local_addr.ip().to_string()
    };
    let port = local_addr.port();
    let default_port = (scheme == "rtsp" && port == 554) || (scheme == "rtsps" && port == 322);
    let port = if default_port {
        String::new()
    } else {
        format!(":{port}")
    };
    format!("{scheme}://{host}{port}{path}")
}

pub(crate) async fn handle_request(
    req: crate::rtsp::Request,
    state: &State,
    local_addr: std::net::SocketAddr,
    scheme: &str,
) -> Response {
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
                "OPTIONS, DESCRIBE, ANNOUNCE, SETUP, PLAY, PAUSE, RECORD, TEARDOWN, GET_PARAMETER, SET_PARAMETER"
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
            let mut d = mount.description_snapshot();
            if d.tracks.is_empty() {
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
            let is_proxy = matches!(mount.config, crate::mount::MountConfig::Proxy { .. });
            for track in &mut d.tracks {
                if is_proxy {
                    // The downstream client must send RTCP to this server's
                    // negotiated port, not to the upstream address/port.
                    track.rtcp = None;
                }
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
            headers.insert("content-type".into(), "application/sdp".into());
            headers.insert(
                "content-base".into(),
                content_base(&req, local_addr, scheme),
            );
            Response {
                status: 200,
                reason: "OK".into(),
                headers,
                body: sdp::encode_with_rtx(&d, Some(local_addr.ip()), true, state.config.rtx)
                    .into_bytes(),
            }
        }
        // SET_PARAMETER is accepted and ignored: RTSP 1.0 leaves the
        // parameter set open, and clients use it for non-critical stream
        // properties. Unknown methods still get 501.
        Method::SetParameter => Response {
            status: 200,
            reason: "OK".into(),
            headers,
            body: vec![],
        },
        Method::Unsupported => Response {
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
pub(crate) fn announced_fmtp(
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
