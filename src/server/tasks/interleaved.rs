use super::*;

pub(crate) fn spawn_interleaved_sender_from(
    mount: Arc<Mount>,
    input: SenderInput,
    rtp_channel: u8,
    rtcp_channel: u8,
    track: TrackDescription,
    writer: Arc<Mutex<ControlWriteHalf>>,
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
        let _bye = InterleavedByeGuard {
            writer: writer.clone(),
            channel: rtcp_channel,
            ssrc,
        };
        let mut packetizer = match Packetizer::new(track.codec, track.payload_type, ssrc, 1200) {
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
        let mut report_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        report_tick.tick().await;
        let mount_cancel = mount.cancel_token();
        loop {
            let frame = if let Some(frame) = startup.pop_front() {
                if let Some(clock) = startup_clock {
                    let due = clock + frame.pts.saturating_sub(startup_base_pts);
                    tokio::time::sleep_until(due).await;
                } else {
                    startup_clock = Some(tokio::time::Instant::now());
                    startup_base_pts = frame.pts;
                }
                frame
            } else {
                tokio::select! {
                    biased;
                    _ = mount_cancel.cancelled() => {
                        // Mount unregistered or server shutting down.
                        return;
                    }
                    _ = report_tick.tick() => {
                        if sent_packets > 0
                            && let Some(clock) = ntp_clock.as_mut()
                            && let Ok(report) = encode_sender_report(
                                ssrc,
                                clock.ntp_for(last_rtp_timestamp),
                                last_rtp_timestamp,
                                sent_packets,
                                sent_octets,
                            )
                            && let Ok(wire) = (InterleavedFrame {
                                channel: rtcp_channel,
                                payload: report,
                            })
                            .try_encode()
                        {
                            if !write_interleaved(&writer, &wire).await {
                                return;
                            }
                            mount.stats.add_sender_reports(1);
                        }
                        continue;
                    }
                    result = receiver.recv() => match result {
                        Ok(frame) => {
                            if startup_keys.contains(&(frame.track_id, frame.rtp_timestamp)) {
                                continue;
                            }
                            frame
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            mount.stats.add_dropped(count);
                            awaiting_key = track.kind == MediaKind::Video;
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            };
            if frame.track_id != track.id || (awaiting_key && !frame.is_keyframe) {
                continue;
            }
            if frame.data.len() > mount.max_queued_bytes_per_session {
                mount.stats.add_dropped(1);
                awaiting_key = track.kind == MediaKind::Video;
                continue;
            }
            awaiting_key = false;
            if let Ok(packets) = packetizer.packetize(&frame) {
                for mut packet in packets {
                    packet.header.sequence = sequence.fetch_add(1, Ordering::Relaxed);
                    let payload_len = packet.payload.len() as u32;
                    let payload = match crypto.protect_rtp(packet.encode()) {
                        Ok(payload) => payload,
                        Err(_) => continue,
                    };
                    let wire = InterleavedFrame {
                        channel: rtp_channel,
                        payload,
                    }
                    .try_encode();
                    let Ok(wire) = wire else {
                        return;
                    };
                    if !write_interleaved(&writer, &wire).await {
                        return;
                    }
                    sent_packets = sent_packets.wrapping_add(1);
                    sent_octets = sent_octets.wrapping_add(payload_len);
                    last_rtp_timestamp = packet.header.timestamp;
                    if ntp_clock.is_none() {
                        ntp_clock = RtpNtpClock::new(last_rtp_timestamp, track.clock_rate);
                    }
                    mount.stats.add_rtp(1, u64::from(payload_len));
                    if sent_packets == 1
                        && let Some(clock) = ntp_clock.as_mut()
                        && let Ok(report) = encode_sender_report(
                            ssrc,
                            clock.ntp_for(last_rtp_timestamp),
                            last_rtp_timestamp,
                            sent_packets,
                            sent_octets,
                        )
                        && let Ok(payload) = crypto.protect_rtcp(report)
                        && let Ok(wire) = (InterleavedFrame {
                            channel: rtcp_channel,
                            payload,
                        })
                        .try_encode()
                    {
                        if writer.lock().await.write_all(&wire).await.is_err() {
                            return;
                        }
                        mount.stats.add_sender_reports(1);
                    }
                }
            }
        }
    })
}
