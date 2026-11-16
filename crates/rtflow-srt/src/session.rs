use super::connection::{ConnState, HandshakeInfo};
use super::*;

impl SrtServer {
    /// Periodic ACK/NAK/keepalive/timeout processing.
    pub(super) async fn maintenance(
        &self,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
        _ack_seq_counter: &mut u32,
    ) {
        let now = Instant::now();

        // Expire unredeemed SYN cookies so a flood of inductions cannot grow
        // the table.
        {
            let mut pending = self
                .pending_cookies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.retain(|_, cookie| cookie.issued_at.elapsed() <= COOKIE_TTL);
        }

        let mut expired = Vec::new();
        for (peer, conn) in connections.iter_mut() {
            if now.duration_since(conn.last_recv_time) > self.config.peer_idle_timeout {
                expired.push(*peer);
                continue;
            }
            if conn.state != ConnState::Established {
                continue;
            }
            let timestamp = conn.start_time.elapsed().as_micros() as u32;

            // Keepalive when the connection has been quiet.
            if now.duration_since(conn.last_sent_time) >= self.config.keepalive_interval {
                let keepalive = keepalive_packet(conn.peer_socket_id, timestamp);
                self.send_control(&keepalive, *peer).await;
                conn.last_sent_time = now;
                conn.last_keepalive_time = now;
            }

            // Release play-time packets (TLPKTDROP may skip missing ones).
            let skipped_before = conn.tsbpd.skipped_count();
            let ready = conn.tsbpd.pop_ready();
            let skipped = conn.tsbpd.skipped_count() > skipped_before;
            if !ready.is_empty() {
                for pkt in ready {
                    conn.demuxer.push(&pkt.payload);
                }
                self.deliver_units(conn, *peer).await;
            }
            if skipped {
                // Fake ACK: tell the sender we moved past the gap so it can
                // release its retransmission buffer.
                self.send_ack(conn, *peer, timestamp).await;
            }

            // Loss reports (NAK) for gaps that may still be recovered.
            let losses = conn.tsbpd.detect_losses();
            if !losses.is_empty()
                && now.duration_since(conn.last_nak_time).as_millis()
                    >= u128::from(self.config.nak_interval_ms)
                && losses != conn.last_nak_losses
            {
                let nak = nak_packet(&losses, conn.peer_socket_id, timestamp);
                self.send_control(&nak, *peer).await;
                debug!(peer = %peer, ranges = losses.len(), "SRT: sent NAK");
                conn.last_nak_losses = losses;
                conn.last_nak_time = now;
            }

            // Full ACK cadence.
            if conn.pkts_since_ack > 0
                && now.duration_since(conn.last_ack_time).as_millis()
                    >= u128::from(self.config.ack_interval_ms)
            {
                self.send_ack(conn, *peer, timestamp).await;
            }
        }

        for peer in expired {
            warn!(peer = %peer, "SRT: connection timed out (no keepalive)");
            if let Some(conn) = connections.get(&peer) {
                self.log_connection_stats(conn, peer, "timeout");
                if conn.mount_registered
                    && let Some(path) = &conn.mount_path
                {
                    self.mounts.unregister(path);
                    info!("SRT: unregistered mount {path} with expired connection");
                }
            }
            connections.remove(&peer);
        }
    }

    async fn send_control(&self, packet: &ControlPacket, peer: SocketAddr) {
        let encoded = encode_control_packet(packet);
        if let Err(error) = self.socket.send_to(&encoded, peer).await {
            debug!(peer = %peer, %error, "SRT: control packet send failed");
        }
    }

    /// Send a full ACK and arm the RTT probe.
    async fn send_ack(&self, conn: &mut SrtConnection, peer: SocketAddr, timestamp: u32) {
        conn.ack_seq = conn.ack_seq.wrapping_add(1).max(1);
        let elapsed = conn.start_time.elapsed().as_secs_f64().max(0.001);
        let packet_rate = (conn.packets_received as f64 / elapsed) as u32;
        let byte_rate = (conn.bytes_received as f64 / elapsed) as u32;
        let available = conn
            .tsbpd
            .capacity()
            .saturating_sub(conn.tsbpd.buffered_count());
        let fields = AckFields {
            ack_sequence: conn.ack_seq,
            last_ack_sequence: conn.tsbpd.ack_number(),
            rtt_us: conn.rtt_us,
            rtt_var_us: conn.rtt_var_us,
            available_buffer: available as u32,
            packet_rate,
            link_capacity: packet_rate,
            byte_rate,
        };
        let ack = ack_packet(&fields, conn.peer_socket_id, timestamp);
        self.send_control(&ack, peer).await;
        conn.last_ack_time = Instant::now();
        conn.pkts_since_ack = 0;
        conn.last_ack_sent = Some((conn.ack_seq, Instant::now()));
    }

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
        warn!(
            peer = %peer,
            reason,
            detail,
            "SRT: rejected connection"
        );
        self.send_control(&packet, peer).await;
    }

    fn log_connection_stats(&self, conn: &SrtConnection, peer: SocketAddr, reason: &str) {
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
            peer_recv_latency_ms = conn.peer_caps.recv_latency_ms,
            peer_send_latency_ms = conn.peer_caps.send_latency_ms,
            "SRT: connection closed"
        );
    }

    pub(super) async fn handle_control(
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
                    self.mounts.unregister(path);
                }
                connections.remove(&peer);
            }
            ControlType::CongestionWarning => {
                debug!(peer = %peer, "SRT: congestion warning");
            }
            _ => {
                debug!(peer = %peer, kind = ?ctrl.control_type, "SRT: unhandled control type");
            }
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
                let response = induction_response(handshake.socket_id, cookie);
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
        if handshake.requests_encryption() {
            self.send_rejection(
                peer,
                handshake,
                listener_socket_id,
                reject::CRYPTO,
                "encryption is not supported",
            )
            .await;
            return;
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

        let response = conclusion_response(
            listener_socket_id,
            handshake,
            HsExtFields {
                srt_version: SRT_VERSION,
                flags: HsFlags::live_defaults(),
                recv_latency_ms: latency_ms as u16,
                send_latency_ms: 0,
            },
        );
        let packet = handshake_control_packet(&response, handshake.socket_id, ctrl.timestamp);
        self.send_control(&packet, peer).await;

        let now = Instant::now();
        let info = HandshakeInfo {
            srt_version: caps.srt_version,
            flags: caps.flags.0,
            recv_latency_ms: caps.recv_latency_ms,
            send_latency_ms: caps.send_latency_ms,
        };
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

    pub(super) async fn handle_data(
        &self,
        data_pkt: &packet::DataPacket,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
    ) {
        let Some(conn) = connections.get_mut(&peer) else {
            debug!(peer = %peer, "SRT: data from unknown peer");
            return;
        };
        conn.last_recv_time = Instant::now();
        if conn.state != ConnState::Established {
            return;
        }
        // The destination socket id must be the one we assigned in the
        // conclusion response; anything else belongs to another connection.
        if data_pkt.socket_id != conn.local_socket_id {
            debug!(
                peer = %peer,
                socket_id = data_pkt.socket_id,
                expected = conn.local_socket_id,
                "SRT: data for a different socket id"
            );
            return;
        }

        let accepted = conn.tsbpd.push(
            data_pkt.sequence,
            data_pkt.payload.clone(),
            data_pkt.timestamp,
            data_pkt.retransmitted,
        );
        if accepted {
            conn.pkts_since_ack += 1;
            conn.packets_received += 1;
            conn.bytes_received += data_pkt.payload.len() as u64;
        }

        let skipped_before = conn.tsbpd.skipped_count();
        let ready = conn.tsbpd.pop_ready();
        if !ready.is_empty() {
            for pkt in ready {
                conn.demuxer.push(&pkt.payload);
            }
            self.deliver_units(conn, peer).await;
        }
        if conn.tsbpd.skipped_count() > skipped_before {
            let timestamp = conn.start_time.elapsed().as_micros() as u32;
            self.send_ack(conn, peer, timestamp).await;
        }
    }

    /// Deliver completed access units from the demuxer to the mount.
    pub(super) async fn deliver_units(&self, conn: &mut SrtConnection, peer: SocketAddr) {
        let units = conn.demuxer.take_units();
        if units.is_empty() {
            return;
        }

        if !conn.mount_registered {
            if let Some(desc) = conn.demuxer.description() {
                let mount_path = mount_path_for(&conn.stream_id, peer);
                let config = MountConfig::Publisher {
                    description: desc,
                    allow_replace_after_disconnect: true,
                };
                match self.mounts.register(mount_path.clone().into(), config) {
                    Ok(handle) => {
                        info!("SRT: registered mount {mount_path}");
                        #[cfg(feature = "metrics")]
                        if let Some(metrics) = &self.config.metrics {
                            use std::sync::atomic::Ordering;
                            metrics.srt_streams_active.fetch_add(1, Ordering::Relaxed);
                        }
                        conn.mount_path = Some(mount_path);
                        conn.mount_registered = true;
                        conn.mount = Some(handle.mount().clone());
                        if let Ok(publisher) = handle.publisher() {
                            for unit in units {
                                let _ = publisher.push(unit);
                            }
                        }
                    }
                    Err(error) => warn!(%error, "SRT: failed to register mount"),
                }
            }
        } else if let Some(mount) = &conn.mount {
            for unit in units {
                let _ = mount.publish_cached(unit);
            }
        }
    }
}

/// Mount path for a Stream ID: strip the authentication query and any
/// `#!::` key-value wrapper (`r=` is the resource).
pub(super) fn mount_path_for(stream_id: &Option<String>, peer: SocketAddr) -> String {
    let Some(stream_id) = stream_id else {
        return format!("/srt/{}", peer.port());
    };
    let resource = parse_stream_id_string(stream_id);
    let path = resource.split('?').next().unwrap_or(&resource);
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

/// Extract the resource from an SRT Stream ID (`live/cam?token=...` or
/// `#!::u=user,r=live/cam,h=host`).
pub(super) fn parse_stream_id_string(stream_id: &str) -> String {
    let Some(kv) = stream_id.strip_prefix("#!::") else {
        return stream_id.to_owned();
    };
    for pair in kv.split(',') {
        if let Some(resource) = pair.strip_prefix("r=") {
            return resource.to_owned();
        }
    }
    kv.to_owned()
}

/// Parse the (first, last) sequence range of a DROPREQ packet.
fn drop_request_range(packet: &ControlPacket) -> Option<(u32, u32)> {
    if packet.data.len() < 8 {
        return None;
    }
    let first = u32::from_be_bytes(packet.data[0..4].try_into().ok()?) & 0x7FFF_FFFF;
    let last = u32::from_be_bytes(packet.data[4..8].try_into().ok()?) & 0x7FFF_FFFF;
    Some((first, last))
}
