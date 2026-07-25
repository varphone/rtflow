//! Per-connection RTSP state machine and media task ownership.
use super::{
    ConnectionPhase, Mount, MountConfig, State, description_for,
    proxy_runtime::start_proxy_if_idle,
    tasks::{
        SenderInput, spawn_interleaved_sender, spawn_interleaved_sender_from, spawn_udp_receiver,
        spawn_udp_sender, spawn_udp_sender_from,
    },
    vod_runtime::{VodPlayback, spawn_vod_playback},
};
use crate::{
    Error, Result,
    auth::{AccessRole, AuthDecision, AuthRequest},
    identity::{MountId, SessionId},
    media::MediaDescription,
    observe::{FrameworkEvent, SecurityEvent, emit},
    protocol::range::{NptPosition, NptRange},
    protocol::sdp,
    rtcp::Packet as RtcpPacket,
    rtp::{Depacketizer, RtpPacket},
    rtsp::{self, Message, Method, Response},
    runtime::{
        framing::{ConnectionFramer, InboundFrame},
        io::ControlConnection,
    },
    transport::{TransportSpec, UdpPair},
};
use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

pub(super) async fn serve_connection(
    stream: ControlConnection,
    peer: SocketAddr,
    session_id: SessionId,
    state: Arc<State>,
) -> Result<()> {
    let (mut reader, write_half) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(write_half));
    let mut framer = ConnectionFramer::new(
        state.config.max_rtsp_header_bytes,
        state.config.max_rtsp_body_bytes,
        state.config.resource_limits.max_interleaved_frame_bytes,
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
    let session_id = session_id.to_string();
    let mut phase = ConnectionPhase::Init;
    let mut selected_mount: Option<String> = None;
    let mut interleaved_depacketizers: HashMap<u8, Depacketizer> = HashMap::new();
    let mut vod_cursor = std::time::Duration::ZERO;
    let mut rtp_sequences: HashMap<u8, Arc<AtomicU16>> = HashMap::new();
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
                                    matches!(
                                        packet,
                                        RtcpPacket::ReceiverReport { .. }
                                            | RtcpPacket::ReceiverReportWithReports { .. }
                                    )
                                })
                                .count() as u64;
                            mount.stats.add_receiver_reports(reports);
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
                                mount.stats.add_accepted(1);
                            } else {
                                mount.stats.add_dropped(1);
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
                if setups.len() + interleaved.len()
                    >= state.config.resource_limits.max_tracks_per_session
                {
                    writer
                        .lock()
                        .await
                        .write_all(&rtsp::encode_response(&simple_response(
                            453,
                            "Not Enough Bandwidth",
                            cseq,
                        )))
                        .await?;
                    continue;
                }
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
                            continue;
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
                    continue;
                }
                for task in resources.senders.drain(..) {
                    task.abort();
                }
                if let Some(position) = resources.stop_vod() {
                    vod_cursor = position;
                }
                let start = requested_range
                    .map(|range| match range.start {
                        NptPosition::Time(position) => position,
                        NptPosition::Now => vod_cursor,
                    })
                    .unwrap_or(vod_cursor);
                let end = requested_range.and_then(|range| range.end);
                let vod_output = vod_source.as_ref().map(|_| {
                    tokio::sync::broadcast::channel(state.config.consumer_queue_capacity).0
                });
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
                    phase = ConnectionPhase::Playing;
                    let mut response = simple_response(200, "OK", cseq);
                    response
                        .headers
                        .insert("session".into(), session_id.clone());
                    if vod_source.is_some() {
                        response.headers.insert(
                            "range".into(),
                            NptRange {
                                start: NptPosition::Time(start),
                                end,
                            }
                            .render(),
                        );
                    } else if let Some(range) = requested_range {
                        response.headers.insert("range".into(), range.render());
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
                            spawn_interleaved_sender(
                                mount.clone(),
                                rtp_channel,
                                rtcp_channel,
                                track,
                                writer.clone(),
                                sequence,
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
                            spawn_udp_sender(
                                mount.clone(),
                                socket,
                                rtp_target,
                                rtcp_target,
                                track,
                                sequence,
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
                if let Some(position) = resources.stop_vod() {
                    vod_cursor = position;
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
                if let Some(position) = resources.stop_vod() {
                    vod_cursor = position;
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
    vod: Option<VodPlayback>,
}
impl ConnectionResources {
    fn abort_senders(&mut self) {
        for task in self.senders.drain(..) {
            task.abort();
        }
    }

    fn stop_vod(&mut self) -> Option<std::time::Duration> {
        self.vod.take().map(VodPlayback::stop)
    }

    fn cleanup(&mut self) {
        self.abort_senders();
        self.stop_vod();
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
            tokio::time::sleep(delay).await;
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
            let Some(mount) = mount else {
                return Response {
                    status: 404,
                    reason: "Not Found".into(),
                    headers,
                    body: vec![],
                };
            };
            let Ok(d) = description_for(&mount.config) else {
                return Response {
                    status: 503,
                    reason: "Service Unavailable".into(),
                    headers,
                    body: vec![],
                };
            };
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
