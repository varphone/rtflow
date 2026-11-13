use super::*;
use crate::server::timed_writer::TimedWriter;

struct NackCache {
    max: usize,
    entries: std::collections::VecDeque<(u16, Bytes)>,
}

impl NackCache {
    fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            entries: std::collections::VecDeque::new(),
        }
    }

    fn insert(&mut self, sequence: u16, payload: Bytes) {
        if self.entries.len() >= self.max {
            self.entries.pop_front();
        }
        self.entries.push_back((sequence, payload));
    }

    fn get(&self, sequence: u16) -> Option<Bytes> {
        self.entries
            .iter()
            .find(|(current, _)| *current == sequence)
            .map(|(_, payload)| payload.clone())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_interleaved_sender_from(
    mount: Arc<Mount>,
    input: SenderInput,
    rtp_channel: u8,
    rtcp_channel: u8,
    track: TrackDescription,
    writer: Arc<Mutex<TimedWriter>>,
    anchor_ntp: Option<u64>,
    report_interval: std::time::Duration,
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
        let mut nack_cache = NackCache::new(512);
        let mut nack_rx = mount.nack_requests.subscribe();
        let mut report_tick = tokio::time::interval(report_interval);
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
                    nack = nack_rx.recv() => match nack {
                        Ok(request) => {
                            if request.track_id != track.id {
                                continue;
                            }
                            let mut batch = BytesMut::with_capacity(1500 * request.sequences.len().max(1));
                            for sequence in request.sequences {
                                let Some(payload) = nack_cache.get(sequence) else {
                                    continue;
                                };
                                let Ok(length) = u16::try_from(payload.len()) else {
                                    continue;
                                };
                                batch.extend_from_slice(&[b'$', rtp_channel]);
                                batch.extend_from_slice(&length.to_be_bytes());
                                batch.extend_from_slice(&payload);
                                mount.stats.add_rtp(1, payload.len() as u64);
                            }
                            if !batch.is_empty() && !write_interleaved(&writer, &batch).await {
                                return;
                            }
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            mount.stats.add_dropped(count);
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => continue,
                    },
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
                            && let Ok(report) = crypto.protect_rtcp(report)
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
            let packets = match packetizer.packetize(&frame) {
                Ok(packets) => packets,
                Err(error) => {
                    mount.stats.add_dropped(1);
                    tracing::warn!(track = track.id, %error, "RTP packetization failed");
                    continue;
                }
            };
            let first_batch = sent_packets == 0;
            let mut batch = BytesMut::with_capacity(1500 * packets.len().max(1));
            let mut batch_packets = 0u64;
            let mut batch_octets = 0u64;
            for mut packet in packets {
                packet.header.sequence = sequence.fetch_add(1, Ordering::Relaxed);
                let payload_len = packet.payload.len() as u32;
                let payload = match crypto.protect_rtp(packet.encode()) {
                    Ok(payload) => payload,
                    Err(error) => {
                        mount.stats.add_dropped(1);
                        tracing::warn!(track = track.id, %error, "SRTP protect failed");
                        continue;
                    }
                };
                let Ok(length) = u16::try_from(payload.len()) else {
                    mount.stats.add_dropped(1);
                    tracing::warn!(track = track.id, "interleaved frame too large");
                    continue;
                };
                // Batch every RTP packet of this access unit into one
                // write: one lock + one syscall instead of one per packet.
                batch.extend_from_slice(&[b'$', rtp_channel]);
                batch.extend_from_slice(&length.to_be_bytes());
                batch.extend_from_slice(&payload);
                nack_cache.insert(packet.header.sequence, payload);
                sent_packets = sent_packets.wrapping_add(1);
                sent_octets = sent_octets.wrapping_add(payload_len);
                last_rtp_timestamp = packet.header.timestamp;
                if ntp_clock.is_none() {
                    ntp_clock = RtpNtpClock::new_with_anchor(
                        last_rtp_timestamp,
                        track.clock_rate,
                        anchor_ntp,
                    );
                }
                batch_packets = batch_packets.saturating_add(1);
                batch_octets = batch_octets.saturating_add(u64::from(payload_len));
            }
            if !batch.is_empty() && !write_interleaved(&writer, &batch).await {
                return;
            }
            mount.stats.add_rtp(batch_packets, batch_octets);
            if first_batch
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
        }
    })
}
