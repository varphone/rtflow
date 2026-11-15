use super::{CompoundPacket, Packet, ReportBlock, SdesChunk, SdesItem};
use bytes::Bytes;
use rtflow_core::error::{Error, Result};
use rtflow_rtp::RtpPacket;
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

const RTP_SEQUENCE_MODULUS: u32 = 1 << 16;

/// Counters for RFC 4585/5506 feedback received for one track.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FeedbackSnapshot {
    pub nack_entries: u64,
    pub pli: u64,
    pub fir: u64,
    pub remb: Option<u64>,
    pub app: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReceptionSnapshot {
    pub packets_received: u64,
    pub packets_expected: u64,
    pub packets_lost: i64,
    pub extended_highest_sequence: u32,
    pub interarrival_jitter: u32,
    pub last_rtt: Option<Duration>,
}

pub struct ReceptionStats {
    source_ssrc: u32,
    clock_rate: u32,
    started_at: Instant,
    base_sequence: Option<u16>,
    maximum_sequence: u16,
    sequence_cycles: u32,
    received: u64,
    expected_prior: u64,
    received_prior: u64,
    previous_transit: Option<i64>,
    jitter: f64,
    /// Arrival clock (in clock-rate units) of the first observed packet.
    first_arrival_units: Option<i64>,
    /// RTP timestamp of the first observed packet.
    first_timestamp: Option<u32>,
    last_sender_report: Option<(u32, Instant)>,
    last_rtt: Option<Duration>,
}

impl ReceptionStats {
    pub fn new(source_ssrc: u32, clock_rate: u32, now: Instant) -> Result<Self> {
        if clock_rate == 0 {
            return Err(Error::Media("RTP clock rate is zero".into()));
        }
        Ok(Self {
            source_ssrc,
            clock_rate,
            started_at: now,
            base_sequence: None,
            maximum_sequence: 0,
            sequence_cycles: 0,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            previous_transit: None,
            jitter: 0.0,
            first_arrival_units: None,
            first_timestamp: None,
            last_sender_report: None,
            last_rtt: None,
        })
    }

    pub fn observe_rtp(&mut self, packet: &RtpPacket, received_at: Instant) -> Result<()> {
        if packet.header.ssrc != self.source_ssrc {
            return Err(Error::Media("RTP packet SSRC changed".into()));
        }
        let sequence = packet.header.sequence;
        match self.base_sequence {
            None => {
                self.base_sequence = Some(sequence);
                self.maximum_sequence = sequence;
            }
            Some(_) => {
                if sequence < self.maximum_sequence
                    && self.maximum_sequence.wrapping_sub(sequence) > u16::MAX / 2
                {
                    self.sequence_cycles =
                        self.sequence_cycles.saturating_add(RTP_SEQUENCE_MODULUS);
                    self.maximum_sequence = sequence;
                } else if sequence > self.maximum_sequence
                    && sequence.wrapping_sub(self.maximum_sequence) < u16::MAX / 2
                {
                    self.maximum_sequence = sequence;
                }
            }
        }
        self.received = self.received.saturating_add(1);

        let elapsed = received_at.saturating_duration_since(self.started_at);
        let arrival_units = (elapsed.as_secs_f64() * f64::from(self.clock_rate)) as i64;
        // RFC 3550 transit = arrival(clock units) - RTP timestamp. The two
        // clocks share no absolute zero, so subtract the first packet's
        // (arrival, timestamp) pair: the constant offset cancels and the
        // jitter estimate is unaffected by the reference mismatch (and by
        // a 32-bit timestamp wrap, via wrapping subtraction).
        let transit = match (self.first_arrival_units, self.first_timestamp) {
            (Some(first_arrival), Some(first_ts)) => {
                (arrival_units - first_arrival)
                    - i64::from(packet.header.timestamp.wrapping_sub(first_ts))
            }
            _ => {
                self.first_arrival_units = Some(arrival_units);
                self.first_timestamp = Some(packet.header.timestamp);
                0
            }
        };
        if let Some(previous) = self.previous_transit {
            let difference = transit.abs_diff(previous) as f64;
            self.jitter += (difference - self.jitter) / 16.0;
        }
        self.previous_transit = Some(transit);
        Ok(())
    }

    pub fn observe_sender_report(&mut self, ntp: u64, received_at: Instant) {
        self.last_sender_report = Some((ntp_middle_32(ntp), received_at));
    }

    pub fn set_last_rtt(&mut self, value: Duration) {
        self.last_rtt = Some(value);
    }

    pub fn snapshot(&self) -> ReceptionSnapshot {
        let expected = self.expected_packets();
        ReceptionSnapshot {
            packets_received: self.received,
            packets_expected: expected,
            packets_lost: expected as i64 - self.received as i64,
            extended_highest_sequence: self.extended_highest_sequence(),
            interarrival_jitter: self.jitter.round().clamp(0.0, u32::MAX as f64) as u32,
            last_rtt: self.last_rtt,
        }
    }

    pub fn report_block(&mut self, now: Instant) -> ReportBlock {
        let expected = self.expected_packets();
        let expected_interval = expected.saturating_sub(self.expected_prior);
        let received_interval = self.received.saturating_sub(self.received_prior);
        let lost_interval = expected_interval as i64 - received_interval as i64;
        let fraction_lost = if expected_interval == 0 || lost_interval <= 0 {
            0
        } else {
            ((lost_interval.saturating_mul(256) / expected_interval as i64).clamp(0, 255)) as u8
        };
        self.expected_prior = expected;
        self.received_prior = self.received;
        let cumulative_lost =
            (expected as i64 - self.received as i64).clamp(-8_388_608, 8_388_607) as i32;
        let (last_sender_report, delay_since_last_sender_report) = self
            .last_sender_report
            .map(|(middle, received_at)| {
                let delay = now.saturating_duration_since(received_at).as_secs_f64() * 65_536.0;
                (middle, delay.clamp(0.0, u32::MAX as f64) as u32)
            })
            .unwrap_or((0, 0));
        ReportBlock {
            source_ssrc: self.source_ssrc,
            fraction_lost,
            cumulative_lost,
            extended_highest_sequence: self.extended_highest_sequence(),
            interarrival_jitter: self.jitter.round().clamp(0.0, u32::MAX as f64) as u32,
            last_sender_report,
            delay_since_last_sender_report,
        }
    }

    fn extended_highest_sequence(&self) -> u32 {
        self.sequence_cycles + u32::from(self.maximum_sequence)
    }

    fn expected_packets(&self) -> u64 {
        let Some(base) = self.base_sequence else {
            return 0;
        };
        u64::from(self.extended_highest_sequence())
            .saturating_sub(u64::from(base))
            .saturating_add(1)
    }
}

/// RFC 3550 §6.3.1-style RTCP transmission interval.
///
/// The calculation is intentionally small: it exposes the bandwidth and
/// membership inputs and the randomized 0.5–1.5 factor, while keeping the
/// session's existing configurable `report_interval` as the default.
#[derive(Clone, Debug)]
pub struct RtcpInterval {
    members: usize,
    senders: usize,
    rtcp_bandwidth: f64,
    avg_rtcp_size: f64,
    initial: bool,
    seed: u64,
}

impl RtcpInterval {
    pub fn new(
        members: usize,
        senders: usize,
        bandwidth_bps: f64,
        avg_rtcp_size: f64,
    ) -> Result<Self> {
        if members == 0 || senders > members {
            return Err(Error::Limit("RTCP membership"));
        }
        if !bandwidth_bps.is_finite() || bandwidth_bps <= 0.0 {
            return Err(Error::Limit("RTCP bandwidth"));
        }
        if !avg_rtcp_size.is_finite() || avg_rtcp_size <= 0.0 {
            return Err(Error::Limit("RTCP average packet size"));
        }
        Ok(Self {
            members,
            senders,
            rtcp_bandwidth: bandwidth_bps,
            avg_rtcp_size,
            initial: true,
            seed: 0x9e37_79b9_7f4a_7c15,
        })
    }

    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed.max(1);
        self
    }

    /// Name/length word of the next packet updates the average size estimate.
    pub fn observe_packet_size(&mut self, bytes: usize) {
        let bytes = bytes as f64;
        self.avg_rtcp_size = self.avg_rtcp_size * 0.9375 + bytes * 0.0625;
    }

    /// Compute the next interval; random factor is in [0.5, 1.5].
    pub fn next_interval(&mut self) -> Duration {
        let min = if self.initial { 2.5 } else { 5.0 };
        self.initial = false;
        let share = if self.senders * 4 <= self.members {
            0.25
        } else {
            1.0
        };
        let bandwidth = self.rtcp_bandwidth * 0.05 * share;
        let deterministic = self.avg_rtcp_size * self.members as f64 / bandwidth;
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let random = (self.seed >> 11) as f64 / (1u64 << 53) as f64;
        let seconds = deterministic.max(min) * (0.5 + random);
        Duration::from_secs_f64(seconds.max(min * 0.5))
    }
}

#[derive(Default)]
pub struct RttEstimator {
    sent_reports: HashMap<u32, Instant>,
    maximum_reports: usize,
}

impl RttEstimator {
    pub fn new(maximum_reports: usize) -> Result<Self> {
        if maximum_reports == 0 {
            return Err(Error::Limit("RTCP RTT history"));
        }
        Ok(Self {
            sent_reports: HashMap::new(),
            maximum_reports,
        })
    }

    pub fn record_sender_report(&mut self, ntp: u64, sent_at: Instant) {
        if self.sent_reports.len() >= self.maximum_reports
            && let Some(oldest) = self
                .sent_reports
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(key, _)| *key)
        {
            self.sent_reports.remove(&oldest);
        }
        self.sent_reports.insert(ntp_middle_32(ntp), sent_at);
    }

    pub fn observe_report(
        &mut self,
        report: &ReportBlock,
        received_at: Instant,
    ) -> Option<Duration> {
        let sent_at = self.sent_reports.remove(&report.last_sender_report)?;
        let elapsed = received_at.saturating_duration_since(sent_at);
        let remote_delay =
            Duration::from_secs_f64(f64::from(report.delay_since_last_sender_report) / 65_536.0);
        Some(elapsed.saturating_sub(remote_delay))
    }
}

pub struct RtcpSession {
    local_ssrc: u32,
    cname: Bytes,
    report_interval: Duration,
    dynamic_interval: Option<RtcpInterval>,
    next_report: Instant,
    reception: ReceptionStats,
    rtt: RttEstimator,
    feedback: FeedbackSnapshot,
    collision: bool,
}

impl RtcpSession {
    pub fn new(
        local_ssrc: u32,
        remote_ssrc: u32,
        clock_rate: u32,
        cname: impl Into<Bytes>,
        report_interval: Duration,
        now: Instant,
    ) -> Result<Self> {
        if report_interval == Duration::ZERO {
            return Err(Error::Limit("RTCP report interval"));
        }
        let cname = cname.into();
        if cname.is_empty() || cname.len() > u8::MAX as usize {
            return Err(Error::Limit("RTCP CNAME"));
        }
        Ok(Self {
            local_ssrc,
            cname,
            report_interval,
            dynamic_interval: None,
            next_report: now + report_interval,
            reception: ReceptionStats::new(remote_ssrc, clock_rate, now)?,
            rtt: RttEstimator::new(32)?,
            feedback: FeedbackSnapshot::default(),
            collision: false,
        })
    }

    pub fn on_rtp(&mut self, packet: &RtpPacket, received_at: Instant) -> Result<()> {
        self.reception.observe_rtp(packet, received_at)
    }

    pub fn on_rtcp(&mut self, packet: &Packet, received_at: Instant) {
        let remote_ssrc = match packet {
            Packet::SenderReport { ssrc, .. }
            | Packet::SenderReportWithReports { ssrc, .. }
            | Packet::ReceiverReport { ssrc }
            | Packet::ReceiverReportWithReports { ssrc, .. } => Some(*ssrc),
            Packet::SourceDescription { chunks } => chunks.first().map(|chunk| chunk.ssrc),
            _ => None,
        };
        if remote_ssrc == Some(self.local_ssrc) {
            self.collision = true;
        }
        match packet {
            Packet::SenderReport { ntp, .. } => {
                self.reception.observe_sender_report(*ntp, received_at);
            }
            Packet::SenderReportWithReports { ntp, reports, .. } => {
                self.reception.observe_sender_report(*ntp, received_at);
                if let Some(rtt) = reports
                    .iter()
                    .find_map(|report| self.rtt.observe_report(report, received_at))
                {
                    self.reception.set_last_rtt(rtt);
                }
            }
            Packet::ReceiverReportWithReports { reports, .. } => {
                if let Some(rtt) = reports
                    .iter()
                    .find_map(|report| self.rtt.observe_report(report, received_at))
                {
                    self.reception.set_last_rtt(rtt);
                }
            }
            Packet::RtpFeedback { fmt: 1, fci, .. } => {
                self.feedback.nack_entries = self
                    .feedback
                    .nack_entries
                    .saturating_add((fci.len() / 4) as u64);
            }
            Packet::PayloadFeedback { fmt, .. } => match fmt {
                1 => self.feedback.pli = self.feedback.pli.saturating_add(1),
                4 => self.feedback.fir = self.feedback.fir.saturating_add(1),
                15 => {
                    if let Some(bitrate) = packet.remb_bitrate() {
                        self.feedback.remb = Some(bitrate);
                    }
                }
                _ => {}
            },
            Packet::App { .. } => {
                self.feedback.app = self.feedback.app.saturating_add(1);
            }
            _ => {}
        }
    }

    pub fn record_sender_report(&mut self, ntp: u64, sent_at: Instant) {
        self.rtt.record_sender_report(ntp, sent_at);
    }

    /// Opt into RFC 3550-style bandwidth/membership interval calculation.
    pub fn with_interval(mut self, interval: RtcpInterval) -> Self {
        self.dynamic_interval = Some(interval);
        self
    }

    pub fn poll_report(&mut self, now: Instant) -> Result<Option<CompoundPacket>> {
        if now < self.next_report {
            return Ok(None);
        }
        let interval = self
            .dynamic_interval
            .as_mut()
            .map(RtcpInterval::next_interval)
            .unwrap_or(self.report_interval);
        self.next_report = now + interval;
        self.report_now(now).map(Some)
    }

    pub fn report_now(&mut self, now: Instant) -> Result<CompoundPacket> {
        CompoundPacket::new(vec![
            Packet::ReceiverReportWithReports {
                ssrc: self.local_ssrc,
                reports: vec![self.reception.report_block(now)],
            },
            Packet::SourceDescription {
                chunks: vec![SdesChunk {
                    ssrc: self.local_ssrc,
                    items: vec![SdesItem {
                        kind: 1,
                        value: self.cname.clone(),
                    }],
                }],
            },
        ])
    }

    /// Build a BYE (+SDES CNAME) compound packet for graceful shutdown.
    pub fn bye(&self, reason: Option<&str>) -> Result<CompoundPacket> {
        CompoundPacket::new(vec![
            Packet::ReceiverReport {
                ssrc: self.local_ssrc,
            },
            Packet::ByeSources {
                sources: vec![self.local_ssrc],
                reason: reason.map(str::to_owned),
            },
            Packet::SourceDescription {
                chunks: vec![SdesChunk {
                    ssrc: self.local_ssrc,
                    items: vec![SdesItem {
                        kind: 1,
                        value: self.cname.clone(),
                    }],
                }],
            },
        ])
    }

    pub fn snapshot(&self) -> ReceptionSnapshot {
        self.reception.snapshot()
    }

    pub fn feedback(&self) -> FeedbackSnapshot {
        self.feedback
    }

    /// An RTCP packet used our local SSRC (RFC 3550 §8.2 collision).
    pub fn collision_detected(&self) -> bool {
        self.collision
    }
}

pub const fn ntp_middle_32(ntp: u64) -> u32 {
    (ntp >> 16) as u32
}
