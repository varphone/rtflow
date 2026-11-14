use super::segmenter::Segment;
use std::collections::VecDeque;
use std::sync::RwLock;

/// Hard upper bound for the sliding window. A typo in configuration must not
/// pin arbitrarily many segments (and their keyframes) in memory.
pub const MAX_SEGMENTS: usize = 128;

/// Bounded ring buffer for completed segments.
pub struct SegmentRing {
    segments: RwLock<VecDeque<Segment>>,
    max_segments: usize,
}

impl SegmentRing {
    pub fn new(max_segments: usize) -> Self {
        // Config validation clamps/normalizes this; keep the type itself
        // panic-free even for a direct zero-capacity call.
        let max_segments = max_segments.clamp(1, MAX_SEGMENTS);
        Self {
            segments: RwLock::new(VecDeque::with_capacity(max_segments)),
            max_segments,
        }
    }

    /// Push a new segment, evicting the oldest if at capacity.
    pub fn push(&self, segment: Segment) {
        let mut segments = self
            .segments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A segmenter restart or a racing producer must never insert an
        // older sequence after a newer one: the playlist would go
        // 114,115,116,115 and hls.js would treat it as a seek backwards.
        if segments
            .back()
            .is_some_and(|last| segment.sequence <= last.sequence)
        {
            return;
        }
        if segments.len() >= self.max_segments {
            segments.pop_front();
        }
        segments.push_back(segment);
    }

    /// Get a snapshot of all segments (for playlist generation).
    pub fn snapshot(&self) -> Vec<Segment> {
        let segments = self
            .segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        segments.iter().cloned().collect()
    }

    /// Get the latest init segment data.
    pub fn latest_init(&self) -> Option<bytes::Bytes> {
        let segments = self
            .segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        segments.back().map(|s| s.init.clone())
    }

    /// Get a specific segment by sequence number.
    pub fn get(&self, sequence: u64) -> Option<Segment> {
        let segments = self
            .segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let first = segments.front()?.sequence;
        let last = segments.back()?.sequence;
        if sequence < first || sequence > last {
            return None;
        }
        // Sequences are contiguous; index directly instead of a linear scan.
        let index = (sequence - first) as usize;
        segments
            .get(index)
            .filter(|s| s.sequence == sequence)
            .cloned()
    }

    /// Remove all segments (mount restart/reuse).
    pub fn clear(&self) {
        self.segments
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Number of segments currently stored.
    pub fn len(&self) -> usize {
        self.segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_capacity_is_normalized_without_panicking() {
        let ring = SegmentRing::new(0);
        assert!(ring.is_empty());
    }

    #[test]
    fn oversized_window_is_clamped() {
        let ring = SegmentRing::new(usize::MAX);
        assert_eq!(ring.max_segments, MAX_SEGMENTS);
    }
}
