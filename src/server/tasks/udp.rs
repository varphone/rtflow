use super::*;

/// Small RTP retransmission cache for RFC 4585 NACK feedback (UDP/plain RTP).
struct RtpCache {
    max: usize,
    entries: VecDeque<(u16, Bytes)>,
}

impl RtpCache {
    fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            entries: VecDeque::new(),
        }
    }

    fn insert(&mut self, sequence: u16, packet: Bytes) {
        if self.entries.len() >= self.max {
            self.entries.pop_front();
        }
        self.entries.push_back((sequence, packet));
    }

    fn get(&self, sequence: u16) -> Option<&Bytes> {
        self.entries
            .iter()
            .find(|(current, _)| *current == sequence)
            .map(|(_, packet)| packet)
    }
}

struct PublisherLease(Arc<Mount>);
impl Drop for PublisherLease {
    fn drop(&mut self) {
        self.0
            .publishers
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

pub(crate) fn spawn_udp_sender_from(
    mount: Arc<Mount>,
    input: SenderInput,
    pair: UdpPair,
    rtp_target: SocketAddr,
    rtcp_target: SocketAddr,
    track: TrackDescription,
    anchor_ntp: Option<u64>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let SenderInput {
            mut receiver,
            startup,
            sequence,
            mut crypto,
        } = input;
        // Frames the startup snapshot already delivered; the broadcast ring
        // will replay the same frames, so skip them.
        let startup_keys = startup_dedup_keys(&startup);
        let mut startup = VecDeque::from(startup);
        // Pace startup frames at their media rate so a burst of cached
        // frames does not slam the receiver all at once; the bounded
        // startup window (STARTUP_WINDOW) keeps this short.
        let mut startup_clock: Option<tokio::time::Instant> = None;
        let mut startup_base_pts = std::time::Duration::ZERO;
        let ssrc = initial_ssrc(track.id);
        let _bye = ByeGuard {
            pair: pair.clone(),
            target: rtcp_target,
            ssrc,
        };
        let mut packetizer = match Packetizer::new(
            track.codec,
            track.payload_type,
            ssrc,
            if track.codec == crate::media::Codec::Jpeg {
                60_000
            } else {
                1200
            },
        ) {
            Ok(value) => value.with_dimensions(
                u16::try_from(track.width.unwrap_or(0)).unwrap_or(0),
                u16::try_from(track.height.unwrap_or(0)).unwrap_or(0),
            ),
            Err(_) => return,
        };
        let mut awaiting_key = track.kind == MediaKind::Video;
        let mut sent_packets = 0u32;
        let mut sent_octets = 0u32;
        let mut last_rtp_timestamp = 0u32;
        let mut ntp_clock: Option<RtpNtpClock> = None;
        let mut nack_cache = RtpCache::new(512);
        // Cache the protected bytes too: retransmitting an SRTP packet with
        // its original index is safe (the original was lost) and avoids
        // reusing the sender's keystream state.
        let retransmit = true;
        let mut rtcp_buffer = BytesMut::zeroed(2048);
        // Last sender report's NTP timestamp, for periodic receiver reports
        // (LSR reference).
        let last_sr_ntp: Option<u64> = None;
        let mut report_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        report_tick.tick().await;
        let mount_cancel = mount.cancel_token();
        loop {
            tokio::select! {
                biased;
                _ = mount_cancel.cancelled() => {
                    // Mount unregistered or server shutting down.
                    return;
                }
                _ = report_tick.tick() => {
                    if sent_packets > 0 {
                        let report = ntp_clock.as_mut().map(|clock| {
                            encode_sender_report(
                                ssrc,
                                clock.ntp_for(last_rtp_timestamp),
                                last_rtp_timestamp,
                                sent_packets,
                                sent_octets,
                            )
                        });
                        if let Some(Ok(report)) = report
                            && let Ok(report) = crypto.protect_rtcp(report)
                            && pair.rtcp.send_to(&report, rtcp_target).await.is_ok()
                        {
                            mount.stats.add_sender_reports(1);
                        }
                    }
                    // Periodic receiver report (RFC 3550 §6.4.2): keep the
                    // peer's sender statistics alive even when we receive no
                    // SRs in between.
                    if let Some(sr_ntp) = last_sr_ntp
                        && let Ok(wire) = CompoundPacket::new(vec![RtcpPacket::ReceiverReport {
                            ssrc,
                        }])
                        .and_then(|packet| packet.try_encode())
                        && let Ok(wire) = crypto.protect_rtcp(wire)
                        && pair.rtcp.send_to(&wire, rtcp_target).await.is_ok()
                    {
                        mount.stats.add_receiver_reports(1);
                        let _ = sr_ntp;
                    }
                }
                report = pair.rtcp.recv_from(&mut rtcp_buffer) => {
                    let Ok((size, source)) = report else { return; };
                    if source != rtcp_target {
                        mount.stats.add_dropped(1);
                        continue;
                    }
                    let payload = bytes::Bytes::copy_from_slice(&rtcp_buffer[..size]);
                    let Ok(payload) = crypto.unprotect_rtcp(payload) else {
                        mount.stats.add_dropped(1);
                        continue;
                    };
                    if let Ok(packets) = RtcpPacket::parse(payload) {
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
                        if retransmit {
                            for packet in &packets {
                                let Some(entries) = packet.nack_entries() else {
                                    continue;
                                };
                                for entry in entries {
                                    let requested = std::iter::once(entry.packet_id).chain(
                                        (0..16)
                                            .filter(|bit| entry.lost & (1 << bit) != 0)
                                            .map(|bit| entry.packet_id.wrapping_add(1 + bit as u16)),
                                    );
                                    for sequence in requested {
                                        if let Some(cached) = nack_cache.get(sequence).cloned()
                                            && pair.rtp.send_to(&cached, rtp_target).await.is_ok()
                                        {
                                            sent_packets = sent_packets.wrapping_add(1);
                                            sent_octets =
                                                sent_octets.wrapping_add(cached.len() as u32);
                                        }
                                    }
                                }
                            }
                        }
                        if packets.iter().any(|packet| {
                            matches!(
                                packet,
                                RtcpPacket::RtpFeedback { fmt: 1, .. }
                                    | RtcpPacket::PayloadFeedback { fmt: 1 | 4, .. }
                            )
                        }) {
                            mount.request_keyframe(track.id);
                        }
                    }
                }
                received = async {
                    match startup.pop_front() {
                        Some(frame) => {
                            // Pace startup frames at their media rate (see
                            // startup_clock above); live frames are not paced.
                            if let Some(clock) = startup_clock {
                                let due = clock + frame.pts.saturating_sub(startup_base_pts);
                                tokio::time::sleep_until(due).await;
                            } else {
                                startup_clock = Some(tokio::time::Instant::now());
                                startup_base_pts = frame.pts;
                            }
                            // Startup snapshot frames must NOT be filtered
                            // by the broadcast dedup below: the snapshot
                            // was already delivered through this sender.
                            Ok((frame, false))
                        }
                        None => receiver.recv().await.map(|frame| (frame, true)),
                    }
                } => {
                    let (frame, from_broadcast) = match received {
                        Ok(frame) => frame,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            mount.stats.add_dropped(count);
                            awaiting_key = track.kind == MediaKind::Video;
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
                    // Skip broadcast duplicates of snapshot frames only;
                    // snapshot frames themselves are delivered as replayed.
                    if from_broadcast
                        && startup_keys.contains(&(frame.track_id, frame.rtp_timestamp))
                    {
                        continue;
                    }
                    if frame.track_id != track.id || (awaiting_key && !frame.is_keyframe) {
                        continue;
                    }
                    if frame.data.len() > mount.max_queued_bytes_per_session {
                        mount.stats.add_dropped(1);
                        awaiting_key = track.kind == MediaKind::Video;
                        continue;
                    }
                    awaiting_key = false;
                    let mut consecutive_send_failures = 0u32;
                    let packets = match packetizer.packetize(&frame) {
                        Ok(packets) => packets,
                        Err(error) => {
                            mount.stats.add_dropped(1);
                            tracing::warn!(track = track.id, %error, "RTP packetization failed");
                            continue;
                        }
                    };
                    {
                        let mut wire = BytesMut::with_capacity(1500);
                        let mut frame_packets = 0u64;
                        let mut frame_octets = 0u64;
                        for mut packet in packets {
                            packet.header.sequence = sequence.fetch_add(1, Ordering::Relaxed);
                            packet.encode_into(&mut wire);
                            let plain = wire.split().freeze();
                            let protected = match crypto.protect_rtp(plain) {
                                Ok(wire) => wire,
                                Err(error) => {
                                    mount.stats.add_dropped(1);
                                    tracing::warn!(track = track.id, %error, "SRTP protect failed");
                                    continue;
                                }
                            };
                            let cached = retransmit.then(|| protected.clone());
                            let sequence_number = packet.header.sequence;
                            if pair.rtp.send_to(&protected, rtp_target).await.is_err() {
                                // A persistently unreachable target should not
                                // keep the sender spinning silently.
                                consecutive_send_failures = consecutive_send_failures.saturating_add(1);
                                if consecutive_send_failures >= 100 {
                                    tracing::warn!(
                                        %rtp_target,
                                        "RTP sender giving up after 100 consecutive send failures"
                                    );
                                    return;
                                }
                                continue;
                            }
                            consecutive_send_failures = 0;
                            if let Some(cached) = cached {
                                nack_cache.insert(sequence_number, cached);
                            }
                            sent_packets = sent_packets.wrapping_add(1);
                            sent_octets = sent_octets.wrapping_add(packet.payload.len() as u32);
                            last_rtp_timestamp = packet.header.timestamp;
                            if ntp_clock.is_none() {
                                ntp_clock = RtpNtpClock::new_with_anchor(
                                    last_rtp_timestamp,
                                    track.clock_rate,
                                    anchor_ntp,
                                );
                            }
                            frame_packets = frame_packets.saturating_add(1);
                            frame_octets = frame_octets.saturating_add(packet.payload.len() as u64);
                            if sent_packets == 1
                                && let Some(clock) = ntp_clock.as_mut()
                                && let Ok(report) = encode_sender_report(
                                    ssrc,
                                    clock.ntp_for(last_rtp_timestamp),
                                    last_rtp_timestamp,
                                    sent_packets,
                                    sent_octets,
                                )
                                && let Ok(report) = crypto.protect_rtcp(report)
                                && pair.rtcp.send_to(&report, rtcp_target).await.is_ok()
                            {
                                mount.stats.add_sender_reports(1);
                            }
                        }
                        mount.stats.add_rtp(frame_packets, frame_octets);
                    }
                }
            }
        }
    })
}

pub(crate) fn spawn_udp_receiver(
    mount: Arc<Mount>,
    pair: UdpPair,
    expected_rtp_source: SocketAddr,
    track: TrackDescription,
    mut crypto: MediaCrypto,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if !matches!(mount.config, MountConfig::Publisher { .. }) {
            return;
        }
        mount
            .publishers
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let _lease = PublisherLease(mount.clone());
        let mut depacketizer = Depacketizer::new(track.codec, track.id, track.clock_rate);
        let mut buffer = BytesMut::zeroed(65_536);
        let mut expected_ssrc = None;
        while let Ok((size, source)) = pair.rtp.recv_from(&mut buffer).await {
            if source != expected_rtp_source {
                mount.stats.add_dropped(1);
                continue;
            }
            let payload = bytes::Bytes::copy_from_slice(&buffer[..size]);
            let payload = match crypto.unprotect_rtp(payload) {
                Ok(payload) => payload,
                Err(_) => {
                    mount.stats.add_dropped(1);
                    continue;
                }
            };
            let packet = match RtpPacket::parse(payload) {
                Ok(packet) => packet,
                Err(_) => {
                    mount.stats.add_dropped(1);
                    continue;
                }
            };
            if packet.header.payload_type != track.payload_type
                && !(packet.header.payload_type >= 96 && track.payload_type >= 96)
                && !matches!(
                    track.codec,
                    crate::media::Codec::H263 | crate::media::Codec::Jpeg
                )
            {
                mount.stats.add_dropped(1);
                continue;
            }
            if let Some(ssrc) = expected_ssrc {
                if packet.header.ssrc != ssrc {
                    mount.stats.add_dropped(1);
                    continue;
                }
            } else {
                expected_ssrc = Some(packet.header.ssrc);
            }
            if let Ok(frames) = depacketizer.push(packet) {
                for frame in frames {
                    if mount.publish_cached(frame).is_ok() {
                        mount.stats.add_accepted(1);
                    } else {
                        mount.stats.add_dropped(1);
                    }
                }
            }
        }
    })
}
