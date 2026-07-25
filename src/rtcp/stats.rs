use super::{CompoundPacket, Packet, ReportBlock, SdesChunk, SdesItem};
use crate::{Error, Result, rtp::RtpPacket};
use bytes::Bytes;
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

const RTP_SEQUENCE_MODULUS: u32 = 1 << 16;

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
        let transit = arrival_units - i64::from(packet.header.timestamp);
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
    next_report: Instant,
    reception: ReceptionStats,
    rtt: RttEstimator,
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
            next_report: now + report_interval,
            reception: ReceptionStats::new(remote_ssrc, clock_rate, now)?,
            rtt: RttEstimator::new(32)?,
        })
    }

    pub fn on_rtp(&mut self, packet: &RtpPacket, received_at: Instant) -> Result<()> {
        self.reception.observe_rtp(packet, received_at)
    }

    pub fn on_rtcp(&mut self, packet: &Packet, received_at: Instant) {
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
            _ => {}
        }
    }

    pub fn record_sender_report(&mut self, ntp: u64, sent_at: Instant) {
        self.rtt.record_sender_report(ntp, sent_at);
    }

    pub fn poll_report(&mut self, now: Instant) -> Result<Option<CompoundPacket>> {
        if now < self.next_report {
            return Ok(None);
        }
        self.next_report = now + self.report_interval;
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

    pub fn snapshot(&self) -> ReceptionSnapshot {
        self.reception.snapshot()
    }
}

pub const fn ntp_middle_32(ntp: u64) -> u32 {
    (ntp >> 16) as u32
}
