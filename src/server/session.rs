//! Per-connection RTSP state machine and media task ownership.
use super::{
    ConnectionPhase, Mount, MountConfig, State, description_for,
    proxy_runtime::start_proxy_if_idle,
    tasks::{spawn_interleaved_sender, spawn_udp_receiver, spawn_udp_sender},
};
use crate::{
    Error, Result,
    auth::{AccessRole, AuthDecision, AuthRequest},
    media::MediaDescription,
    protocol::sdp,
    rtcp::Packet as RtcpPacket,
    rtp::{Depacketizer, RtpPacket},
    rtsp::{self, Message, Method, Response},
    runtime::framing::{ConnectionFramer, InboundFrame},
    transport::{TransportSpec, UdpPair},
};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{Arc, atomic::Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};

pub(super) async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<State>,
) -> Result<()> {
    let (mut reader, write_half) = stream.into_split();
    let writer = Arc::new(Mutex::new(write_half));
    let mut framer = ConnectionFramer::new(
        state.config.max_rtsp_header_bytes,
        state.config.max_rtsp_body_bytes,
        65_536,
    );
    let mut scratch = [0u8; 4096];
    let mut setups: Vec<(
        Arc<Mount>,
        UdpPair,
        SocketAddr,
        SocketAddr,
        crate::media::TrackDescription,
    )> = Vec::new();
    let mut resources = ConnectionResources::default();
    let mut interleaved: Vec<(Arc<Mount>, u8, u8, crate::media::TrackDescription)> = Vec::new();
    let session_id = state
        .next_session
        .fetch_add(1, Ordering::Relaxed)
        .to_string();
    let mut phase = ConnectionPhase::Init;
    let mut selected_mount: Option<String> = None;
    let mut interleaved_depacketizers: HashMap<u8, Depacketizer> = HashMap::new();
    loop {
        let n = tokio::select! { _ = state.cancel.cancelled() => break, result = tokio::time::timeout(state.config.session_timeout, reader.read(&mut scratch)) => result.map_err(|_| Error::Closed)?? };
        if n == 0 {
            break;
        }
        framer.push(&scratch[..n]);
        while let Some(frame) = framer.next_frame()? {
            let msg = match frame {
                InboundFrame::Rtsp(message) => message,
                InboundFrame::Interleaved(frame) => {
                    if frame.channel % 2 == 1 {
                        if let Some((mount, _, _, _)) = interleaved
                            .iter()
                            .find(|(_, _, rtcp_channel, _)| *rtcp_channel == frame.channel)
                            && let Ok(packets) = RtcpPacket::parse(frame.payload)
                        {
                            let reports = packets
                                .iter()
                                .filter(|packet| {
                                    matches!(packet, RtcpPacket::ReceiverReport { .. })
                                })
                                .count() as u64;
                            let mut stats = mount.stats.lock().unwrap();
                            stats.rtcp_receiver_reports =
                                stats.rtcp_receiver_reports.saturating_add(reports);
                        }
                        continue;
                    }
                    if phase == ConnectionPhase::Recording
                        && let Some((mount, _, _, track)) = interleaved
                            .iter()
                            .find(|(_, rtp_channel, _, _)| *rtp_channel == frame.channel)
                            .cloned()
                    {
                        let depacketizer = interleaved_depacketizers
                            .entry(frame.channel)
                            .or_insert_with(|| Depacketizer::new(track.codec, track.id));
                        if let Ok(Some(unit)) = RtpPacket::parse(frame.payload)
                            .and_then(|packet| depacketizer.push(packet))
                        {
                            if mount.frames.send(unit).is_ok() {
                                mount.stats.lock().unwrap().accepted_frames += 1;
                            } else {
                                mount.stats.lock().unwrap().dropped_frames += 1;
                            }
                        }
                    }
                    continue;
                }
            };
            let Message::Request(req) = msg else {
                return Err(Error::Rtsp("client sent response".into()));
            };
            let cseq = req.headers.get("cseq").cloned().unwrap_or_default();
            if let Some(response) = authorize_request(&req, peer, &state, cseq.clone()).await {
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&response))
                    .await?;
                continue;
            }
            let provided_session = req
                .headers
                .get("session")
                .map(|value| value.split(';').next().unwrap_or(value).trim());
            let invalid_session = if req.method == Method::Setup {
                provided_session.is_some_and(|value| value != session_id)
            } else if matches!(
                req.method,
                Method::Play | Method::Pause | Method::Record | Method::Teardown
            ) {
                provided_session != Some(session_id.as_str())
            } else {
                false
            };
            if invalid_session {
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(
                        454,
                        "Session Not Found",
                        cseq,
                    )))
                    .await?;
                continue;
            }
            let valid_phase = match req.method {
                Method::Setup => matches!(phase, ConnectionPhase::Init | ConnectionPhase::Ready),
                Method::Play => matches!(phase, ConnectionPhase::Ready | ConnectionPhase::Paused),
                Method::Pause => phase == ConnectionPhase::Playing,
                Method::Record => phase == ConnectionPhase::Ready,
                _ => true,
            };
            if !valid_phase {
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(
                        455,
                        "Method Not Valid in This State",
                        cseq,
                    )))
                    .await?;
                continue;
            }
            if req.method == Method::Setup {
                let Some((path, mount)) = registered_mount_for_uri(&state, &req.uri).await else {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            404,
                            "Not Found",
                            cseq,
                        )))
                        .await?;
                    continue;
                };
                if selected_mount
                    .as_ref()
                    .is_some_and(|selected| selected != &path)
                {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            459,
                            "Aggregate Operation Not Allowed",
                            cseq,
                        )))
                        .await?;
                    continue;
                }
                let Some(raw) = req.headers.get("transport") else {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            461,
                            "Unsupported Transport",
                            cseq,
                        )))
                        .await?;
                    continue;
                };
                if let Ok(TransportSpec::TcpInterleaved {
                    rtp_channel,
                    rtcp_channel,
                }) = TransportSpec::parse(raw)
                {
                    let track = track_for_uri(&description_for(&mount.config)?, &req.uri)?;
                    interleaved.push((mount, rtp_channel, rtcp_channel, track));
                    selected_mount.get_or_insert(path.clone());
                    phase = ConnectionPhase::Ready;
                    let mut response = simple_response(200, "OK", cseq);
                    response
                        .headers
                        .insert("session".into(), session_id.clone());
                    response.headers.insert(
                        "transport".into(),
                        TransportSpec::TcpInterleaved {
                            rtp_channel,
                            rtcp_channel,
                        }
                        .render(),
                    );
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&response))
                        .await?;
                    continue;
                }
                let Ok(TransportSpec::Udp {
                    client_rtp,
                    client_rtcp,
                    ..
                }) = TransportSpec::parse(raw)
                else {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            461,
                            "Unsupported Transport",
                            cseq,
                        )))
                        .await?;
                    continue;
                };
                let bind_ip = match peer.ip() {
                    std::net::IpAddr::V4(_) => std::net::Ipv4Addr::UNSPECIFIED.into(),
                    std::net::IpAddr::V6(_) => std::net::Ipv6Addr::UNSPECIFIED.into(),
                };
                let pair = UdpPair::bind(bind_ip, state.config.udp_port_range.clone()).await?;
                let server_rtp = pair.rtp_port()?;
                let server_rtcp = pair.rtcp_port()?;
                let rtp_target = SocketAddr::new(peer.ip(), client_rtp);
                let rtcp_target = SocketAddr::new(peer.ip(), client_rtcp);
                let track = track_for_uri(&description_for(&mount.config)?, &req.uri)?;
                let mut response = simple_response(200, "OK", cseq);
                response
                    .headers
                    .insert("session".into(), session_id.clone());
                response.headers.insert(
                    "transport".into(),
                    TransportSpec::Udp {
                        client_rtp,
                        client_rtcp,
                        server_rtp: Some(server_rtp),
                        server_rtcp: Some(server_rtcp),
                    }
                    .render(),
                );
                setups.push((mount, pair, rtp_target, rtcp_target, track));
                selected_mount.get_or_insert(path);
                phase = ConnectionPhase::Ready;
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&response))
                    .await?;
            } else if req.method == Method::Play {
                phase = ConnectionPhase::Playing;
                for task in resources.senders.drain(..) {
                    task.abort();
                }
                for (mount, rtp_channel, rtcp_channel, track) in interleaved.iter().cloned() {
                    start_proxy_if_idle(
                        mount.clone(),
                        state.cancel.clone(),
                        state.config.no_subscriber_grace,
                    );
                    resources.senders.push(spawn_interleaved_sender(
                        mount,
                        rtp_channel,
                        rtcp_channel,
                        track,
                        writer.clone(),
                    ));
                }
                for (mount, socket, rtp_target, rtcp_target, track) in setups.iter().cloned() {
                    start_proxy_if_idle(
                        mount.clone(),
                        state.cancel.clone(),
                        state.config.no_subscriber_grace,
                    );
                    resources.senders.push(spawn_udp_sender(
                        mount,
                        socket,
                        rtp_target,
                        rtcp_target,
                        track,
                    ));
                }
                if resources.senders.is_empty() {
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
                    let mut response = simple_response(200, "OK", cseq);
                    response
                        .headers
                        .insert("session".into(), session_id.clone());
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&response))
                        .await?;
                }
            } else if req.method == Method::Record {
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
                        continue;
                    }
                }
                for task in resources.senders.drain(..) {
                    task.abort();
                }
                for (mount, socket, _rtp_target, _rtcp_target, track) in setups.iter().cloned() {
                    if matches!(mount.config, MountConfig::Publisher { .. }) {
                        resources
                            .senders
                            .push(spawn_udp_receiver(mount, socket, track));
                    }
                }
                let mut has_interleaved_receiver = false;
                for (mount, rtp_channel, _rtcp_channel, track) in interleaved.iter().cloned() {
                    if matches!(mount.config, MountConfig::Publisher { .. }) {
                        has_interleaved_receiver = true;
                        interleaved_depacketizers
                            .insert(rtp_channel, Depacketizer::new(track.codec, track.id));
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
                    phase = ConnectionPhase::Recording;
                    let mut response = simple_response(200, "OK", cseq);
                    response
                        .headers
                        .insert("session".into(), session_id.clone());
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&response))
                        .await?;
                }
            } else if req.method == Method::Pause {
                phase = ConnectionPhase::Paused;
                release_recording_mounts(&mut resources.recording_mounts);
                interleaved_depacketizers.clear();
                for task in resources.senders.drain(..) {
                    task.abort();
                }
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(200, "OK", cseq)))
                    .await?;
            } else if req.method == Method::Teardown {
                release_recording_mounts(&mut resources.recording_mounts);
                release_remote_publishers(&mut resources.claims, false);
                interleaved_depacketizers.clear();
                for task in resources.senders.drain(..) {
                    task.abort();
                }
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(200, "OK", cseq)))
                    .await?;
                return Ok(());
            } else {
                let response = handle_request(req, &state).await;
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&response))
                    .await?;
            }
        }
    }
    resources.cleanup();
    Ok(())
}
#[derive(Default)]
struct ConnectionResources {
    senders: Vec<tokio::task::JoinHandle<()>>,
    recording_mounts: Vec<Arc<Mount>>,
    claims: Vec<Arc<Mount>>,
}
impl ConnectionResources {
    fn abort_senders(&mut self) {
        for task in self.senders.drain(..) {
            task.abort();
        }
    }

    fn cleanup(&mut self) {
        self.abort_senders();
        release_recording_mounts(&mut self.recording_mounts);
        release_remote_publishers(&mut self.claims, false);
    }
}
impl Drop for ConnectionResources {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn claim_remote_publishers(candidates: Vec<Arc<Mount>>) -> Result<Vec<Arc<Mount>>> {
    let mut claims: Vec<Arc<Mount>> = Vec::new();
    for mount in candidates {
        if !matches!(mount.config, MountConfig::Publisher { .. })
            || claims.iter().any(|current| Arc::ptr_eq(current, &mount))
        {
            continue;
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

fn release_remote_publishers(mounts: &mut Vec<Arc<Mount>>, force: bool) {
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

fn release_recording_mounts(mounts: &mut Vec<Arc<Mount>>) {
    for mount in mounts.drain(..) {
        mount.publishers.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn authorize_request(
    req: &crate::rtsp::Request,
    peer: SocketAddr,
    state: &State,
    cseq: String,
) -> Option<Response> {
    let path = registered_mount_for_uri(state, &req.uri)
        .await
        .map(|(path, _)| path)
        .unwrap_or_else(|| uri_path(&req.uri));
    let role = if matches!(req.method, Method::Announce | Method::Record) {
        AccessRole::Publish
    } else {
        AccessRole::Read
    };
    match state
        .config
        .authenticator
        .authorize(AuthRequest {
            method: req.method,
            path: &path,
            peer_addr: peer,
            role,
            authorization: req.headers.get("authorization").map(String::as_str),
        })
        .await
    {
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
fn simple_response(status: u16, reason: &str, cseq: String) -> Response {
    let mut headers = BTreeMap::new();
    headers.insert("cseq".into(), cseq);
    Response {
        status,
        reason: reason.into(),
        headers,
        body: vec![],
    }
}
async fn handle_request(req: crate::rtsp::Request, state: &State) -> Response {
    let path = uri_path(&req.uri);
    let cseq = req.headers.get("cseq").cloned().unwrap_or_default();
    let mut headers = BTreeMap::new();
    headers.insert("cseq".into(), cseq);
    let mounts = state.mounts.read().await;
    let mount = mounts.get(path.as_str());
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
                "OPTIONS, DESCRIBE, ANNOUNCE, SETUP, PLAY, PAUSE, RECORD, TEARDOWN, GET_PARAMETER, SET_PARAMETER".into(),
            );
            Response {
                status: 200,
                reason: "OK".into(),
                headers,
                body: vec![],
            }
        }
        Method::Describe => {
            let d = description_for(&mount.unwrap().config).unwrap();
            headers.insert("content-type".into(), "application/sdp".into());
            headers.insert(
                "content-base".into(),
                format!("{}/", req.uri.trim_end_matches('/')),
            );
            Response {
                status: 200,
                reason: "OK".into(),
                headers,
                body: sdp::encode(&d).into_bytes(),
            }
        }
        _ => Response {
            status: 200,
            reason: "OK".into(),
            headers,
            body: vec![],
        },
    }
}
fn track_for_uri(
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

async fn registered_mount_for_uri(state: &State, uri: &str) -> Option<(String, Arc<Mount>)> {
    let requested = uri_path(uri);
    let mounts = state.mounts.read().await;
    mounts
        .iter()
        .filter(|(path, _)| {
            requested == path.as_ref()
                || requested
                    .strip_prefix(path.as_ref())
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
        .max_by_key(|(path, _)| path.len())
        .map(|(path, mount)| (path.to_string(), mount.clone()))
}
fn uri_path(uri: &str) -> String {
    url::Url::parse(uri)
        .map(|u| u.path().to_owned())
        .unwrap_or_else(|_| uri.to_owned())
}
