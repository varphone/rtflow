use crate::mount::{Mount, MountConfig, startup_dedup_keys};
use crate::{
    media::{MediaKind, TrackDescription},
    rtcp::{CompoundPacket, Packet as RtcpPacket, SdesChunk, SdesItem},
    rtp::{Depacketizer, Packetizer, RtpPacket},
    runtime::io::ControlWriteHalf,
    transport::{InterleavedFrame, UdpPair},
};
use bytes::Bytes;
use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, broadcast},
    task::JoinHandle,
};

/// Upper bound for a single interleaved (RTSP/TCP) write. A stalled peer
/// must not hold the per-session write lock indefinitely — that would
/// freeze every track's sender and the RTCP path.
const INTERLEAVED_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Write to the shared interleaved writer with a hard timeout.
/// Returns `false` on timeout/error (caller should tear down the session).
async fn write_interleaved(
    writer: &Arc<Mutex<ControlWriteHalf>>,
    wire: &[u8],
) -> bool {
    match tokio::time::timeout(INTERLEAVED_WRITE_TIMEOUT, async {
        let mut guard = writer.lock().await;
        guard.write_all(wire).await
    })
    .await
    {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
        Err(_) => false,
    }
}

#[derive(Clone)]
pub(super) struct RtpNtpClock {
    anchor_ntp: u64,
    anchor_rtp_timestamp: u32,
    clock_rate: u32,
    /// Last observed RTP timestamp, for wrap-extended deltas.
    last_rtp: u32,
    /// Cumulative signed RTP-clock delta since the anchor (wrap-free).
    extended_delta: i64,
}

impl RtpNtpClock {
    fn new(anchor_rtp_timestamp: u32, clock_rate: u32) -> Option<Self> {
        (clock_rate != 0).then(|| Self {
            anchor_ntp: ntp_now(),
            anchor_rtp_timestamp,
            clock_rate,
            last_rtp: anchor_rtp_timestamp,
            extended_delta: 0,
        })
    }

    /// NTP wall clock for an RTP timestamp. The per-packet delta is
    /// interpreted as a signed 32-bit difference (so short backward jumps
    /// and wrap-arounds are right) and accumulated into a wrap-free 64-bit
    /// clock — a plain signed delta would make the sender-report NTP jump
    /// backwards after ~6.6h of continuous playback.
    fn ntp_for(&mut self, rtp_timestamp: u32) -> u64 {
        let raw_delta = rtp_timestamp.wrapping_sub(self.last_rtp) as i32 as i64;
        self.extended_delta = self.extended_delta.wrapping_add(raw_delta);
        self.last_rtp = rtp_timestamp;
        let delta_ntp = i128::from(self.extended_delta) * (1i128 << 32)
            / i128::from(self.clock_rate);
        self.anchor_ntp.wrapping_add_signed(delta_ntp as i64)
    }
}

/// Per-process random SSRC seed: RFC 3550 wants a random initial SSRC so
/// two servers on the same network do not collide (and a receiver must not
/// merge sessions from different sources).
fn initial_ssrc(track_id: u8) -> u32 {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u32;
    seed ^ (track_id as u32).rotate_left(24) ^ 0x5254_5350
}

/// SDES CNAME: RFC 3550 wants a globally unique per-session identifier;
/// `user@host` is the canonical form (statistics tools aggregate by it).
fn sdes_cname() -> Bytes {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into());
    Bytes::from(format!("rtflow@{host}"))
}

/// Sends an RTCP BYE when the sender task ends (RAII; Drop runs on the
/// tokio runtime the task lives in).
struct ByeGuard {
    pair: UdpPair,
    target: SocketAddr,
    ssrc: u32,
}
impl Drop for ByeGuard {
    fn drop(&mut self) {
        if let Ok(wire) = CompoundPacket::new(vec![RtcpPacket::Bye { ssrc: self.ssrc }])
            .and_then(|compound| compound.try_encode())
        {
            let pair = self.pair.clone();
            let target = self.target;
            tokio::spawn(async move {
                let _ = pair.rtcp.send_to(&wire, target).await;
            });
        }
    }
}

/// Interleaved variant of [`ByeGuard`]: writes BYE over the shared RTSP
/// control connection instead of a UDP socket.
struct InterleavedByeGuard {
    writer: Arc<Mutex<ControlWriteHalf>>,
    channel: u8,
    ssrc: u32,
}
impl Drop for InterleavedByeGuard {
    fn drop(&mut self) {
        let Ok(wire) = CompoundPacket::new(vec![RtcpPacket::Bye { ssrc: self.ssrc }])
            .and_then(|compound| compound.try_encode())
        else {
            return;
        };
        let Ok(frame) = InterleavedFrame {
            channel: self.channel,
            payload: wire,
        }
        .try_encode()
        else {
            return;
        };
        let writer = self.writer.clone();
        tokio::spawn(async move {
            let mut guard = writer.lock().await;
            let _ = guard.write_all(&frame).await;
        });
    }
}

pub(super) struct SenderInput {
    pub receiver: broadcast::Receiver<crate::media::AccessUnit>,
    pub startup: Vec<crate::media::AccessUnit>,
    pub sequence: Arc<AtomicU16>,
}

pub(super) fn spawn_interleaved_sender_from(
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
            Ok(value) => value,
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
                    let wire = InterleavedFrame {
                        channel: rtp_channel,
                        payload: packet.encode(),
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
                        && let Ok(wire) = (InterleavedFrame {
                            channel: rtcp_channel,
                            payload: report,
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

pub(super) fn spawn_udp_sender_from(
    mount: Arc<Mount>,
    input: SenderInput,
    pair: UdpPair,
    rtp_target: SocketAddr,
    rtcp_target: SocketAddr,
    track: TrackDescription,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let SenderInput {
            mut receiver,
            startup,
            sequence,
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
        let mut packetizer = match Packetizer::new(track.codec, track.payload_type, ssrc, 1200) {
            Ok(value) => value,
            Err(_) => return,
        };
        let mut awaiting_key = track.kind == MediaKind::Video;
        let mut sent_packets = 0u32;
        let mut sent_octets = 0u32;
        let mut last_rtp_timestamp = 0u32;
        let mut ntp_clock: Option<RtpNtpClock> = None;
        let mut rtcp_buffer = [0u8; 2048];
        // Last sender report's NTP timestamp, for periodic receiver reports
        // (LSR reference).
        let mut last_sr_ntp: Option<u64> = None;
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
                    if let Ok(packets) = RtcpPacket::parse(Bytes::copy_from_slice(&rtcp_buffer[..size])) {
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
                    if let Ok(packets) = packetizer.packetize(&frame) {
                        for mut packet in packets {
                            packet.header.sequence = sequence.fetch_add(1, Ordering::Relaxed);
                            let wire = packet.encode();
                            if pair.rtp.send_to(&wire, rtp_target).await.is_err() {
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
                            sent_packets = sent_packets.wrapping_add(1);
                            sent_octets = sent_octets.wrapping_add(packet.payload.len() as u32);
                            last_rtp_timestamp = packet.header.timestamp;
                            if ntp_clock.is_none() {
                                ntp_clock = RtpNtpClock::new(last_rtp_timestamp, track.clock_rate);
                            }
                            mount.stats.add_rtp(1, packet.payload.len() as u64);
                            if sent_packets == 1
                                && let Some(clock) = ntp_clock.as_mut()
                                && let Ok(report) = encode_sender_report(
                                    ssrc,
                                    clock.ntp_for(last_rtp_timestamp),
                                    last_rtp_timestamp,
                                    sent_packets,
                                    sent_octets,
                                )
                                && pair.rtcp.send_to(&report, rtcp_target).await.is_ok()
                            {
                                mount.stats.add_sender_reports(1);
                            }
                        }
                    }
                }
            }
        }
    })
}

struct PublisherLease(Arc<Mount>);
impl Drop for PublisherLease {
    fn drop(&mut self) {
        self.0
            .publishers
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

pub(super) fn spawn_udp_receiver(
    mount: Arc<Mount>,
    pair: UdpPair,
    expected_rtp_source: SocketAddr,
    track: TrackDescription,
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
        let mut buffer = vec![0u8; 65_536];
        let mut expected_ssrc = None;
        while let Ok((size, source)) = pair.rtp.recv_from(&mut buffer).await {
            if source != expected_rtp_source {
                mount.stats.add_dropped(1);
                continue;
            }
            let packet = match RtpPacket::parse(Bytes::copy_from_slice(&buffer[..size])) {
                Ok(packet) => packet,
                Err(_) => {
                    mount.stats.add_dropped(1);
                    continue;
                }
            };
            if packet.header.payload_type != track.payload_type
                && !(packet.header.payload_type >= 96 && track.payload_type >= 96)
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

fn encode_sender_report(
    ssrc: u32,
    ntp: u64,
    rtp_timestamp: u32,
    packets: u32,
    octets: u32,
) -> crate::Result<Bytes> {
    CompoundPacket::new(vec![
        RtcpPacket::SenderReport {
            ssrc,
            ntp,
            rtp_timestamp,
            packets,
            octets,
        },
        RtcpPacket::SourceDescription {
            chunks: vec![SdesChunk {
                ssrc,
                items: vec![SdesItem {
                    kind: 1,
                    value: sdes_cname(),
                }],
            }],
        },
    ])
    .and_then(|compound| compound.try_encode())
}

const NTP_UNIX_EPOCH_OFFSET_SECS: u64 = 2_208_988_800;

fn ntp_now() -> u64 {
    system_time_to_ntp(std::time::SystemTime::now()).unwrap_or(0)
}

fn system_time_to_ntp(time: std::time::SystemTime) -> Option<u64> {
    let value = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    let seconds = value.as_secs().checked_add(NTP_UNIX_EPOCH_OFFSET_SECS)?;
    let fraction = (value.subsec_nanos() as u64 * (1u64 << 32)) / 1_000_000_000;
    Some((seconds << 32) | fraction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sender_reports_are_compound_and_include_cname() {
        let wire = encode_sender_report(7, 8, 9, 10, 11).unwrap();
        let packets = RtcpPacket::parse(wire).unwrap();
        assert!(matches!(
            packets.first(),
            Some(RtcpPacket::SenderReport { .. })
        ));
        assert!(matches!(
            packets.get(1),
            Some(RtcpPacket::SourceDescription { .. })
        ));
    }

    #[test]
    fn rtp_ntp_clock_advances_at_the_media_clock_rate() {
        let mut clock = RtpNtpClock {
            anchor_ntp: NTP_UNIX_EPOCH_OFFSET_SECS << 32,
            anchor_rtp_timestamp: 90_000,
            clock_rate: 90_000,
            last_rtp: 90_000,
            extended_delta: 0,
        };
        assert_eq!(
            clock.ntp_for(180_000),
            (NTP_UNIX_EPOCH_OFFSET_SECS + 1) << 32
        );
        assert_eq!(
            clock.ntp_for(45_000),
            (NTP_UNIX_EPOCH_OFFSET_SECS << 32) - (1 << 31)
        );
    }

    #[test]
    fn unix_epoch_maps_to_ntp_epoch_offset() {
        assert_eq!(
            system_time_to_ntp(std::time::UNIX_EPOCH),
            Some(NTP_UNIX_EPOCH_OFFSET_SECS << 32)
        );
        assert_eq!(
            system_time_to_ntp(std::time::UNIX_EPOCH + Duration::from_millis(500)),
            Some((NTP_UNIX_EPOCH_OFFSET_SECS << 32) | (1 << 31))
        );
    }
}
