use crate::mount::{Mount, MountConfig, startup_dedup_keys};
use crate::{
    media::{MediaKind, TrackDescription},
    rtcp::{CompoundPacket, Packet as RtcpPacket, SdesChunk, SdesItem},
    rtp::{Depacketizer, Packetizer, RtpPacket},
    server::timed_writer::TimedWriter,
    transport::{InterleavedFrame, UdpPair},
};
use bytes::{Bytes, BytesMut};
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
async fn write_interleaved(writer: &Arc<Mutex<TimedWriter>>, wire: &[u8]) -> bool {
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
    clock_rate: u32,
    /// Last observed RTP timestamp, for wrap-extended deltas.
    last_rtp: u32,
    /// Cumulative signed RTP-clock delta since the anchor (wrap-free).
    extended_delta: i64,
}

impl RtpNtpClock {
    fn new_with_anchor(
        anchor_rtp_timestamp: u32,
        clock_rate: u32,
        anchor_ntp: Option<u64>,
    ) -> Option<Self> {
        (clock_rate != 0).then(|| Self {
            anchor_ntp: anchor_ntp.unwrap_or_else(ntp_now),
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
        let delta_ntp =
            i128::from(self.extended_delta) * (1i128 << 32) / i128::from(self.clock_rate);
        self.anchor_ntp.wrapping_add_signed(delta_ntp as i64)
    }
}

/// Per-process random SSRC seed: RFC 3550 wants a random initial SSRC so
/// two servers on the same network do not collide (and a receiver must not
/// merge sessions from different sources).
fn initial_ssrc(track_id: u8) -> u32 {
    let mut bytes = [0u8; 4];
    let seed = if getrandom::getrandom(&mut bytes).is_ok() {
        u32::from_le_bytes(bytes)
    } else {
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u32
    };
    seed ^ (track_id as u32).rotate_left(24) ^ 0x5254_5350
}

/// SDES CNAME: RFC 3550 wants a globally unique per-session identifier;
/// `user@host` is the canonical form (statistics tools aggregate by it).
fn sdes_cname(ssrc: u32) -> Bytes {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into());
    Bytes::from(format!("rtflow@{host}-{ssrc:08x}"))
}

pub(super) struct SenderInput {
    pub receiver: broadcast::Receiver<crate::media::AccessUnit>,
    pub startup: Vec<crate::media::AccessUnit>,
    pub sequence: Arc<AtomicU16>,
    pub crypto: MediaCrypto,
}

/// SRTP/SRTCP contexts for one track; empty when the `srtp` feature is off
/// or the track has no `a=crypto` attribute.
#[derive(Default)]
pub(super) struct MediaCrypto {
    #[cfg(feature = "srtp")]
    srtp: Option<crate::srtp::SrtpContext>,
    #[cfg(feature = "srtp")]
    srtcp: Option<crate::srtp::SrtcpContext>,
}

impl MediaCrypto {
    pub(super) fn from_track(track: &crate::media::TrackDescription) -> crate::Result<Self> {
        #[cfg(feature = "srtp")]
        if let Some(crypto) = &track.crypto {
            let (srtp, srtcp) = crate::srtp::from_sdp_crypto(crypto)?;
            return Ok(Self {
                srtp: Some(srtp),
                srtcp: Some(srtcp),
            });
        }
        let _ = track;
        Ok(Self::default())
    }

    pub(super) fn protect_rtp(&mut self, wire: Bytes) -> crate::Result<Bytes> {
        #[cfg(feature = "srtp")]
        if let Some(srtp) = self.srtp.as_mut() {
            return srtp.protect_rtp(&wire);
        }
        Ok(wire)
    }

    pub(super) fn unprotect_rtp(&mut self, wire: Bytes) -> crate::Result<Bytes> {
        #[cfg(feature = "srtp")]
        if let Some(srtp) = self.srtp.as_mut() {
            return srtp.unprotect_rtp(&wire);
        }
        Ok(wire)
    }

    pub(super) fn protect_rtcp(&mut self, wire: Bytes) -> crate::Result<Bytes> {
        #[cfg(feature = "srtp")]
        if let Some(srtcp) = self.srtcp.as_mut() {
            return srtcp.protect(&wire);
        }
        Ok(wire)
    }

    pub(super) fn is_secure(&self) -> bool {
        #[cfg(feature = "srtp")]
        {
            self.srtp.is_some()
        }
        #[cfg(not(feature = "srtp"))]
        {
            false
        }
    }

    pub(super) fn unprotect_rtcp(&mut self, wire: Bytes) -> crate::Result<Bytes> {
        #[cfg(feature = "srtp")]
        if let Some(srtcp) = self.srtcp.as_mut() {
            return srtcp.unprotect(&wire);
        }
        Ok(wire)
    }
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
                    value: sdes_cname(ssrc),
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

/// Sends an RTCP BYE when the sender task ends.
struct ByeGuard {
    pair: UdpPair,
    target: SocketAddr,
    ssrc: u32,
}
fn bye_compound(ssrc: u32) -> crate::Result<Bytes> {
    CompoundPacket::new(vec![
        RtcpPacket::ReceiverReport { ssrc },
        RtcpPacket::ByeSources {
            sources: vec![ssrc],
            reason: None,
        },
        RtcpPacket::SourceDescription {
            chunks: vec![SdesChunk {
                ssrc,
                items: vec![SdesItem {
                    kind: 1,
                    value: sdes_cname(ssrc),
                }],
            }],
        },
    ])
    .and_then(|compound| compound.try_encode())
}

impl Drop for ByeGuard {
    fn drop(&mut self) {
        if let Ok(wire) = bye_compound(self.ssrc) {
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
    writer: Arc<Mutex<TimedWriter>>,
    channel: u8,
    ssrc: u32,
}
impl Drop for InterleavedByeGuard {
    fn drop(&mut self) {
        let Ok(wire) = bye_compound(self.ssrc) else {
            return;
        };
        let Ok(frame) = InterleavedFrame {
            channel: self.channel,
            payload: wire,
        }
        .try_encode() else {
            return;
        };
        let writer = self.writer.clone();
        tokio::spawn(async move {
            let mut guard = writer.lock().await;
            let _ = guard.write_all(&frame).await;
        });
    }
}

mod interleaved;
#[cfg(test)]
mod tests;
mod udp;

pub(super) use interleaved::spawn_interleaved_sender_from;
pub(super) use udp::{spawn_udp_receiver, spawn_udp_sender_from};
