use super::connection::ConnState;
use super::*;

mod handshake;
use crate::packet::{
    AckFields, ControlPacket, ack_packet, encode_control_packet, keepalive_packet, nak_packet,
};

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
                #[cfg(feature = "metrics")]
                if let Some(metrics) = &self.config.metrics {
                    use std::sync::atomic::Ordering;
                    metrics
                        .srt_skipped_packets
                        .fetch_add(conn.tsbpd.skipped_count(), Ordering::Relaxed);
                }
                // Fake ACK: tell the sender we moved past the gap so it can
                // release its retransmission buffer.
                self.send_ack(conn, *peer, timestamp).await;
            }

            // Loss reports (NAK) for gaps that may still be recovered.
            let losses = conn.tsbpd.detect_losses();
            if !losses.is_empty() {
                debug!(peer = %peer, ranges = losses.len(), first = losses[0].0, last = losses[0].1, since_nak_ms = now.duration_since(conn.last_nak_time).as_millis(), "SRT: loss detected");
            }
            if !losses.is_empty()
                && now.duration_since(conn.last_nak_time).as_millis()
                    >= u128::from(self.config.nak_interval_ms)
            {
                let nak = nak_packet(&losses, conn.peer_socket_id, timestamp);
                self.send_control(&nak, *peer).await;
                debug!(peer = %peer, ranges = losses.len(), "SRT: sent NAK");
                conn.last_nak_losses = losses.clone();
                conn.last_nak_time = now;
                #[cfg(feature = "metrics")]
                if let Some(metrics) = &self.config.metrics {
                    use std::sync::atomic::Ordering;
                    let total: u64 = losses
                        .iter()
                        .map(|(start, end)| {
                            u64::from(tsbpd::seq_distance(*start, *end).wrapping_add(1))
                        })
                        .sum();
                    metrics.srt_loss_packets.fetch_add(total, Ordering::Relaxed);
                }
            }

            // Full ACK cadence.
            if conn.pkts_since_ack > 0
                && now.duration_since(conn.last_ack_time).as_millis()
                    >= u128::from(self.config.ack_interval_ms)
            {
                self.send_ack(conn, *peer, timestamp).await;
            }
        }

        // Refresh the monitoring snapshot before dropping expired entries so
        // callers see the final counters once, then they disappear.
        let snapshot: Vec<SrtConnectionStats> = connections
            .iter()
            .map(|(peer, conn)| {
                let stats = conn.tsbpd.stats();
                SrtConnectionStats {
                    peer: *peer,
                    stream_id: conn.stream_id.clone(),
                    mount: conn.mount_path.clone(),
                    encrypted: conn.crypto.is_some(),
                    latency_ms: conn.latency_ms,
                    rtt_us: conn.rtt_us,
                    packets_received: conn.packets_received,
                    bytes_received: conn.bytes_received,
                    bytes_sent: 0,
                    delivered: stats.delivered,
                    skipped: stats.skipped,
                    loss_events: stats.loss_events,
                    retransmitted: conn.retransmitted,
                    undecryptable: conn.undecryptable,
                    duration_secs: conn.start_time.elapsed().as_secs(),
                    srt_version: conn.peer_caps.srt_version,
                }
            })
            .collect();
        *self
            .stats
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;

        for peer in expired {
            warn!(peer = %peer, "SRT: connection timed out (no keepalive)");
            if let Some(conn) = connections.get(&peer) {
                self.log_connection_stats(conn, peer, "timeout");
                if conn.mount_registered
                    && let Some(path) = &conn.mount_path
                {
                    self.release_stream_metric();
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
        if data_pkt.retransmitted {
            conn.retransmitted += 1;
        }
        if data_pkt.socket_id != conn.local_socket_id {
            debug!(
                peer = %peer,
                socket_id = data_pkt.socket_id,
                expected = conn.local_socket_id,
                "SRT: data for a different socket id"
            );
            return;
        }

        let mut payload = data_pkt.payload.clone();
        match (&mut conn.crypto, data_pkt.key_flags & 0x03) {
            (Some(crypto), 1..=3) => {
                let mut buffer = payload.to_vec();
                if !crypto.decrypt(data_pkt.sequence, data_pkt.key_flags, &mut buffer) {
                    conn.undecryptable += 1;
                    debug!(
                        peer = %peer,
                        sequence = data_pkt.sequence,
                        "SRT: undecryptable data packet"
                    );
                    return;
                }
                payload = bytes::Bytes::from(buffer);
            }
            (Some(_), _) => {
                // A secured link's legitimate data always carries KK bits.
                conn.undecryptable += 1;
                debug!(peer = %peer, "SRT: cleartext packet on an encrypted link");
                return;
            }
            (None, 0) => {}
            (None, _) => {
                conn.undecryptable += 1;
                debug!(peer = %peer, "SRT: encrypted packet without key material");
                return;
            }
        }

        let accepted = conn.tsbpd.push(
            data_pkt.sequence,
            payload,
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
                        match handle.publisher() {
                            Ok(publisher) => {
                                for unit in units {
                                    let _ = publisher.push(unit);
                                }
                                // Keep the lease: mount snapshots and the
                                // recording supervisor use publisher_connected.
                                conn.publisher = Some(publisher);
                            }
                            Err(error) => warn!(%error, "SRT: publisher attach failed"),
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
