use super::*;
use crate::media::TrackDescription;

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
    phase: &mut ConnectionPhase,
) -> Result<()> {
    #[cfg(not(feature = "multicast"))]
    let _ = multicast_tracks;
    if setups.len() + interleaved.len() >= state.config.resource_limits.max_tracks_per_session {
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
    let transport = TransportSpec::parse_all(raw).ok().and_then(|alternatives| {
        alternatives.into_iter().find(|candidate| match candidate {
            TransportSpec::TcpInterleaved {
                rtp_channel,
                rtcp_channel,
                rtcp_mux,
                ..
            } => {
                if *rtcp_mux {
                    rtp_channel == rtcp_channel
                        && !interleaved.iter().any(|(_, current_rtp, current_rtcp, _)| {
                            current_rtp == rtp_channel || current_rtcp == rtp_channel
                        })
                } else {
                    rtp_channel != rtcp_channel
                        && !interleaved.iter().any(|(_, current_rtp, current_rtcp, _)| {
                            [*current_rtp, *current_rtcp]
                                .into_iter()
                                .any(|current| current == *rtp_channel || current == *rtcp_channel)
                        })
                }
            }
            TransportSpec::Udp { .. } | TransportSpec::Multicast { .. } => true,
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
    if matches!(transport, TransportSpec::Multicast { .. }) {
        #[cfg(feature = "multicast")]
        {
            let connection = description_for(&mount.config)?.connection;
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
            let Ok(track) = track_for_uri(&description_for(&mount.config)?, &req.uri) else {
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
    } = transport
    {
        if secure {
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
        let Ok(track) = track_for_uri(&description_for(&mount.config)?, &req.uri) else {
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
                secure: false,
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
            4 * 1024 * 1024,
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
    let Ok(track) = track_for_uri(&description_for(&mount.config)?, &req.uri) else {
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
        }
        .render(),
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
