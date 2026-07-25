//! Bounded histogram for latency and distribution metrics.
//!
//! Uses fixed bucket boundaries for zero-allocation recording.
//! All operations use `Ordering::Relaxed` for minimal contention.

use std::sync::atomic::{AtomicU64, Ordering};

/// Fixed-boundary histogram for latency metrics.
///
/// Bucket boundaries are in microseconds. Recording is O(bucket_count) but
/// bucket_count is small (typically 12-15) and all operations are lock-free.
pub struct Histogram {
    buckets: Vec<AtomicU64>,
    boundaries: Vec<f64>,
    sum: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    /// Create a histogram with the given bucket boundaries (in the unit of the recorded values).
    pub fn new(boundaries: &[f64]) -> Self {
        Self {
            buckets: (0..=boundaries.len()).map(|_| AtomicU64::new(0)).collect(),
            boundaries: boundaries.to_vec(),
            sum: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Create a histogram with latency-oriented boundaries (in milliseconds).
    ///
    /// Boundaries: 1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 5s, +Inf
    pub fn latency_ms() -> Self {
        Self::new(&[
            1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0,
        ])
    }

    /// Record a value into the histogram.
    pub fn record(&self, value: f64) {
        let bucket = self
            .boundaries
            .iter()
            .position(|&b| value <= b)
            .unwrap_or(self.boundaries.len());
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        // Store value * 1000 for microsecond precision in the sum.
        self.sum
            .fetch_add((value * 1000.0) as u64, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Get the total number of recorded values.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Get the sum of all recorded values.
    pub fn sum(&self) -> f64 {
        self.sum.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Get the approximate mean of all recorded values.
    pub fn mean(&self) -> f64 {
        let count = self.count();
        if count == 0 {
            0.0
        } else {
            self.sum() / count as f64
        }
    }

    /// Get the count for each bucket (including the +Inf bucket).
    pub fn bucket_counts(&self) -> Vec<(f64, u64)> {
        let mut result = Vec::with_capacity(self.buckets.len());
        for (i, boundary) in self.boundaries.iter().enumerate() {
            result.push((*boundary, self.buckets[i].load(Ordering::Relaxed)));
        }
        // +Inf bucket.
        result.push((
            f64::INFINITY,
            self.buckets[self.boundaries.len()].load(Ordering::Relaxed),
        ));
        result
    }

    /// Get the cumulative count up to and including each bucket.
    /// This is the format used by Prometheus histograms.
    pub fn cumulative_counts(&self) -> Vec<(f64, u64)> {
        let mut result = Vec::with_capacity(self.buckets.len());
        let mut cumulative = 0u64;
        for (i, boundary) in self.boundaries.iter().enumerate() {
            cumulative += self.buckets[i].load(Ordering::Relaxed);
            result.push((*boundary, cumulative));
        }
        cumulative += self.buckets[self.boundaries.len()].load(Ordering::Relaxed);
        result.push((f64::INFINITY, cumulative));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_count() {
        let h = Histogram::latency_ms();
        assert_eq!(h.count(), 0);
        h.record(0.5);
        h.record(15.0);
        h.record(100.0);
        assert_eq!(h.count(), 3);
    }

    #[test]
    fn buckets_are_correct() {
        let h = Histogram::new(&[10.0, 20.0, 30.0]);
        h.record(5.0); // bucket 0 (<=10)
        h.record(15.0); // bucket 1 (<=20)
        h.record(25.0); // bucket 2 (<=30)
        h.record(35.0); // bucket 3 (+Inf)
        let counts = h.bucket_counts();
        assert_eq!(counts[0], (10.0, 1));
        assert_eq!(counts[1], (20.0, 1));
        assert_eq!(counts[2], (30.0, 1));
        assert_eq!(counts[3], (f64::INFINITY, 1));
    }

    #[test]
    fn cumulative_counts() {
        let h = Histogram::new(&[10.0, 20.0]);
        h.record(5.0);
        h.record(5.0);
        h.record(15.0);
        h.record(25.0);
        let cum = h.cumulative_counts();
        assert_eq!(cum[0], (10.0, 2));
        assert_eq!(cum[1], (20.0, 3));
        assert_eq!(cum[2], (f64::INFINITY, 4));
    }

    #[test]
    fn mean_computation() {
        let h = Histogram::new(&[100.0]);
        h.record(10.0);
        h.record(20.0);
        h.record(30.0);
        let mean = h.mean();
        assert!((mean - 20.0).abs() < 0.01);
    }
}
