//! RTP timestamp generation for hardware-encoder integration.
//!
//! Encoders (HiSilicon HiMPP VENC, V4L2, ...) deliver frame presentation
//! timestamps in microseconds or arbitrary units; RTP needs a monotonic
//! clock in the negotiated rate (typically 90 kHz for video, the codec
//! sample rate for audio) that wraps at 32 bits without ever stepping
//! backwards. [`RtpClock`] bridges the two.

use std::time::Duration;

/// Generates monotonic 32-bit RTP timestamps from frame PTS values.
///
/// The input PTS may be in any unit (microseconds, nanoseconds, ticks) as
/// long as it is linear; only *differences* are used. Non-monotonic input
/// (small jitter, duplicate PTS) is clamped so the output never steps
/// backwards — receivers treat timestamp regression as a disorder/loss
/// signal.
#[derive(Clone, Debug)]
pub struct RtpClock {
    /// RTP clock rate in Hz (e.g. 90_000).
    rate: u64,
    /// Last input PTS, for difference computation.
    last_pts: Option<Duration>,
    /// Wrap-free output clock in rate ticks.
    extended_ticks: u64,
}

impl RtpClock {
    /// Create a clock for the given RTP rate (must be non-zero).
    pub fn new(rate: u32) -> Self {
        Self {
            rate: u64::from(rate.max(1)),
            last_pts: None,
            extended_ticks: 0,
        }
    }

    /// Map a frame PTS to the next RTP timestamp.
    ///
    /// The first call returns 0; every later call advances by
    /// `round(delta_pts * rate)` ticks, clamped to non-negative so a
    /// slightly re-ordered or duplicated PTS does not produce a
    /// timestamp regression.
    pub fn timestamp(&mut self, pts: Duration) -> u32 {
        match self.last_pts {
            None => {
                self.last_pts = Some(pts);
                self.extended_ticks = 0;
                0
            }
            Some(last) => {
                let delta = pts.saturating_sub(last);
                self.last_pts = Some(pts);
                let delta_ticks = (delta.as_nanos() as u128 * u128::from(self.rate)
                    / 1_000_000_000u128) as u64;
                self.extended_ticks = self.extended_ticks.wrapping_add(delta_ticks);
                self.extended_ticks as u32
            }
        }
    }

    /// Map an already-rate-scaled tick count to an RTP timestamp.
    ///
    /// Use this when the encoder already produces 90 kHz (or sample-rate)
    /// timestamps: `timestamp_ticks(ticks)` returns `ticks as u32` and
    /// only guards against regression (the caller still owns the mapping
    /// between PTS and ticks).
    pub fn timestamp_ticks(&mut self, ticks: u64) -> u32 {
        let ticks = ticks.max(self.extended_ticks);
        self.extended_ticks = ticks;
        ticks as u32
    }

    /// Current wrap-free tick position (useful for RTCP SR bookkeeping).
    pub fn position_ticks(&self) -> u64 {
        self.extended_ticks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_starts_at_zero_and_advances_at_rate() {
        let mut clock = RtpClock::new(90_000);
        assert_eq!(clock.timestamp(Duration::ZERO), 0);
        assert_eq!(
            clock.timestamp(Duration::from_micros(33_333)),
            2999 // 33.333ms * 90k = 2999.97 → floor 2999 ticks
        );
        assert_eq!(clock.timestamp(Duration::from_micros(66_666)), 5998);
    }

    #[test]
    fn regression_is_clamped() {
        let mut clock = RtpClock::new(90_000);
        clock.timestamp(Duration::ZERO);
        clock.timestamp(Duration::from_millis(40));
        // Duplicate / slightly earlier PTS must not step the clock back.
        assert_eq!(clock.timestamp(Duration::from_millis(40)), 3600);
        assert_eq!(clock.timestamp(Duration::from_millis(80)), 7200);
    }

    #[test]
    fn audio_rate_44_1k() {
        let mut clock = RtpClock::new(44_100);
        clock.timestamp(Duration::ZERO);
        // 1024 samples per AAC frame at 44.1kHz ≈ 23.22ms.
        let ts = clock.timestamp(Duration::from_micros(23_220));
        assert_eq!(ts, 1024);
    }

    #[test]
    fn wraps_at_32_bits_while_staying_monotonic() {
        let mut clock = RtpClock::new(90_000);
        // Push far past the 32-bit wrap (≈13.2h at 90kHz).
        let mut prev = 0u32;
        for frame in 0..2_000_000u64 {
            let pts = Duration::from_micros(frame * 33_333);
            let ts = clock.timestamp(pts);
            assert!(
                ts.wrapping_sub(prev) < 1 << 30,
                "timestamp jumped: {prev} -> {ts}"
            );
            prev = ts;
        }
        // The clock must have wrapped at least once (13.2h > 66.6s*2M).
        assert!(clock.position_ticks() > (1u64 << 32));
    }
}
