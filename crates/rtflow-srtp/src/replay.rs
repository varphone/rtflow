//! Sliding-window replay protection for SRTP/SRTCP (RFC 3711 §3.3.2).
//!
//! Uses a 128-bit window to track received packet indices and reject duplicates
//! or packets that fall too far behind the leading edge.

const WINDOW_BITS: u64 = 128;

/// Replay window that tracks received packet indices.
///
/// The window is centered on `leading_index` and extends `WINDOW_BITS` positions
/// behind it. Packets ahead of the leading edge advance the window; packets
/// within the window are checked against a bitmask; packets behind the window
/// are rejected as too old.
#[derive(Clone, Debug)]
pub struct ReplayWindow {
    leading_index: u64,
    /// Bitmask of received indices relative to `leading_index`.
    /// Bit 0 corresponds to `leading_index`, bit 1 to `leading_index - 1`, etc.
    window: u128,
}

impl ReplayWindow {
    /// Create a new empty replay window.
    pub fn new() -> Self {
        Self {
            leading_index: 0,
            window: 0,
        }
    }

    /// Check whether `index` is acceptable and, if so, record it.
    ///
    /// Returns `true` if the packet should be processed, `false` if it is a
    /// duplicate or too old.
    pub fn check(&mut self, index: u64) -> bool {
        if index > self.leading_index {
            // Advance the window.
            let shift = index - self.leading_index;
            if shift < WINDOW_BITS {
                self.window <<= shift;
                self.window |= 1;
            } else {
                // Jumped past the entire window.
                self.window = 1;
            }
            self.leading_index = index;
            true
        } else {
            let delta = self.leading_index - index;
            if delta >= WINDOW_BITS {
                // Too old.
                return false;
            }
            let bit = 1u128 << delta;
            if self.window & bit != 0 {
                // Duplicate.
                false
            } else {
                self.window |= bit;
                true
            }
        }
    }

    /// Current leading (highest accepted) index.
    #[allow(dead_code)] // used by unit tests and diagnostics
    pub fn leading_index(&self) -> u64 {
        self.leading_index
    }
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_first_packet() {
        let mut w = ReplayWindow::new();
        assert!(w.check(0));
        assert_eq!(w.leading_index(), 0);
    }

    #[test]
    fn accepts_sequential_packets() {
        let mut w = ReplayWindow::new();
        for i in 0..256 {
            assert!(w.check(i), "packet {i} should be accepted");
        }
        assert_eq!(w.leading_index(), 255);
    }

    #[test]
    fn rejects_duplicate() {
        let mut w = ReplayWindow::new();
        assert!(w.check(42));
        assert!(!w.check(42));
    }

    #[test]
    fn accepts_out_of_order_within_window() {
        let mut w = ReplayWindow::new();
        assert!(w.check(100));
        assert!(w.check(50));
        assert!(w.check(99));
        assert!(!w.check(50)); // duplicate
        assert!(!w.check(100)); // duplicate
    }

    #[test]
    fn rejects_too_old() {
        let mut w = ReplayWindow::new();
        assert!(w.check(200));
        // Index 71 is 129 positions behind 200 — outside the 128-bit window.
        assert!(!w.check(71));
    }

    #[test]
    fn rejects_exactly_at_boundary() {
        let mut w = ReplayWindow::new();
        assert!(w.check(128));
        // Index 0 is exactly 128 positions behind — at the boundary, should be rejected.
        assert!(!w.check(0));
        // Index 1 is 127 positions behind — within the window, should be accepted.
        assert!(w.check(1));
    }

    #[test]
    fn large_jump_resets_window() {
        let mut w = ReplayWindow::new();
        assert!(w.check(10));
        assert!(w.check(10_000));
        assert_eq!(w.leading_index(), 10_000);
        // Old index should be rejected.
        assert!(!w.check(10));
        // New sequential should work.
        assert!(w.check(10_001));
    }
}
