use super::{Mount, MountConfig};
use crate::{
    media::{MediaKind, TrackDescription},
    rtcp::Packet as RtcpPacket,
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

pub(super) struct SenderInput {
    pub receiver: broadcast::Receiver<crate::media::AccessUnit>,
    pub startup: Vec<crate::media::AccessUnit>,
    pub sequence: Arc<AtomicU16>,
}

pub(super) fn spawn_interleaved_sender(
    mount: Arc<Mount>,
    rtp_channel: u8,
    rtcp_channel: u8,
    track: TrackDescription,
    writer: Arc<Mutex<ControlWriteHalf>>,
    sequence: Arc<AtomicU16>,
) -> JoinHandle<()> {
    let (receiver, startup) = mount.subscribe_with_startup(track.id);
    spawn_interleaved_sender_from(
        mount,
        SenderInput {
            receiver,
            startup,
            sequence,
        },
        rtp_channel,
        rtcp_channel,
        track,
        writer,
    )
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
        let mut startup = VecDeque::from(startup);
        let ssrc = 0x5254_5350u32.wrapping_add(track.id as u32);
        let mut packetizer = match Packetizer::new(track.codec, track.payload_type, ssrc, 1200) {
            Ok(value) => value,
            Err(_) => return,
        };
        let mut awaiting_key = track.kind == MediaKind::Video;
        let mut sent_packets = 0u32;
        let mut sent_octets = 0u32;
        loop {
            let frame = if let Some(frame) = startup.pop_front() {
                frame
            } else {
                match receiver.recv().await {
                    Ok(frame) => frame,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        mount.stats.add_dropped(count);
                        awaiting_key = track.kind == MediaKind::Video;
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
                    if writer.lock().await.write_all(&wire).await.is_err() {
                        return;
                    }
                    sent_packets = sent_packets.wrapping_add(1);
                    sent_octets = sent_octets.wrapping_add(payload_len);
                    mount.stats.add_rtp(1, u64::from(payload_len));
                    if sent_packets == 1 || sent_packets.is_multiple_of(30) {
                        let report = RtcpPacket::SenderReport {
                            ssrc,
                            ntp: ntp_now(),
                            rtp_timestamp: packet.header.timestamp,
                            packets: sent_packets,
                            octets: sent_octets,
                        }
                        .encode();
                        let wire = InterleavedFrame {
                            channel: rtcp_channel,
                            payload: report,
                        }
                        .try_encode();
                        let Ok(wire) = wire else {
                            return;
                        };
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

pub(super) fn spawn_udp_sender(
    mount: Arc<Mount>,
    pair: UdpPair,
    rtp_target: SocketAddr,
    rtcp_target: SocketAddr,
    track: TrackDescription,
    sequence: Arc<AtomicU16>,
) -> JoinHandle<()> {
    let (receiver, startup) = mount.subscribe_with_startup(track.id);
    spawn_udp_sender_from(
        mount,
        SenderInput {
            receiver,
            startup,
            sequence,
        },
        pair,
        rtp_target,
        rtcp_target,
        track,
    )
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
        let mut startup = VecDeque::from(startup);
        let ssrc = 0x5254_5350u32.wrapping_add(track.id as u32);
        let mut packetizer = match Packetizer::new(track.codec, track.payload_type, ssrc, 1200) {
            Ok(value) => value,
            Err(_) => return,
        };
        let mut awaiting_key = track.kind == MediaKind::Video;
        let mut sent_packets = 0u32;
        let mut sent_octets = 0u32;
        let mut rtcp_buffer = [0u8; 2048];
        loop {
            tokio::select! {
                report = pair.rtcp.recv_from(&mut rtcp_buffer) => {
                    let Ok((size, _)) = report else { return; };
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
                        Some(frame) => Ok(frame),
                        None => receiver.recv().await,
                    }
                } => {
                    let frame = match received {
                        Ok(frame) => frame,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            mount.stats.add_dropped(count);
                            awaiting_key = track.kind == MediaKind::Video;
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
                            let wire = packet.encode();
                            if pair.rtp.send_to(&wire, rtp_target).await.is_err() {
                                continue;
                            }
                            sent_packets = sent_packets.wrapping_add(1);
                            sent_octets = sent_octets.wrapping_add(packet.payload.len() as u32);
                            mount.stats.add_rtp(1, packet.payload.len() as u64);
                            if sent_packets == 1 || sent_packets.is_multiple_of(30) {
                                let report = RtcpPacket::SenderReport {
                                    ssrc,
                                    ntp: ntp_now(),
                                    rtp_timestamp: packet.header.timestamp,
                                    packets: sent_packets,
                                    octets: sent_octets,
                                }
                                .encode();
                                if pair.rtcp.send_to(&report, rtcp_target).await.is_ok() {
                                    mount.stats.add_sender_reports(1);
                                }
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
        let mut depacketizer = Depacketizer::new(track.codec, track.id);
        let mut buffer = vec![0u8; 65_536];
        while let Ok((size, _)) = pair.rtp.recv_from(&mut buffer).await {
            if let Ok(Some(frame)) = RtpPacket::parse(Bytes::copy_from_slice(&buffer[..size]))
                .and_then(|packet| depacketizer.push(packet))
            {
                if mount.frames.send(frame).is_ok() {
                    mount.stats.add_accepted(1);
                } else {
                    mount.stats.add_dropped(1);
                }
            }
        }
    })
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
