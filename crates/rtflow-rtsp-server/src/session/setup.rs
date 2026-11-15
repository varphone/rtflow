use super::*;
use rtflow_media::TrackDescription;

fn track_already_setup(
    mount: &Arc<Mount>,
    track: &TrackDescription,
    setups: &[(
        Arc<Mount>,
        UdpPair,
        SocketAddr,
        SocketAddr,
        TrackDescription,
    )],
    interleaved: &[(Arc<Mount>, u8, u8, TrackDescription)],
    multicast_tracks: &[(Arc<Mount>, TrackDescription)],
) -> bool {
    setups.iter().any(|(current, .., current_track)| {
        Arc::ptr_eq(current, mount) && current_track.id == track.id
    }) || interleaved.iter().any(|(current, .., current_track)| {
        Arc::ptr_eq(current, mount) && current_track.id == track.id
    }) || multicast_tracks
        .iter()
        .any(|(current, current_track)| Arc::ptr_eq(current, mount) && current_track.id == track.id)
}

/// SETUP: negotiate and remember one transport for this session.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_setup(
    req: &rtsp::Request,
    state: &Arc<State>,
    writer: &ConnWriter,
    session_id: &str,
    peer: SocketAddr,
    cseq: String,
    setups: &mut Vec<(
        Arc<Mount>,
        UdpPair,
        SocketAddr,
        SocketAddr,
        TrackDescription,
    )>,
    interleaved: &mut Vec<(Arc<Mount>, u8, u8, TrackDescription)>,
    selected_mount: &mut Option<String>,
    multicast_tracks: &mut Vec<(Arc<Mount>, TrackDescription)>,
    interleaved_crypto: &mut std::collections::HashMap<u8, MediaCrypto>,
    phase: &mut ConnectionPhase,
) -> Result<()> {
    #[cfg(not(feature = "multicast"))]
    let _ = multicast_tracks;
    if setups.len() + interleaved.len() + multicast_tracks.len()
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
        return Ok(());
    }
    let Some((path, mount)) = registered_mount_for_uri(state, &req.uri).await else {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                404,
                "Not Found",
                cseq,
            )))
            .await?;
        return Ok(());
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
        return Ok(());
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
        return Ok(());
    };
    #[cfg(any(feature = "srtp", feature = "multicast"))]
    let mount_description = mount.description_snapshot();
    #[cfg(feature = "srtp")]
    let requested_track = track_for_uri(&mount_description, &req.uri).ok();
    let secure_matches = |secure: bool| {
        #[cfg(feature = "srtp")]
        {
            requested_track
                .as_ref()
                .is_none_or(|track| secure == track.crypto.is_some())
        }
        #[cfg(not(feature = "srtp"))]
        {
            !secure
        }
    };
    #[cfg(feature = "multicast")]
    let multicast_mount = mount_description
        .connection
        .as_ref()
        .is_some_and(|connection| connection.address.is_multicast());
    #[cfg(not(feature = "multicast"))]
    let multicast_mount = false;
    let transport = TransportSpec::parse_all(raw).ok().and_then(|alternatives| {
        alternatives.into_iter().find(|candidate| match candidate {
            TransportSpec::TcpInterleaved {
                rtp_channel,
                rtcp_channel,
                rtcp_mux,
                secure,
                ..
            } => {
                secure_matches(*secure)
                    && if *rtcp_mux {
                        rtp_channel == rtcp_channel
                            && !interleaved.iter().any(|(_, current_rtp, current_rtcp, _)| {
                                current_rtp == rtp_channel || current_rtcp == rtp_channel
                            })
                    } else {
                        rtp_channel != rtcp_channel
                            && !interleaved.iter().any(|(_, current_rtp, current_rtcp, _)| {
                                [*current_rtp, *current_rtcp].into_iter().any(|current| {
                                    current == *rtp_channel || current == *rtcp_channel
                                })
                            })
                    }
            }
            TransportSpec::Udp { secure, .. } => secure_matches(*secure),
            TransportSpec::Multicast { secure, .. } => multicast_mount && secure_matches(*secure),
            // `TransportSpec` is `#[non_exhaustive]`.
            _ => false,
        })
    });
    let Some(transport) = transport else {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                461,
                "Unsupported Transport",
                cseq,
            )))
            .await?;
        return Ok(());
    };
    // A Publisher mount only accepts mode=record, while a Local/Proxy/VOD
    // mount only accepts mode=play. Clients commonly omit mode; keep that
    // valid for both roles.
    let publisher_mount = matches!(mount.config, MountConfig::Publisher { .. });
    let requested_mode = match &transport {
        TransportSpec::Udp { mode, .. }
        | TransportSpec::TcpInterleaved { mode, .. }
        | TransportSpec::Multicast { mode, .. } => *mode,
        _ => None,
    };
    let mode_allowed = match requested_mode {
        None => true,
        Some(rtflow_rtsp::transport::TransportMode::Record) => publisher_mount,
        Some(rtflow_rtsp::transport::TransportMode::Play) => !publisher_mount,
    };
    if !mode_allowed {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                461,
                "Unsupported Transport",
                cseq,
            )))
            .await?;
        return Ok(());
    }
    #[allow(unused_variables)]
    if let TransportSpec::Multicast {
        source,
        mode,
        secure,
        ..
    } = &transport
    {
        #[cfg(feature = "multicast")]
        {
            let connection = mount.description_snapshot().connection;
            let Some(connection) =
                connection.filter(|connection| connection.address.is_multicast())
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
                return Ok(());
            };
            let Ok(track) = track_for_uri(&mount.description_snapshot(), &req.uri) else {
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(
                        404,
                        "Not Found",
                        cseq,
                    )))
                    .await?;
                return Ok(());
            };
            if track_already_setup(&mount, &track, setups, interleaved, multicast_tracks) {
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(
                        455,
                        "Method Not Valid in This State",
                        cseq,
                    )))
                    .await?;
                return Ok(());
            }
            #[cfg(feature = "srtp")]
            let profile_mismatch = *secure != track.crypto.is_some();
            #[cfg(not(feature = "srtp"))]
            let profile_mismatch = *secure;
            if profile_mismatch {
                writer
                    .lock()
                    .await
                    .write_all(&rtsp::encode_response(&simple_response(
                        461,
                        "Unsupported Transport",
                        cseq,
                    )))
                    .await?;
                return Ok(());
            }
            let port = connection.port.unwrap_or(5004);
            let mut response = simple_response(200, "OK", cseq);
            response.headers.insert(
                "session".into(),
                session_header(session_id, state.config.session_timeout),
            );
            response.headers.insert(
                "transport".into(),
                TransportSpec::Multicast {
                    destination: Some(connection.address),
                    port: Some(port),
                    ttl: connection.ttl,
                    source: *source,
                    mode: *mode,
                    secure: *secure,
                }
                .render(),
            );
            multicast_tracks.push((mount.clone(), track));
            selected_mount.get_or_insert(path.clone());
            *phase = ConnectionPhase::Ready;
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&response))
                .await?;
            return Ok(());
        }
        #[cfg(not(feature = "multicast"))]
        {
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&simple_response(
                    461,
                    "Unsupported Transport",
                    cseq,
                )))
                .await?;
            return Ok(());
        }
    }
    if let TransportSpec::TcpInterleaved {
        rtp_channel,
        rtcp_channel,
        rtcp_mux,
        secure,
        mode,
    } = transport
    {
        let Ok(track) = track_for_uri(&mount.description_snapshot(), &req.uri) else {
            // Unknown control URI: reply 404 instead of dropping
            // the connection (strict clients treat a dropped
            // connection as a network failure).
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&simple_response(
                    404,
                    "Not Found",
                    cseq,
                )))
                .await?;
            return Ok(());
        };
        if track_already_setup(&mount, &track, setups, interleaved, multicast_tracks) {
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&simple_response(
                    455,
                    "Method Not Valid in This State",
                    cseq,
                )))
                .await?;
            return Ok(());
        }
        #[cfg(feature = "srtp")]
        let profile_mismatch = secure != track.crypto.is_some();
        #[cfg(not(feature = "srtp"))]
        let profile_mismatch = secure;
        if profile_mismatch {
            writer
                .lock()
                .await
                .write_all(&rtsp::encode_response(&simple_response(
                    461,
                    "Unsupported Transport",
                    cseq,
                )))
                .await?;
            return Ok(());
        }
        if secure {
            interleaved_crypto.insert(rtp_channel, MediaCrypto::from_track(&track)?);
        }
        tracing::debug!(
            session = %session_id,
            path = %path,
            track = track.id,
            codec = ?track.codec,
            rtp_channel,
            rtcp_channel,
            rtcp_mux,
            "RTSP SETUP interleaved"
        );
        let proxy_mount = mount.clone();
        interleaved.push((mount, rtp_channel, rtcp_channel, track));
        selected_mount.get_or_insert(path.clone());
        *phase = ConnectionPhase::Ready;
        let mut response = simple_response(200, "OK", cseq);
        response.headers.insert(
            "session".into(),
            session_header(session_id, state.config.session_timeout),
        );
        response.headers.insert(
            "transport".into(),
            TransportSpec::TcpInterleaved {
                rtp_channel,
                rtcp_channel,
                rtcp_mux,
                secure,
                mode,
            }
            .render(),
        );
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&response))
            .await?;
        // Warm the proxy during SETUP so PLAY's first frame does
        // not wait for a full control round trip.
        start_proxy_if_idle(
            proxy_mount,
            state.cancel.clone(),
            state.supervisor.clone(),
            state.config.no_subscriber_grace,
        );
        return Ok(());
    }
    let TransportSpec::Udp {
        client_rtp,
        client_rtcp,
        rtcp_mux,
        secure,
        destination,
        source,
        mode,
        ..
    } = transport
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
        return Ok(());
    };
    let bind_ip = match peer.ip() {
        std::net::IpAddr::V4(_) => std::net::Ipv4Addr::UNSPECIFIED.into(),
        std::net::IpAddr::V6(_) => std::net::Ipv6Addr::UNSPECIFIED.into(),
    };
    let pair = if rtcp_mux {
        UdpPair::bind_mux(
            bind_ip,
            state.config.udp_port_range.clone(),
            state.config.udp_recv_buffer,
        )
        .await?
    } else {
        UdpPair::bind_with_recv_buffer(
            bind_ip,
            state.config.udp_port_range.clone(),
            state.config.udp_recv_buffer,
        )
        .await?
    };
    let server_rtp = pair.rtp_port()?;
    let server_rtcp = if rtcp_mux {
        server_rtp
    } else {
        pair.rtcp_port()?
    };
    let client_rtcp = if rtcp_mux { client_rtp } else { client_rtcp };
    let rtp_target = SocketAddr::new(peer.ip(), client_rtp);
    let rtcp_target = SocketAddr::new(peer.ip(), client_rtcp);
    let Ok(track) = track_for_uri(&mount.description_snapshot(), &req.uri) else {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                404,
                "Not Found",
                cseq,
            )))
            .await?;
        return Ok(());
    };
    if track_already_setup(&mount, &track, setups, interleaved, multicast_tracks) {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                455,
                "Method Not Valid in This State",
                cseq,
            )))
            .await?;
        return Ok(());
    }
    #[cfg(feature = "srtp")]
    let profile_mismatch = secure != track.crypto.is_some();
    #[cfg(not(feature = "srtp"))]
    let profile_mismatch = secure;
    if profile_mismatch {
        writer
            .lock()
            .await
            .write_all(&rtsp::encode_response(&simple_response(
                461,
                "Unsupported Transport",
                cseq,
            )))
            .await?;
        return Ok(());
    }
    let mut response = simple_response(200, "OK", cseq);
    response.headers.insert(
        "session".into(),
        session_header(session_id, state.config.session_timeout),
    );
    response.headers.insert(
        "transport".into(),
        TransportSpec::Udp {
            client_rtp,
            client_rtcp,
            server_rtp: Some(server_rtp),
            server_rtcp: Some(server_rtcp),
            rtcp_mux,
            secure,
            destination,
            source,
            mode,
        }
        .render(),
    );
    tracing::debug!(
        session = %session_id,
        path = %path,
        track = track.id,
        codec = ?track.codec,
        %server_rtp,
        %server_rtcp,
        %rtp_target,
        rtcp_mux,
        "RTSP SETUP udp"
    );
    let proxy_mount = mount.clone();
    setups.push((mount, pair, rtp_target, rtcp_target, track));
    selected_mount.get_or_insert(path);
    *phase = ConnectionPhase::Ready;
    writer
        .lock()
        .await
        .write_all(&rtsp::encode_response(&response))
        .await?;
    // Warm the proxy during SETUP so PLAY's first frame does not
    // wait for a full OPTIONS+DESCRIBE+SETUP+PLAY round trip.
    start_proxy_if_idle(
        proxy_mount,
        state.cancel.clone(),
        state.supervisor.clone(),
        state.config.no_subscriber_grace,
    );
    Ok(())
}
