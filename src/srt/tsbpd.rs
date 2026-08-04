//! SRT TSBPD (Time-Stamp Based Packet Delivery) reorder buffer.
//!
//! Implements the receiver-side packet ordering and timing mechanism
//! defined in the SRT specification. Packets arrive potentially out of
//! order over UDP and are held in a buffer until their scheduled
//! delivery time (arrival + TSBPD delay).
use bytes::Bytes;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Default TSBPD delay (receiver buffer depth).
pub const DEFAULT_TSBPD_DELAY_MS: u32 = 120;

/// Maximum forward sequence jump a single packet may cause before it is
/// treated as an anomaly and dropped. Real traffic stays within the flow
/// window (thousands); a jump near half the sequence space would advance
/// the delivery cursor past every in-flight packet.
pub const MAX_GAP_SKIP: u32 = 1 << 20;

/// A buffered SRT data packet awaiting ordered delivery.
#[derive(Clone, Debug)]
pub struct BufferedPacket {
    /// Packet sequence number.
    pub sequence: u32,
    /// Payload data (typically 188-byte TS packets or fragments).
    pub payload: Bytes,
    /// When this packet was received from the network.
    pub arrival_time: Instant,
    /// Whether this is a retransmitted packet.
    pub retransmitted: bool,
}

/// TSBPD reorder buffer state.
pub struct TsbpdBuffer {
    /// Packets sorted by sequence number.
    buffer: BTreeMap<u32, BufferedPacket>,
    /// TSBPD delay before releasing packets.
    delay: Duration,
    /// Next expected sequence number for in-order delivery.
    next_deliver_seq: u32,
    /// Highest sequence number received (for loss detection).
    highest_received: u32,
    /// Whether we've been initialized with a starting sequence.
    initialized: bool,
    /// Count of packets delivered in order.
    delivered_count: u64,
    /// Count of packets dropped (too late).
    dropped_count: u64,
    /// Count of loss events detected.
    loss_events: u64,
}

impl TsbpdBuffer {
    /// Create a new TSBPD buffer with the given delay.
    pub fn new(delay_ms: u32) -> Self {
        Self {
            buffer: BTreeMap::new(),
            delay: Duration::from_millis(delay_ms as u64),
            next_deliver_seq: 0,
            highest_received: 0,
            initialized: false,
            delivered_count: 0,
            dropped_count: 0,
            loss_events: 0,
        }
    }

    /// Create with default delay (120ms).
    pub fn with_default_delay() -> Self {
        Self::new(DEFAULT_TSBPD_DELAY_MS)
    }

    /// Initialize the buffer with the first expected sequence number.
    pub fn init(&mut self, initial_seq: u32) {
        self.next_deliver_seq = initial_seq;
        self.highest_received = initial_seq;
        self.initialized = true;
    }

    /// Insert a packet into the reorder buffer.
    ///
    /// Returns `true` if the packet was accepted, `false` if it's a
    /// duplicate or too old (already delivered).
    pub fn push(&mut self, seq: u32, payload: Bytes, retransmitted: bool) -> bool {
        if !self.initialized {
            self.init(seq);
        }

        // Reject packets that are too old (before our delivery cursor)
        if seq_before(seq, self.next_deliver_seq) {
            self.dropped_count += 1;
            return false;
        }

        // Reject absurdly large forward jumps: a single packet must not be
        // able to advance the delivery cursor by a huge amount, which would
        // make every in-flight packet look "too old" afterwards.
        if seq_after(seq, self.next_deliver_seq)
            && seq_distance(self.next_deliver_seq, seq) > MAX_GAP_SKIP
        {
            self.dropped_count += 1;
            return false;
        }

        // Reject duplicates
        if self.buffer.contains_key(&seq) {
            return false;
        }

        // Update highest received
        if !self.initialized || seq_after(seq, self.highest_received) {
            self.highest_received = seq;
        }

        self.buffer.insert(
            seq,
            BufferedPacket {
                sequence: seq,
                payload,
                arrival_time: Instant::now(),
                retransmitted,
            },
        );

        true
    }

    /// Release packets that are ready for delivery.
    ///
    /// A packet is ready when:
    /// 1. It's the next expected sequence AND has been buffered for >= delay, OR
    /// 2. It has been buffered for >= 2x delay (timeout, skip gaps)
    ///
    /// Returns packets in sequence order.
    pub fn pop_ready(&mut self) -> Vec<BufferedPacket> {
        let now = Instant::now();
        let mut ready = Vec::new();

        loop {
            // Find the smallest sequence in the buffer
            let (&seq, packet) = match self.buffer.iter().next() {
                Some(entry) => entry,
                None => break,
            };

            let age = now.duration_since(packet.arrival_time);

            if seq == self.next_deliver_seq {
                // In-order: release after TSBPD delay
                if age >= self.delay {
                    let pkt = self.buffer.remove(&seq).unwrap();
                    self.next_deliver_seq = seq.wrapping_add(1);
                    self.delivered_count += 1;
                    ready.push(pkt);
                } else {
                    break; // not yet time
                }
            } else if seq_after(seq, self.next_deliver_seq) {
                // There's a gap. Check if we've waited too long (2x delay timeout)
                if age >= self.delay * 2 {
                    let gap = seq_distance(self.next_deliver_seq, seq);
                    if gap > MAX_GAP_SKIP {
                        // Defensive: an anomalous far-jump packet must not
                        // advance the cursor; drop it and keep waiting.
                        self.buffer.remove(&seq);
                        self.dropped_count += 1;
                        continue;
                    }
                    // Skip the gap: advance cursor and count loss
                    self.loss_events += 1;
                    self.dropped_count += gap as u64;
                    self.next_deliver_seq = seq;
                    // Loop again to deliver this packet
                } else {
                    break; // wait for retransmission
                }
            } else {
                // seq before next_deliver_seq: shouldn't happen (filtered in push)
                self.buffer.remove(&seq);
            }
        }

        ready
    }

    /// Detect sequence gaps and return loss ranges for NAK generation.
    ///
    /// Returns a list of (start, end) inclusive sequence ranges that are missing.
    pub fn detect_losses(&self) -> Vec<(u32, u32)> {
        if !self.initialized || self.buffer.is_empty() {
            return Vec::new();
        }

        let mut losses = Vec::new();
        let mut expected = self.next_deliver_seq;

        for &seq in self.buffer.keys() {
            if seq_after(seq, expected) {
                // Gap from expected to seq-1
                losses.push((expected, seq.wrapping_sub(1)));
            }
            expected = seq.wrapping_add(1);
        }

        losses
    }

    /// Get the ACK number: the next expected sequence (all before this are received).
    pub fn ack_number(&self) -> u32 {
        self.next_deliver_seq
    }

    /// Number of packets currently buffered.
    pub fn buffered_count(&self) -> usize {
        self.buffer.len()
    }

    /// Statistics snapshot.
    pub fn stats(&self) -> TsbpdStats {
        TsbpdStats {
            buffered: self.buffer.len(),
            delivered: self.delivered_count,
            dropped: self.dropped_count,
            loss_events: self.loss_events,
        }
    }
}

/// TSBPD buffer statistics.
#[derive(Clone, Debug, Default)]
pub struct TsbpdStats {
    pub buffered: usize,
    pub delivered: u64,
    pub dropped: u64,
    pub loss_events: u64,
}

// --- Sequence number arithmetic (handles wrapping) ---

/// Returns true if `a` is "after" `b` in sequence space (wrapping).
pub fn seq_after(a: u32, b: u32) -> bool {
    // a is after b if the signed difference (a - b) is positive
    // Using the standard technique: interpret (a - b) as i32
    (a.wrapping_sub(b) as i32) > 0
}

/// Returns true if `a` is "before" `b` in sequence space (wrapping).
pub fn seq_before(a: u32, b: u32) -> bool {
    seq_after(b, a)
}

/// Distance from `a` to `b` (how many steps from a to reach b).
pub fn seq_distance(a: u32, b: u32) -> u32 {
    b.wrapping_sub(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_delivery() {
        let mut buf = TsbpdBuffer::new(0); // 0ms delay for testing
        buf.init(100);

        buf.push(100, Bytes::from_static(b"pkt0"), false);
        buf.push(101, Bytes::from_static(b"pkt1"), false);
        buf.push(102, Bytes::from_static(b"pkt2"), false);

        // With 0 delay, all should be immediately ready
        std::thread::sleep(Duration::from_millis(1));
        let ready = buf.pop_ready();
        assert_eq!(ready.len(), 3);
        assert_eq!(ready[0].sequence, 100);
        assert_eq!(ready[1].sequence, 101);
        assert_eq!(ready[2].sequence, 102);
        assert_eq!(buf.ack_number(), 103);
    }

    #[test]
    fn out_of_order_reorder() {
        let mut buf = TsbpdBuffer::new(0);
        buf.init(0);

        // Arrive out of order: 2, 0, 1
        buf.push(2, Bytes::from_static(b"pkt2"), false);
        buf.push(0, Bytes::from_static(b"pkt0"), false);
        buf.push(1, Bytes::from_static(b"pkt1"), false);

        std::thread::sleep(Duration::from_millis(1));
        let ready = buf.pop_ready();
        assert_eq!(ready.len(), 3);
        assert_eq!(ready[0].sequence, 0);
        assert_eq!(ready[1].sequence, 1);
        assert_eq!(ready[2].sequence, 2);
    }

    #[test]
    fn duplicate_rejected() {
        let mut buf = TsbpdBuffer::new(0);
        buf.init(0);

        assert!(buf.push(0, Bytes::from_static(b"pkt0"), false));
        assert!(!buf.push(0, Bytes::from_static(b"dup"), false)); // duplicate
        assert_eq!(buf.buffered_count(), 1);
    }

    #[test]
    fn loss_detection() {
        let mut buf = TsbpdBuffer::new(50);
        buf.init(0);

        buf.push(0, Bytes::from_static(b"pkt0"), false);
        buf.push(1, Bytes::from_static(b"pkt1"), false);
        // seq 2 is missing
        buf.push(3, Bytes::from_static(b"pkt3"), false);

        let losses = buf.detect_losses();
        assert_eq!(losses.len(), 1);
        assert_eq!(losses[0], (2, 2));
    }

    #[test]
    fn gap_timeout_skip() {
        // Use a very short delay so timeout triggers quickly
        let mut buf = TsbpdBuffer::new(1); // 1ms delay, 2ms timeout
        buf.init(0);

        buf.push(0, Bytes::from_static(b"pkt0"), false);
        // seq 1 is lost
        buf.push(2, Bytes::from_static(b"pkt2"), false);

        // Wait for 2x delay timeout
        std::thread::sleep(Duration::from_millis(5));
        let ready = buf.pop_ready();

        // Should deliver pkt0, then skip gap and deliver pkt2
        assert_eq!(ready.len(), 2);
        assert_eq!(ready[0].sequence, 0);
        assert_eq!(ready[1].sequence, 2);
        assert_eq!(buf.stats().loss_events, 1);
    }

    #[test]
    fn sequence_wrapping() {
        assert!(seq_after(1, 0));
        assert!(seq_after(0, 0xFFFF_FFFF)); // wrap
        assert!(seq_before(0xFFFF_FFFF, 0));
        assert!(!seq_after(5, 5)); // equal
        assert_eq!(seq_distance(0xFFFF_FFFE, 1), 3); // wrap distance
    }
}
