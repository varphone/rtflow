use super::connection::ConnState;
use super::*;

impl SrtServer {
    pub(super) async fn maintenance(
        &self,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
        ack_seq_counter: &mut u32,
    ) {
        let now = Instant::now();
        let mut expired = Vec::new();

        // Expire unredeemed SYN cookies so a flood of inductions cannot grow the table.
        {
            let mut cookies = self
                .pending_cookies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cookies.len() > 64 {
                cookies.retain(|_, (_, issued_at)| issued_at.elapsed() <= COOKIE_TTL);
            }
        }

        for (peer, conn) in connections.iter_mut() {
            // Keepalive timeout check
            if now.duration_since(conn.last_recv_time) > self.config.keepalive_timeout {
                expired.push(*peer);
                continue;
            }

            if conn.state != ConnState::Established {
                continue;
            }

            let timestamp = conn.start_time.elapsed().as_micros() as u32;

            // Periodic ACK
            let ack_due = now.duration_since(conn.last_ack_time).as_millis()
                >= self.config.ack_interval_ms as u128;
            if ack_due && conn.pkts_since_ack > 0 {
                let ack_num = *ack_seq_counter;
                *ack_seq_counter += 1;
                let ack = ack_packet(
                    ack_num,
                    conn.tsbpd.ack_number(),
                    conn.peer_socket_id,
                    timestamp,
                );
                let encoded = encode_control_packet(&ack);
                let _ = self.socket.send_to(&encoded, *peer).await;
                conn.last_ack_time = now;
                conn.pkts_since_ack = 0;
            }

            // Loss detection -> NAK, throttled and deduplicated: an
            // unchanged gap is only reported once, and never more often
            // than every 20ms.
            let losses = conn.tsbpd.detect_losses();
            if !losses.is_empty() && now.duration_since(conn.last_nak_time).as_millis() >= 20 {
                if losses != conn.last_nak_losses {
                    let nak = nak_packet(&losses, conn.peer_socket_id, timestamp);
                    let encoded = encode_control_packet(&nak);
                    let _ = self.socket.send_to(&encoded, *peer).await;
                    debug!("SRT: sent NAK for {} loss ranges to {peer}", losses.len());
                    conn.last_nak_losses = losses;
                }
                conn.last_nak_time = now;
            }

            // Release TSBPD ready packets -> demuxer
            let ready = conn.tsbpd.pop_ready();
            if !ready.is_empty() {
                for pkt in ready {
                    conn.demuxer.push(&pkt.payload);
                }
                self.deliver_units(conn, *peer).await;
            }
        }

        // Remove expired connections
        for peer in expired {
            warn!("SRT: connection {peer} timed out (no keepalive)");
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &self.config.metrics
                && let Some(conn) = connections.get(&peer)
                && conn.mount_registered
            {
                use std::sync::atomic::Ordering;
                metrics.srt_streams_active.fetch_sub(1, Ordering::Relaxed);
            }
            if let Some(conn) = connections.get(&peer)
                && conn.mount_registered
                && let Some(path) = &conn.mount_path
            {
                self.mounts.unregister(path);
                info!("SRT: unregistered mount {path} with expired connection");
            }
            connections.remove(&peer);
        }
    }

    pub(super) async fn handle_control(
        &self,
        ctrl: &packet::ControlPacket,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
        next_socket_id: &mut u32,
    ) {
        // Update last recv time for existing connections
        if let Some(conn) = connections.get_mut(&peer) {
            conn.last_recv_time = Instant::now();
        }

        match ctrl.control_type {
            ControlType::Handshake => {
                let Some(hs) = parse_handshake(&ctrl.data) else {
                    debug!("SRT: failed to parse handshake from {peer}");
                    return;
                };

                match hs.handshake_type {
                    HandshakeType::Induction => {
                        // Phase 1: Send induction response with cookie
                        let cookie = generate_cookie(peer, ctrl.timestamp);
                        // Remember the issued cookie so the conclusion can be verified.
                        {
                            let mut cookies = self
                                .pending_cookies
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            cookies.insert(peer, (cookie, Instant::now()));
                        }
                        let local_socket_id = *next_socket_id;
                        *next_socket_id += 1;

                        let response = induction_response(local_socket_id, cookie);
                        // Destination is the client's socket id so it accepts
                        // the response; the response's own SRT_SOCKID is ours.
                        let response_pkt =
                            handshake_control_packet(&response, hs.socket_id, ctrl.timestamp);
                        let encoded = encode_control_packet(&response_pkt);

                        if let Err(e) = self.socket.send_to(&encoded, peer).await {
                            warn!("SRT: failed to send induction response to {peer}: {e}");
                        } else {
                            debug!("SRT: induction response sent to {peer}");
                        }
                    }
                    HandshakeType::Conclusion => {
                        // Phase 2: Verify cookie and complete handshake.
                        let cookie_ok = {
                            let mut cookies = self
                                .pending_cookies
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let valid = cookies.get(&peer).is_some_and(|(expected, issued_at)| {
                                *expected == hs.cookie && issued_at.elapsed() <= COOKIE_TTL
                            });
                            // A conclusion redeems the cookie exactly once.
                            cookies.remove(&peer);
                            valid
                        };
                        if !cookie_ok {
                            warn!(
                                "SRT: rejected conclusion handshake from {peer} (invalid or expired cookie)"
                            );
                            return;
                        }
                        if connections.len() >= self.config.max_connections {
                            warn!(
                                "SRT: rejecting connection from {peer}: at capacity ({} connections)",
                                connections.len()
                            );
                            return;
                        }

                        let local_socket_id = *next_socket_id;
                        *next_socket_id += 1;

                        // Parse Stream ID from extension (for mount routing)
                        let stream_id = parse_stream_id(&ctrl.data);

                        // Publish authorization: reject callers whose stream
                        // id is not allowed (open mode when the list is
                        // empty). The caller learns about the rejection via
                        // a failed handshake.
                        let rejected = match &stream_id {
                            Some(actual) => {
                                !self.config.allowed_stream_ids.is_empty()
                                    && !self.config.allowed_stream_ids.iter().any(|allowed| {
                                        allowed == actual
                                            || allowed.trim_start_matches('/')
                                                == actual.trim_start_matches('/')
                                    })
                            }
                            // No stream id at all: only open mode accepts it.
                            None => !self.config.allowed_stream_ids.is_empty(),
                        };
                        if rejected {
                            warn!(
                                "SRT: rejected stream_id {stream_id:?} from {peer} (not allowed)"
                            );
                            return;
                        }
                        if let Some(authenticator) = &self.config.authenticator {
                            let allowed = stream_id
                                .as_deref()
                                .is_some_and(|id| authenticator.authorize(id, peer));
                            if !allowed {
                                warn!(
                                    "SRT: rejected stream_id {stream_id:?} from {peer} (authorization failed)"
                                );
                                return;
                            }
                        }

                        // The response carries the server's own initial packet
                        // sequence; echoing the client's ISN desynchronises
                        // its receive buffer.
                        let server_initial_sequence = (ctrl.timestamp
                            ^ (local_socket_id.wrapping_mul(2_654_435_761)))
                            & 0x7fff_ffff;
                        let response = conclusion_response(
                            local_socket_id,
                            server_initial_sequence,
                            hs.socket_id,
                        );
                        let response_pkt =
                            handshake_control_packet(&response, hs.socket_id, ctrl.timestamp);
                        let encoded = encode_control_packet(&response_pkt);

                        if let Err(e) = self.socket.send_to(&encoded, peer).await {
                            warn!("SRT: failed to send conclusion response to {peer}: {e}");
                        } else {
                            info!(
                                "SRT: connection established with {peer} (stream_id={stream_id:?})"
                            );

                            let now = Instant::now();
                            let mut tsbpd = TsbpdBuffer::new(
                                self.config.tsbpd_delay_ms,
                                self.config.tsbpd_max_buffered,
                            );
                            tsbpd.init(hs.initial_sequence);

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
                                    peer_socket_id: hs.socket_id,
                                    local_socket_id,
                                    start_time: now,
                                    state: ConnState::Established,
                                    tsbpd,
                                    demuxer: ts_demux::TsStreamDemuxer::new(),
                                    mount_path: None,
                                    mount_registered: false,
                                    mount: None,
                                    stream_id,
                                    last_recv_time: now,
                                    last_ack_time: now,
                                    last_nak_time: now,
                                    last_nak_losses: Vec::new(),
                                    ack_seq: 0,
                                    pkts_since_ack: 0,
                                },
                            );
                        }
                    }
                    _ => {
                        debug!(
                            "SRT: unhandled handshake type from {peer}: {:?}",
                            hs.handshake_type
                        );
                    }
                }
            }
            ControlType::Keepalive => {
                if let Some(conn) = connections.get(&peer) {
                    let timestamp = conn.start_time.elapsed().as_micros() as u32;
                    let response = keepalive_packet(conn.peer_socket_id, timestamp);
                    let encoded = encode_control_packet(&response);
                    let _ = self.socket.send_to(&encoded, peer).await;
                }
            }
            ControlType::AckAck => {
                // Sender acknowledged our ACK - nothing to do
                debug!("SRT: received ACKACK from {peer}");
            }
            ControlType::Shutdown => {
                info!("SRT: peer {peer} requested shutdown");
                if let Some(conn) = connections.get_mut(&peer) {
                    conn.state = ConnState::Closing;
                    #[cfg(feature = "metrics")]
                    if let Some(metrics) = &self.config.metrics
                        && conn.mount_registered
                    {
                        use std::sync::atomic::Ordering;
                        metrics.srt_streams_active.fetch_sub(1, Ordering::Relaxed);
                    }
                    if conn.mount_registered
                        && let Some(path) = conn.mount_path.clone()
                    {
                        self.mounts.unregister(&path);
                        info!("SRT: unregistered mount {path} on peer shutdown");
                    }
                }
                connections.remove(&peer);
            }
            _ => {
                debug!(
                    "SRT: unhandled control type from {peer}: {:?}",
                    ctrl.control_type
                );
            }
        }
    }

    pub(super) async fn handle_data(
        &self,
        data_pkt: &packet::DataPacket,
        peer: SocketAddr,
        connections: &mut HashMap<SocketAddr, SrtConnection>,
    ) {
        let Some(conn) = connections.get_mut(&peer) else {
            debug!("SRT: data from unknown peer {peer}");
            return;
        };

        conn.last_recv_time = Instant::now();

        if conn.state != ConnState::Established {
            return;
        }

        // Insert into TSBPD reorder buffer (retransmit detection is
        // wrapping-aware so a packet just before a sequence-space wrap is
        // not mistaken for a retransmission of an old one). Delivery is
        // scheduled from the sender-clock timestamp, absorbing jitter.
        let is_retransmit = tsbpd::seq_before(data_pkt.sequence, conn.tsbpd.ack_number());
        if conn.tsbpd.push(
            data_pkt.sequence,
            data_pkt.payload.clone(),
            data_pkt.timestamp,
            is_retransmit,
        ) {
            conn.pkts_since_ack += 1;
        }

        // Try to release immediately ready packets (low-latency path)
        let ready = conn.tsbpd.pop_ready();
        if !ready.is_empty() {
            for pkt in ready {
                conn.demuxer.push(&pkt.payload);
            }
            self.deliver_units(conn, peer).await;
        }
    }

    /// Deliver completed access units from the demuxer to the mount.
    pub(super) async fn deliver_units(&self, conn: &mut SrtConnection, peer: SocketAddr) {
        let units = conn.demuxer.take_units();
        if units.is_empty() {
            return;
        }

        // Register mount if not done yet
        if !conn.mount_registered {
            if let Some(desc) = conn.demuxer.description() {
                // Use stream_id for mount path if available, else fallback to port-based
                let mount_path = match &conn.stream_id {
                    Some(sid) => {
                        if sid.starts_with('/') {
                            sid.clone()
                        } else {
                            format!("/{sid}")
                        }
                    }
                    None => format!("/srt/{}", peer.port()),
                };

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
                    Err(e) => {
                        warn!("SRT: failed to register mount: {e}");
                    }
                }
            }
        } else if let Some(mount) = &conn.mount {
            // Fast path: deliver straight through the cached mount handle.
            for unit in units {
                let _ = mount.publish_cached(unit);
            }
        }
    }
}

/// Parse Stream ID from HSv5 conclusion handshake extension.
///
/// The Stream ID is carried in a SRT_CMD_SID extension block:
/// - Extension type 0x0005 (SRT_CMD_SID)
/// - Followed by the stream ID as UTF-8 string (padded to 4-byte boundary)
///
/// Common format: `#!::u=user,r=live/stream,h=host`
/// We extract the `r=` field as the mount path.
pub(super) fn parse_stream_id(hs_data: &[u8]) -> Option<String> {
    // The handshake data has a fixed 48-byte header, then extensions follow.
    // Each extension: 2 bytes type, 2 bytes length (in 32-bit words), then data.
    if hs_data.len() < 48 {
        return None;
    }

    let mut offset = 48; // skip handshake header
    while offset + 4 <= hs_data.len() {
        let ext_type = u16::from_be_bytes([hs_data[offset], hs_data[offset + 1]]);
        let ext_len_words = u16::from_be_bytes([hs_data[offset + 2], hs_data[offset + 3]]);
        let ext_len_bytes = (ext_len_words as usize) * 4;
        offset += 4;

        if offset + ext_len_bytes > hs_data.len() {
            break;
        }

        // SRT_CMD_SID = 5
        if ext_type == 5 {
            let raw = &hs_data[offset..offset + ext_len_bytes];
            // Trim null padding
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let sid = String::from_utf8_lossy(&raw[..end]).to_string();

            // Parse `#!::` format: extract r= field
            if let Some(kv_part) = sid.strip_prefix("#!::") {
                for pair in kv_part.split(',') {
                    if let Some(resource) = pair.strip_prefix("r=") {
                        return Some(resource.to_string());
                    }
                }
                // No r= field, use the whole thing after #!::
                return Some(kv_part.to_string());
            }
            return Some(sid);
        }

        offset += ext_len_bytes;
    }

    None
}
