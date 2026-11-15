//! Bounded RTP reordering, duplicate suppression and sequence wrap handling.
use super::RtpPacket;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct JitterBufferConfig {
    pub max_packets: usize,
    pub max_reorder_window: u16,
    pub max_delay: Duration,
}
impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            max_packets: 128,
            max_reorder_window: 64,
            // 40ms is enough for typical LAN/edge jitter while keeping the
            // live edge close; 100ms added a fixed latency even when the
            // network had none.
            max_delay: Duration::from_millis(40),
        }
    }
}

#[derive(Debug)]
pub struct JitterBuffer {
    config: JitterBufferConfig,
    next: Option<u16>,
    pending: BTreeMap<u16, (Instant, RtpPacket)>,
    /// Cached (arrival + max_delay, sequence) of the earliest deadline.
    earliest: Option<(Instant, u16)>,
    primed: bool,
}
impl JitterBuffer {
    pub fn new(config: JitterBufferConfig) -> Self {
        Self {
            config,
            next: None,
            pending: BTreeMap::new(),
            earliest: None,
            primed: false,
        }
    }

    /// Hint the expected first sequence from RTSP `RTP-Info` so the first
    /// packet is released immediately instead of being treated as a gap.
    pub fn prime(&mut self, sequence: u16) {
        if self.next.is_none() {
            self.next = Some(sequence);
            self.primed = true;
        }
    }

    pub fn push(&mut self, packet: RtpPacket) -> Vec<RtpPacket> {
        let sequence = packet.header.sequence;
        // RTP-Info is a hint, not a hard drop rule: if the first packet does
        // not use the announced sequence (stale cache, restarted sender),
        // fall back to normal "first packet wins" behaviour.
        if self.primed {
            self.primed = false;
            if self.next != Some(sequence) {
                self.next = Some(sequence);
            }
        }
        let expected = *self.next.get_or_insert(sequence);
        let distance = sequence.wrapping_sub(expected);
        if distance >= 0x8000 || self.pending.contains_key(&sequence) {
            return Vec::new();
        }
        let arrival = Instant::now();
        let deadline = arrival + self.config.max_delay;
        if self.earliest.is_none_or(|(current, _)| deadline < current) {
            self.earliest = Some((deadline, sequence));
        }
        self.pending.insert(sequence, (arrival, packet));
        if self.pending.len() > self.config.max_packets
            || distance > self.config.max_reorder_window
            || (!self.pending.contains_key(&expected) && self.is_expired())
        {
            self.advance_to_nearest();
        }
        self.drain_contiguous()
    }

    /// Releases packets after `max_delay` even when no newer RTP packet arrives.
    /// Callers should invoke this from a timer while a stream is active.
    pub fn drain_expired(&mut self) -> Vec<RtpPacket> {
        let Some(expected) = self.next else {
            return Vec::new();
        };
        if !self.pending.contains_key(&expected) && self.is_expired() {
            self.advance_to_nearest();
        }
        self.drain_contiguous()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let expected = self.next?;
        if self.pending.contains_key(&expected) {
            return Some(Instant::now());
        }
        self.earliest.map(|(deadline, _)| deadline)
    }

    pub fn reset(&mut self) {
        self.next = None;
        self.pending.clear();
        self.earliest = None;
    }

    fn is_expired(&self) -> bool {
        self.earliest
            .is_some_and(|(deadline, _)| deadline <= Instant::now())
    }

    fn refresh_earliest(&mut self) {
        self.earliest = self
            .pending
            .iter()
            .map(|(sequence, (arrival, _))| (*arrival + self.config.max_delay, *sequence))
            .min_by_key(|(deadline, _)| *deadline);
    }

    fn advance_to_nearest(&mut self) {
        let Some(expected) = self.next else { return };
        if let Some(candidate) = self
            .pending
            .keys()
            .copied()
            .min_by_key(|sequence| sequence.wrapping_sub(expected))
        {
            self.next = Some(candidate);
        }
    }

    fn drain_contiguous(&mut self) -> Vec<RtpPacket> {
        let mut ready = Vec::new();
        while let Some(next) = self.next {
            let Some((_, packet)) = self.pending.remove(&next) else {
                break;
            };
            if self.earliest.is_some_and(|(_, sequence)| sequence == next) {
                self.refresh_earliest();
            }
            ready.push(packet);
            self.next = Some(next.wrapping_add(1));
        }
        ready
    }
}
