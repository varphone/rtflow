use super::*;
use rtflow_media::TrackDescription;
use rtflow_rtsp::transport::InterleavedFrame;

/// Handle one interleaved RTP/RTCP frame (publishing direction).
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_incoming_interleaved(
    frame: InterleavedFrame,
    phase: ConnectionPhase,
    interleaved: &[(Arc<Mount>, u8, u8, TrackDescription)],
    depacketizers: &mut HashMap<u8, Depacketizer>,
    ssrcs: &mut HashMap<u8, u32>,
    crypto: &mut HashMap<u8, MediaCrypto>,
    rtcp_sessions: &mut HashMap<u8, RtcpSession>,
    report_interval: std::time::Duration,
) -> Result<()> {
    // `rtcp-mux` puts RTCP on the RTP channel: classify by payload type
    // (RTCP PT 192..=223) before treating the frame as RTP.
    let is_rtcp = frame
        .payload
        .get(1)
        .is_some_and(|byte| (192..=223).contains(byte));
    if is_rtcp || frame.channel % 2 == 1 {
        if let Some((mount, rtp_channel, _, track)) =
            interleaved
                .iter()
                .find(|(_, rtp_channel, rtcp_channel, _)| {
                    *rtcp_channel == frame.channel || (is_rtcp && *rtp_channel == frame.channel)
                })
            && let Ok(payload) = match crypto.get_mut(rtp_channel) {
                Some(crypto) => crypto.unprotect_rtcp(frame.payload),
                None => Ok(frame.payload),
            }
            && let Ok(packets) = RtcpPacket::parse(payload)
        {
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
            let sender_reports = packets
                .iter()
                .filter(|packet| {
                    matches!(
                        packet,
                        RtcpPacket::SenderReport { .. }
                            | RtcpPacket::SenderReportWithReports { .. }
                    )
                })
                .count() as u64;
            if sender_reports > 0 {
                mount.stats.add_publisher_sender_reports(sender_reports);
            }
            if let Some(session) = rtcp_sessions.get_mut(rtp_channel) {
                let now = tokio::time::Instant::now();
                for packet in &packets {
                    session.on_rtcp(packet, now);
                }
            }
            let nack_sequences = packets
                .iter()
                .filter_map(RtcpPacket::nack_sequences)
                .flatten()
                .collect::<Vec<_>>();
            if !nack_sequences.is_empty() {
                let _ = mount.nack_requests.send(NackRequest {
                    track_id: track.id,
                    sequences: nack_sequences,
                });
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
        return Ok(());
    }
    if phase == ConnectionPhase::Recording
        && let Some((mount, _, _, track)) = interleaved
            .iter()
            .find(|(_, rtp_channel, _, _)| *rtp_channel == frame.channel)
            .cloned()
    {
        let depacketizer = depacketizers
            .entry(frame.channel)
            .or_insert_with(|| Depacketizer::new(track.codec, track.id, track.clock_rate));
        let payload = match crypto.get_mut(&frame.channel) {
            Some(crypto) => match crypto.unprotect_rtp(frame.payload) {
                Ok(payload) => payload,
                Err(_) => {
                    tracing::warn!(channel = frame.channel, "invalid interleaved SRTP");
                    return Ok(());
                }
            },
            None => frame.payload,
        };
        let packet = match RtpPacket::parse(payload) {
            Ok(packet) => packet,
            Err(error) => {
                tracing::warn!(channel=frame.channel, error=%error, "invalid interleaved RTP");
                return Ok(());
            }
        };
        if tracing::enabled!(tracing::Level::DEBUG) && frame.channel == 0 {
            tracing::debug!(
                seq = packet.header.sequence,
                ts = packet.header.timestamp,
                marker = packet.header.marker,
                "rtp in"
            );
        }
        if packet.header.payload_type != track.payload_type
            && !(packet.header.payload_type >= 96 && track.payload_type >= 96)
            && !matches!(
                track.codec,
                rtflow_media::Codec::H263 | rtflow_media::Codec::Jpeg
            )
        {
            tracing::debug!(
                channel = frame.channel,
                payload_type = packet.header.payload_type,
                expected = track.payload_type,
                "dropping RTP payload type mismatch"
            );
            return Ok(());
        }
        if let Some(expected) = ssrcs.get(&frame.channel) {
            if *expected != packet.header.ssrc {
                tracing::debug!(
                    channel = frame.channel,
                    ssrc = packet.header.ssrc,
                    expected_ssrc = *expected,
                    "dropping RTP SSRC mismatch"
                );
                return Ok(());
            }
        } else {
            ssrcs.insert(frame.channel, packet.header.ssrc);
        }
        let now = tokio::time::Instant::now();
        if let Some(session) = rtcp_sessions.get_mut(&frame.channel) {
            if session.on_rtp(&packet, now).is_err() {
                return Ok(());
            }
        } else if let Ok(mut session) = RtcpSession::new(
            0x5254_0000 | u32::from(track.id),
            packet.header.ssrc,
            track.clock_rate,
            format!("rtflow-{}-{}", mount.id, track.id),
            report_interval,
            now,
        ) && session.on_rtp(&packet, now).is_ok()
        {
            rtcp_sessions.insert(frame.channel, session);
        }
        match depacketizer.push(packet) {
            Ok(units) => {
                for unit in units {
                    if mount.publish_cached(unit).is_ok() {
                        mount.stats.add_accepted(1);
                    } else {
                        mount.stats.add_dropped(1);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(channel=frame.channel, error=%e, "depacketizer error");
            }
        }
    }
    Ok(())
}

/// Send one RR + SDES compound report on every publisher track's RTCP
/// interleaved channel. Called from the session loop's report tick.
pub(super) async fn send_interleaved_receiver_reports(
    interleaved: &[(Arc<Mount>, u8, u8, TrackDescription)],
    sessions: &mut HashMap<u8, RtcpSession>,
    crypto: &mut HashMap<u8, MediaCrypto>,
    writer: &ConnWriter,
) -> Result<()> {
    let now = tokio::time::Instant::now();
    for (mount, rtp_channel, rtcp_channel, track) in interleaved {
        let Some(session) = sessions.get_mut(rtp_channel) else {
            continue;
        };
        if session.snapshot().packets_received == 0 {
            continue;
        }
        let Ok(Some(compound)) = session.poll_report(now) else {
            continue;
        };
        let Ok(wire) = compound.try_encode() else {
            continue;
        };
        let wire = match crypto.get_mut(rtp_channel) {
            Some(crypto) => match crypto.protect_rtcp(wire) {
                Ok(wire) => wire,
                Err(_) => continue,
            },
            None => wire,
        };
        let _ = track;
        let Ok(frame) = InterleavedFrame {
            channel: *rtcp_channel,
            payload: wire,
        }
        .try_encode() else {
            continue;
        };
        let mut guard = writer.lock().await;
        guard.write_all(&frame).await?;
        drop(guard);
        mount.stats.add_receiver_reports(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtflow_media::{MediaDescription, TrackDescription};
    use rtflow_mount::{MountConfig, MountRegistry};
    use rtflow_rtcp::{CompoundPacket, NackEntry, Packet as RtcpPacket};
    use std::collections::HashMap;

    #[tokio::test]
    async fn nack_is_forwarded_to_interleaved_sender_with_expanded_bitmap() {
        let description = MediaDescription::new(
            "nack",
            vec![TrackDescription::h264_video(0, &[], &[], 640, 360, 30)],
        );
        let registry = MountRegistry::new(1);
        let handle = registry
            .register(
                Arc::from("/nack"),
                MountConfig::Local {
                    description: description.clone(),
                },
            )
            .unwrap();
        let mount = handle.mount().clone();
        let track = description.tracks[0].clone();
        let mut requests = mount.nack_requests.subscribe();

        let nack = RtcpPacket::nack(
            1,
            2,
            &[NackEntry {
                packet_id: 100,
                lost: 0b101,
            }],
        )
        .unwrap();
        let payload = CompoundPacket::new(vec![RtcpPacket::ReceiverReport { ssrc: 1 }, nack])
            .unwrap()
            .try_encode()
            .unwrap();
        let frame = InterleavedFrame {
            channel: 1,
            payload,
        };
        let interleaved = vec![(mount.clone(), 0u8, 1u8, track)];
        let mut depacketizers = HashMap::new();
        let mut ssrcs = HashMap::new();
        let mut crypto = HashMap::new();
        let mut sessions = HashMap::new();
        handle_incoming_interleaved(
            frame,
            ConnectionPhase::Playing,
            &interleaved,
            &mut depacketizers,
            &mut ssrcs,
            &mut crypto,
            &mut sessions,
            std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();

        let request = requests.try_recv().expect("NACK request forwarded");
        assert_eq!(request.track_id, 0);
        assert_eq!(request.sequences, vec![100, 101, 103]);
    }
}
