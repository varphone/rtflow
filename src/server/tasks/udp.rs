use super::*;

struct PublisherLease(Arc<Mount>);
impl Drop for PublisherLease {
    fn drop(&mut self) {
        self.0
            .publishers
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[allow(clippy::too_many_arguments)]
async fn retransmit_cached(
    cache: &RetransmitCache,
    socket: &tokio::net::UdpSocket,
    target: SocketAddr,
    sequences: &[u16],
    rtx: Option<crate::media::RtxConfig>,
    rtx_sequence: &mut u16,
    crypto: &mut MediaCrypto,
) -> (u32, u32) {
    let mut pending = Vec::with_capacity(sequences.len());
    for sequence in sequences {
        if let Some(rtx) = rtx
            && let Some(original) = cache.get_rtp(*sequence)
        {
            if let Ok(wire) = crypto.protect_rtp(build_rtx_packet(&original, rtx, *rtx_sequence)) {
                pending.push(wire);
                *rtx_sequence = rtx_sequence.wrapping_add(1);
            }
            continue;
        }
        if let Some(wire) = cache.get(*sequence) {
            pending.push(wire);
        }
    }
    let mut sent = crate::runtime::udp_batch::send_batch(socket, &pending, target)
        .await
        .unwrap_or(0)
        .min(pending.len());
    let mut octets = pending[..sent]
        .iter()
        .map(|packet| packet.len() as u32)
        .sum::<u32>();
    for packet in pending.iter().skip(sent) {
        if send_udp(socket, packet, target).await.is_ok() {
            sent += 1;
            octets = octets.wrapping_add(packet.len() as u32);
        }
    }
    (sent as u32, octets)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_udp_sender_from(
    mount: Arc<Mount>,
    input: SenderInput,
    pair: UdpPair,
    rtp_target: SocketAddr,
    rtcp_target: SocketAddr,
    track: TrackDescription,
    anchor_ntp: Option<u64>,
    report_interval: std::time::Duration,
    rtx: Option<crate::media::RtxConfig>,
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
        let mut nack_cache = RetransmitCache::new(512);
        let mut rtx_sequence = 0u16;
        // Cache the protected bytes too: retransmitting an SRTP packet with
        // its original index is safe (the original was lost) and avoids
        // reusing the sender's keystream state.
        let retransmit = true;
        let mut rtcp_buffer = BytesMut::zeroed(2048);
        // Last sender report's NTP timestamp, for periodic receiver reports
        // (LSR reference).
        let last_sr_ntp: Option<u64> = None;
        let mut report_tick = tokio::time::interval(report_interval);
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
                            && send_udp(&pair.rtcp, &report, rtcp_target).await.is_ok()
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
                        && send_udp(&pair.rtcp, &wire, rtcp_target).await.is_ok()
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
                                let Some(requested) = packet.nack_sequences() else {
                                    continue;
                                };
                                let (packets, octets) = retransmit_cached(
                                    &nack_cache,
                                    &pair.rtp,
                                    rtp_target,
                                    &requested,
                                    rtx,
                                    &mut rtx_sequence,
                                    &mut crypto,
                                )
                                .await;
                                sent_packets = sent_packets.wrapping_add(packets);
                                sent_octets = sent_octets.wrapping_add(octets);
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
                        Err(crate::mount::FrameRecvError::Lagged(count)) => {
                            mount.stats.add_dropped(count as u64);
                            awaiting_key = track.kind == MediaKind::Video;
                            continue;
                        }
                        Err(crate::mount::FrameRecvError::Closed) => break,
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
                        let mut batch = Vec::with_capacity(packets.len());
                        let mut meta = Vec::with_capacity(packets.len());
                        for mut packet in packets {
                            packet.header.sequence = sequence.fetch_add(1, Ordering::Relaxed);
                            packet.encode_into(&mut wire);
                            let plain = wire.split().freeze();
                            let plain_for_cache = retransmit.then(|| plain.clone());
                            let protected = match crypto.protect_rtp(plain) {
                                Ok(wire) => wire,
                                Err(error) => {
                                    mount.stats.add_dropped(1);
                                    tracing::warn!(track = track.id, %error, "SRTP protect failed");
                                    continue;
                                }
                            };
                            let sequence_number = packet.header.sequence;
                            let payload_len = packet.payload.len() as u32;
                            let timestamp = packet.header.timestamp;
                            let cached = retransmit.then(|| protected.clone());
                            batch.push(protected);
                            meta.push((sequence_number, payload_len, timestamp, cached, plain_for_cache));
                        }
                        let batched = crate::runtime::udp_batch::send_batch(
                            &pair.rtp,
                            &batch,
                            rtp_target,
                        )
                        .await
                        .unwrap_or(0)
                        .min(batch.len());
                        let mut first_report_sent = false;
                        for (index, (sequence_number, payload_len, timestamp, cached, plain)) in
                            meta.into_iter().enumerate()
                        {
                            if index >= batched
                                && send_udp(&pair.rtp, &batch[index], rtp_target)
                                    .await
                                    .is_err()
                            {
                                consecutive_send_failures =
                                    consecutive_send_failures.saturating_add(1);
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
                                nack_cache.insert(sequence_number, cached, plain);
                            }
                            sent_packets = sent_packets.wrapping_add(1);
                            sent_octets = sent_octets.wrapping_add(payload_len);
                            last_rtp_timestamp = timestamp;
                            if ntp_clock.is_none() {
                                ntp_clock = RtpNtpClock::new_with_anchor(
                                    last_rtp_timestamp,
                                    track.clock_rate,
                                    anchor_ntp,
                                );
                            }
                            frame_packets = frame_packets.saturating_add(1);
                            frame_octets = frame_octets.saturating_add(u64::from(payload_len));
                            if !first_report_sent
                                && sent_packets > 0
                                && let Some(clock) = ntp_clock.as_mut()
                                && let Ok(report) = encode_sender_report(
                                    ssrc,
                                    clock.ntp_for(last_rtp_timestamp),
                                    last_rtp_timestamp,
                                    sent_packets,
                                    sent_octets,
                                )
                                && let Ok(report) = crypto.protect_rtcp(report)
                                && send_udp(&pair.rtcp, &report, rtcp_target).await.is_ok()
                            {
                                mount.stats.add_sender_reports(1);
                                first_report_sent = true;
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

#[cfg(test)]
mod tests {
    use super::{MediaCrypto, RetransmitCache, retransmit_cached};
    use bytes::Bytes;

    #[test]
    fn rtp_cache_retains_recent_packets_and_evicts_oldest() {
        let mut cache = RetransmitCache::new(2);
        cache.insert(1, Bytes::from_static(b"one"), None);
        cache.insert(2, Bytes::from_static(b"two"), None);
        assert_eq!(cache.get(1).as_deref(), Some(&b"one"[..]));
        cache.insert(3, Bytes::from_static(b"three"), None);
        assert!(cache.get(1).is_none());
        assert_eq!(cache.get(2).as_deref(), Some(&b"two"[..]));
        assert_eq!(cache.get(3).as_deref(), Some(&b"three"[..]));
    }

    #[test]
    fn rtx_packet_carries_original_sequence_in_osn() {
        let original = crate::rtp::RtpPacket {
            header: crate::rtp::RtpHeader {
                marker: true,
                payload_type: 96,
                sequence: 100,
                timestamp: 9000,
                ssrc: 7,
            },
            payload: bytes::Bytes::from_static(b"payload"),
        };
        let rtx = crate::media::RtxConfig {
            payload_type: 97,
            ssrc: 99,
            apt: 96,
        };
        let wire = super::build_rtx_packet(&original, rtx, 5);
        let parsed = crate::rtp::RtpPacket::parse(wire).unwrap();
        assert_eq!(parsed.header.payload_type, 97);
        assert_eq!(parsed.header.sequence, 5);
        assert_eq!(parsed.header.timestamp, 9000);
        assert_eq!(parsed.header.ssrc, 99);
        assert_eq!(&parsed.payload[..2], &100u16.to_be_bytes());
        assert_eq!(&parsed.payload[2..], b"payload");
    }

    #[tokio::test]
    async fn retransmit_cache_sends_requested_packets() {
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = receiver.local_addr().unwrap();
        let mut cache = RetransmitCache::new(8);
        cache.insert(7, Bytes::from_static(b"packet"), None);
        let (packets, octets) = retransmit_cached(
            &cache,
            &sender,
            target,
            &[7, 8],
            None,
            &mut 0,
            &mut MediaCrypto::default(),
        )
        .await;
        assert_eq!((packets, octets), (1, 6));
        let mut buffer = [0u8; 32];
        let (size, _) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            receiver.recv_from(&mut buffer),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&buffer[..size], b"packet");
    }

    #[test]
    fn rtp_cache_evicts_by_byte_budget() {
        let mut cache = RetransmitCache::new(16);
        let megabyte = Bytes::from(vec![0u8; 1024 * 1024]);
        cache.insert(1, megabyte.clone(), None);
        cache.insert(2, megabyte.clone(), None);
        cache.insert(3, megabyte, None);
        assert!(cache.get(1).is_none());
        assert!(cache.get(2).is_some());
        assert!(cache.get(3).is_some());
    }

    #[test]
    fn rtp_cache_normalizes_zero_capacity_without_panicking() {
        let mut cache = RetransmitCache::new(0);
        cache.insert(7, Bytes::from_static(b"packet"), None);
        assert!(cache.get(7).is_some());
    }
}
