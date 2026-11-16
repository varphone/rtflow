//! Handshake and key-management flow for the SRT listener.
use super::*;
use crate::connection::HandshakeInfo;
use crate::crypto::{KmState, SessionCrypto};
use crate::handshake::{
    Handshake, HandshakeType, HsExtFields, HsExtension, HsFlags, SRT_CMD_FILTER, SRT_CMD_GROUP,
    SRT_VERSION, SRT_VERSION_4, SRT_VERSION_5, conclusion_response, generate_cookie,
    handshake_control_packet, induction_response, parse_handshake, reject, rejection_response,
};
use crate::packet::{ControlType, shutdown_packet};

impl SrtServer {
    async fn send_rejection(
        &self,
        peer: SocketAddr,
        caller: &Handshake,
        listener_socket_id: u32,
        reason: u32,
        detail: &str,
    ) {
        let response = rejection_response(listener_socket_id, caller, reason);
        let packet = handshake_control_packet(&response, caller.socket_id, 0);
        #[cfg(feature = "metrics")]
        if let Some(metrics) = &self.config.metrics {
            use std::sync::atomic::Ordering;
            metrics
                .srt_handshake_failures
                .fetch_add(1, Ordering::Relaxed);
        }
        warn!(
            peer = %peer,
            reason,
            detail,
            "SRT: rejected connection"
        );
        self.send_control(&packet, peer).await;
    }

    #[cfg(feature = "metrics")]
    pub(super) fn release_stream_metric(&self) {
        if let Some(metrics) = &self.config.metrics {
            use std::sync::atomic::Ordering;
            metrics.srt_streams_active.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[cfg(not(feature = "metrics"))]
    pub(super) fn release_stream_metric(&self) {}

    pub(super) fn log_connection_stats(
        &self,
        conn: &SrtConnection,
        peer: SocketAddr,
        reason: &str,
    ) {
        let stats = conn.tsbpd.stats();
        info!(
            peer = %peer,
            reason,
            seconds = conn.start_time.elapsed().as_secs(),
            packets = conn.packets_received,
            bytes = conn.bytes_received,
            delivered = stats.delivered,
            skipped = stats.skipped,
            losses = stats.loss_events,
            rtt_us = conn.rtt_us,
            latency_ms = conn.latency_ms,
            srt_version = format!("{:#x}", conn.peer_caps.srt_version),
            flags = format!("{:#x}", conn.peer_caps.flags),
            undecryptable = conn.undecryptable,
            peer_recv_latency_ms = conn.peer_caps.recv_latency_ms,
            peer_send_latency_ms = conn.peer_caps.send_latency_ms,
            "SRT: connection closed"
        );
    }

    pub(crate) async fn handle_control(
        &self,
        ctrl: &packet::ControlPacket,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
        next_socket_id: &mut u32,
    ) {
        if let Some(conn) = connections.get_mut(&peer) {
            conn.last_recv_time = Instant::now();
        }

        match ctrl.control_type {
            ControlType::Handshake => {
                self.handle_handshake(ctrl, peer, connections, next_socket_id)
                    .await
            }
            ControlType::Keepalive => {
                if let Some(conn) = connections.get(&peer) {
                    let timestamp = conn.start_time.elapsed().as_micros() as u32;
                    let response = keepalive_packet(conn.peer_socket_id, timestamp);
                    self.send_control(&response, peer).await;
                }
            }
            ControlType::AckAck => {
                if let Some(conn) = connections.get_mut(&peer)
                    && let Some((ack_seq, sent_at)) = conn.last_ack_sent
                    && ctrl.additional_info == ack_seq
                {
                    let sample = sent_at.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
                    if conn.rtt_us == 0 {
                        conn.rtt_us = sample;
                    } else {
                        let delta = sample.abs_diff(conn.rtt_us);
                        conn.rtt_var_us = (conn.rtt_var_us * 3 + delta) / 4;
                        conn.rtt_us = (conn.rtt_us * 7 + sample) / 8;
                    }
                    debug!(peer = %peer, rtt_us = conn.rtt_us, "SRT: ACKACK");
                }
            }
            ControlType::Ack => {
                // A bidirectional peer may acknowledge our control packets;
                // nothing to do for an ingest-only listener.
                debug!(peer = %peer, "SRT: received ACK");
            }
            ControlType::Nak => {
                // We are the receiver: a peer NAK would target data we sent,
                // which the listener never does.
                debug!(peer = %peer, "SRT: ignoring NAK (receive-only)");
            }
            ControlType::Shutdown => {
                info!(peer = %peer, "SRT: peer requested shutdown");
                if let Some(conn) = connections.get_mut(&peer) {
                    conn.state = ConnState::Closing;
                    self.log_connection_stats(conn, peer, "shutdown");
                    let timestamp = conn.start_time.elapsed().as_micros() as u32;
                    let reply = shutdown_packet(conn.peer_socket_id, timestamp);
                    self.send_control(&reply, peer).await;
                    if conn.mount_registered
                        && let Some(path) = conn.mount_path.clone()
                    {
                        self.release_stream_metric();
                        self.mounts.unregister(&path);
                        info!("SRT: unregistered mount {path} on peer shutdown");
                    }
                }
                connections.remove(&peer);
            }
            ControlType::DropReq => {
                if let Some(conn) = connections.get_mut(&peer)
                    && let Some(target) = drop_request_range(ctrl)
                {
                    conn.tsbpd.drop_range(target.0, target.1);
                    debug!(peer = %peer, first = target.0, last = target.1, "SRT: DROPREQ");
                }
            }
            ControlType::PeerError => {
                warn!(peer = %peer, code = ctrl.additional_info, "SRT: peer error");
                if let Some(conn) = connections.get(&peer) {
                    self.log_connection_stats(conn, peer, "peer-error");
                }
                if let Some(conn) = connections.get(&peer)
                    && conn.mount_registered
                    && let Some(path) = &conn.mount_path
                {
                    self.release_stream_metric();
                    self.mounts.unregister(path);
                }
                connections.remove(&peer);
            }
            ControlType::CongestionWarning => {
                debug!(peer = %peer, "SRT: congestion warning");
            }
            ControlType::UserDefined => {
                self.handle_user_defined(ctrl, peer, connections).await;
            }
        }
    }

    async fn handle_user_defined(
        &self,
        ctrl: &packet::ControlPacket,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
    ) {
        let Some(conn) = connections.get_mut(&peer) else {
            return;
        };
        match ctrl.type_specific {
            crate::handshake::SRT_CMD_KMREQ => {
                let Some(crypto) = conn.crypto.as_mut() else {
                    debug!(peer = %peer, "SRT: KMREQ without key material");
                    return;
                };
                let (state, response) = crypto.process_km(&ctrl.data);
                if state == KmState::Secured {
                    let packet = packet::user_defined_packet(
                        crate::handshake::SRT_CMD_KMRSP,
                        bytes::Bytes::from(response),
                        conn.peer_socket_id,
                        ctrl.timestamp,
                    );
                    self.send_control(&packet, peer).await;
                    info!(peer = %peer, "SRT: key refresh applied");
                } else if !self.config.enforced_encryption {
                    let packet = packet::user_defined_packet(
                        crate::handshake::SRT_CMD_KMRSP,
                        bytes::Bytes::from(response),
                        conn.peer_socket_id,
                        ctrl.timestamp,
                    );
                    self.send_control(&packet, peer).await;
                }
            }
            crate::handshake::SRT_CMD_KMRSP => {
                debug!(peer = %peer, "SRT: KM response received");
            }
            _ => {}
        }
    }

    async fn handle_handshake(
        &self,
        ctrl: &packet::ControlPacket,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
        next_socket_id: &mut u32,
    ) {
        let Some(handshake) = parse_handshake(&ctrl.data) else {
            debug!(peer = %peer, "SRT: failed to parse handshake");
            return;
        };

        match handshake.handshake_type {
            HandshakeType::Induction => {
                // HSv4 induction request. Rendezvous (WAVEAHAND) and HSv5
                // induction are not supported; the caller must start with
                // an HSv4 induction request.
                if handshake.version != SRT_VERSION_4 {
                    debug!(peer = %peer, version = handshake.version, "SRT: ignoring non-HSv4 induction");
                    return;
                }
                if !handshake.extensions.is_empty() {
                    debug!(peer = %peer, "SRT: induction carried extensions");
                }
                let existing = {
                    let pending = self
                        .pending_cookies
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    pending.get(&peer).and_then(|existing| {
                        (existing.caller_socket_id == handshake.socket_id
                            && existing.issued_at.elapsed() <= COOKIE_TTL)
                            .then_some((existing.cookie, existing.listener_socket_id))
                    })
                };
                let (cookie, listener_socket_id) = match existing {
                    Some((cookie, listener_socket_id)) => (cookie, listener_socket_id),
                    None => {
                        let listener_socket_id = *next_socket_id;
                        *next_socket_id += 1;
                        let cookie = generate_cookie(peer, ctrl.timestamp);
                        self.pending_cookies
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(
                                peer,
                                PendingCookie {
                                    cookie,
                                    listener_socket_id,
                                    caller_socket_id: handshake.socket_id,
                                    issued_at: Instant::now(),
                                },
                            );
                        (cookie, listener_socket_id)
                    }
                };
                let _ = listener_socket_id;
                let mut response = induction_response(handshake.socket_id, cookie);
                if let Some(key_len) = self.config.key_length() {
                    response.encryption = key_len.encryption_field();
                }
                let packet = handshake_control_packet(&response, handshake.socket_id, 0);
                self.send_control(&packet, peer).await;
                debug!(peer = %peer, cookie, "SRT: induction response sent");
            }
            HandshakeType::Conclusion => {
                self.handle_conclusion(ctrl, &handshake, peer, connections, next_socket_id)
                    .await;
            }
            HandshakeType::Agreement => {
                debug!(peer = %peer, "SRT: AGREEMENT received");
            }
            HandshakeType::Wavehand => {
                debug!(peer = %peer, "SRT: WAVEAHAND ignored (listener mode)");
            }
            HandshakeType::Rejection(code) => {
                warn!(peer = %peer, code, "SRT: peer rejected the connection");
            }
        }
    }

    async fn handle_conclusion(
        &self,
        ctrl: &packet::ControlPacket,
        handshake: &Handshake,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
        _next_socket_id: &mut u32,
    ) {
        let pending = self
            .pending_cookies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&peer);
        let Some(pending) = pending else {
            debug!(peer = %peer, "SRT: conclusion without induction");
            return;
        };
        let listener_socket_id = pending.listener_socket_id;

        if handshake.version != SRT_VERSION_5 {
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reject::VERSION,
                "conclusion is not HSv5",
            )
            .await;
            return;
        }
        if pending.issued_at.elapsed() > COOKIE_TTL || handshake.cookie != pending.cookie {
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reject::ROGUE,
                "cookie mismatch",
            )
            .await;
            return;
        }
        if handshake.socket_id == 0 || handshake.socket_id == pending.caller_socket_id {
            // The caller's socket id must be stable across induction and
            // conclusion; a mismatch means a spoofed or confused handshake.
            if handshake.socket_id == 0 {
                self.send_rejection(
                    peer,
                    handshake,
                    listener_socket_id,
                    reject::ROGUE,
                    "missing socket id",
                )
                .await;
                return;
            }
        }
        if handshake
            .extensions
            .iter()
            .any(|ext| matches!(ext, HsExtension::Invalid { .. }))
        {
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reject::ROGUE,
                "malformed handshake extension",
            )
            .await;
            return;
        }
        if handshake
            .extensions
            .iter()
            .any(|ext| matches!(ext, HsExtension::Unknown { cmd, .. } if *cmd == SRT_CMD_FILTER))
        {
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reject::FILTER,
                "packet filter is not supported",
            )
            .await;
            return;
        }
        if handshake
            .extensions
            .iter()
            .any(|ext| matches!(ext, HsExtension::Unknown { cmd, .. } if *cmd == SRT_CMD_GROUP))
        {
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reject::GROUP,
                "group membership is not supported",
            )
            .await;
            return;
        }
        if handshake.extensions.iter().any(|ext| {
            matches!(ext, HsExtension::Congestion(name) if !name.eq_ignore_ascii_case("live"))
        }) {
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reject::CONGESTION,
                "only live congestion control is supported",
            )
            .await;
            return;
        }
        let Some(caps) = handshake.hs_ext() else {
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reject::ROGUE,
                "missing HSREQ",
            )
            .await;
            return;
        };

        // --- HaiCrypt key material (listener = responder) -----------------
        let kmreq = handshake.extensions.iter().find_map(|ext| match ext {
            HsExtension::KmReq(data) => Some(data.clone()),
            _ => None,
        });
        let mut crypto_ctx: Option<SessionCrypto> = None;
        let mut kmrsp: Option<Vec<u8>> = None;
        match (&self.config.passphrase, kmreq) {
            (Some(passphrase), Some(message)) => match SessionCrypto::new(passphrase.as_bytes()) {
                Ok(mut context) => {
                    let (state, response) = context.process_km(&message);
                    if state == KmState::Secured {
                        crypto_ctx = Some(context);
                        kmrsp = Some(response);
                    } else if self.config.enforced_encryption {
                        let reason = if state == KmState::BadSecret {
                            reject::BADSECRET
                        } else {
                            reject::UNSECURE
                        };
                        self.send_rejection(
                            peer,
                            handshake,
                            listener_socket_id,
                            reason,
                            "key material rejected",
                        )
                        .await;
                        return;
                    } else {
                        kmrsp = Some(response);
                    }
                }
                Err(error) => {
                    self.send_rejection(
                        peer,
                        handshake,
                        listener_socket_id,
                        reject::BADSECRET,
                        &format!("invalid passphrase: {error}"),
                    )
                    .await;
                    return;
                }
            },
            (Some(_), None) => {
                if handshake.encryption != 0 || self.config.enforced_encryption {
                    self.send_rejection(
                        peer,
                        handshake,
                        listener_socket_id,
                        reject::UNSECURE,
                        "encryption required but no KMREQ",
                    )
                    .await;
                    return;
                }
                kmrsp = Some(vec![0, 0, 0, 0]);
            }
            (None, Some(_)) => {
                if self.config.enforced_encryption {
                    self.send_rejection(
                        peer,
                        handshake,
                        listener_socket_id,
                        reject::UNSECURE,
                        "peer requires encryption but no passphrase is configured",
                    )
                    .await;
                    return;
                }
                kmrsp = Some((KmState::NoSecret as u32).to_le_bytes().to_vec());
            }
            (None, None) => {}
        }

        let stream_id = handshake.stream_id().map(str::to_owned);
        if !self.stream_id_allowed(stream_id.as_deref(), peer) {
            let reason = if stream_id.is_none() {
                reject::ROGUE
            } else {
                reject::BADSECRET
            };
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reason,
                "stream id rejected",
            )
            .await;
            return;
        }
        if connections.len() >= self.config.max_connections {
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reject::RESOURCE,
                "connection limit reached",
            )
            .await;
            return;
        }

        let latency_ms = self
            .config
            .latency_ms
            .max(u32::from(caps.recv_latency_ms))
            .min(u32::from(u16::MAX));
        let mut tsbpd = TsbpdBuffer::new(latency_ms, self.config.tsbpd_max_buffered);
        tsbpd.set_tlpktdrop(self.config.tlpktdrop);
        tsbpd.set_time_base(Instant::now(), ctrl.timestamp);
        tsbpd.init(handshake.initial_sequence);

        let mut response = conclusion_response(
            listener_socket_id,
            handshake,
            HsExtFields {
                srt_version: SRT_VERSION,
                flags: HsFlags::live_defaults(),
                recv_latency_ms: latency_ms as u16,
                send_latency_ms: 0,
            },
        );
        if let Some(key_len) = self.config.key_length() {
            response.encryption = key_len.encryption_field();
        }
        if let Some(payload) = kmrsp {
            response.extensions.push(HsExtension::KmRsp(payload));
        }
        let packet = handshake_control_packet(&response, handshake.socket_id, ctrl.timestamp);
        self.send_control(&packet, peer).await;

        let now = Instant::now();
        let info = HandshakeInfo {
            srt_version: caps.srt_version,
            flags: caps.flags.0,
            recv_latency_ms: caps.recv_latency_ms,
            send_latency_ms: caps.send_latency_ms,
        };
        #[cfg(feature = "metrics")]
        if let Some(metrics) = &self.config.metrics {
            use std::sync::atomic::Ordering;
            metrics
                .srt_connections_total
                .fetch_add(1, Ordering::Relaxed);
        }
        connections.insert(
            peer,
            SrtConnection {
                peer_socket_id: handshake.socket_id,
                local_socket_id: listener_socket_id,
                start_time: now,
                state: ConnState::Established,
                tsbpd,
                demuxer: ts_demux::TsStreamDemuxer::new(),
                mount_path: None,
                mount_registered: false,
                mount: None,
                publisher: None,
                crypto: crypto_ctx,
                undecryptable: 0,
                stream_id: stream_id.clone(),
                peer_caps: info,
                latency_ms,
                last_recv_time: now,
                last_sent_time: now,
                last_keepalive_time: now,
                last_ack_time: now,
                last_nak_time: now,
                pkts_since_ack: 0,
                ack_seq: 0,
                last_ack_sent: None,
                rtt_us: 0,
                rtt_var_us: 0,
                packets_received: 0,
                bytes_received: 0,
                last_nak_losses: Vec::new(),
            },
        );
        info!(
            peer = %peer,
            stream_id = ?stream_id,
            latency_ms,
            srt_version = format!("{:#x}", caps.srt_version),
            "SRT: connection established"
        );
    }

    fn stream_id_allowed(&self, stream_id: Option<&str>, peer: SocketAddr) -> bool {
        let Some(stream_id) = stream_id else {
            return self.config.allowed_stream_ids.is_empty()
                && self.config.authenticator.is_none();
        };
        let allowed_by_list = self.config.allowed_stream_ids.is_empty()
            || self.config.allowed_stream_ids.iter().any(|allowed| {
                allowed == stream_id
                    || allowed.trim_start_matches('/') == stream_id.trim_start_matches('/')
            });
        if !allowed_by_list {
            return false;
        }
        self.config
            .authenticator
            .as_ref()
            .is_none_or(|authenticator| authenticator.authorize(stream_id, peer))
    }
}
