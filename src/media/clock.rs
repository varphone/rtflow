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
                let delta_ticks =
                    (delta.as_nanos() * u128::from(self.rate) / 1_000_000_000u128) as u64;
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

/// Convert a [`Duration`] to clock ticks at `rate` Hz, truncating toward
/// zero. Integer-exact (no float rounding drift over long runs) and the
/// single reference implementation for every `Duration → ticks` mapping.
pub fn duration_to_ticks(d: Duration, rate: u32) -> u64 {
    let rate = u64::from(rate);
    d.as_secs().saturating_mul(rate)
        + u64::from(d.subsec_nanos()).saturating_mul(rate) / 1_000_000_000
}

/// Round-to-nearest variant of [`duration_to_ticks`] (MPEG-TS PTS
/// convention: half a tick added before truncation).
pub fn duration_to_ticks_rounded(d: Duration, rate: u32) -> u64 {
    let rate = u64::from(rate);
    d.as_secs().saturating_mul(rate)
        + (u64::from(d.subsec_nanos()).saturating_mul(rate) + 500_000_000) / 1_000_000_000
}

/// Convert clock ticks at `rate` Hz back to a [`Duration`].
pub fn ticks_to_duration(ticks: u64, rate: u32) -> Duration {
    let rate = u64::from(rate).max(1);
    Duration::new(ticks / rate, ((ticks % rate) * 1_000_000_000 / rate) as u32)
}

/// RTP clock rate for video tracks (H.264/H.265/VP8/VP9 all use 90 kHz).
pub const VIDEO_CLOCK_RATE: u32 = 90_000;

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

#[cfg(test)]
mod conversion_tests {
    use super::*;

    #[test]
    fn duration_to_ticks_truncates_int_exactly() {
        // 33.333ms at 90 kHz = 2999.97 ticks → floor 2999 (no float drift).
        assert_eq!(
            duration_to_ticks(Duration::from_micros(33_333), 90_000),
            2999
        );
        assert_eq!(
            duration_to_ticks(Duration::from_micros(66_666), 90_000),
            5999
        );
        // Whole seconds are exact.
        assert_eq!(duration_to_ticks(Duration::from_secs(2), 90_000), 180_000);
        // Sub-nanosecond remainder truncates to zero.
        assert_eq!(duration_to_ticks(Duration::from_nanos(1), 44_100), 0);
    }

    #[test]
    fn duration_to_ticks_rounded_half_tick_up() {
        // 33.333ms at 90 kHz: 2999.97 → round 3000.
        assert_eq!(
            duration_to_ticks_rounded(Duration::from_micros(33_333), 90_000),
            3000
        );
        // Exactly on a tick boundary is unchanged.
        assert_eq!(
            duration_to_ticks_rounded(Duration::from_millis(40), 90_000),
            3600
        );
        // Half a tick rounds up.
        assert_eq!(
            duration_to_ticks_rounded(Duration::from_nanos(5_555_555), 90_000),
            500
        );
    }

    #[test]
    fn ticks_to_duration_round_trips() {
        let cases = [
            (0u64, 90_000u32),
            (1, 90_000),
            (2999, 90_000),
            (1024, 44_100),
            (u32::MAX as u64, 8_000),
        ];
        for (ticks, rate) in cases {
            let d = ticks_to_duration(ticks, rate);
            // Truncation loses at most one tick on the way back (the
            // fractional tick of the inverse conversion).
            let back = duration_to_ticks(d, rate);
            assert!(
                back == ticks || back + 1 == ticks,
                "round trip {ticks}@{rate}: got {back}"
            );
        }
        assert_eq!(ticks_to_duration(90_000, 90_000), Duration::from_secs(1));
        assert_eq!(ticks_to_duration(44_100, 44_100), Duration::from_secs(1));
    }

    #[test]
    fn duration_to_ticks_never_loses_a_tick_to_float() {
        // Float math can under-count by one on exact boundaries
        // (703ms * 90000 = 63270 exactly, but f64 gives 63269.9999…).
        // The integer implementation must hit the exact value.
        assert_eq!(
            duration_to_ticks(Duration::from_millis(703), 90_000),
            63_270
        );
        // And stay exact across a long run: every integer millisecond.
        for ms in 0..10_000u32 {
            let d = Duration::from_millis(u64::from(ms));
            let int = duration_to_ticks(d, 90_000);
            let float = (d.as_secs_f64() * 90_000.0) as u64;
            assert!(
                int == float || int == float + 1,
                "ms={ms}: int {int} vs float {float}"
            );
        }
    }
}
