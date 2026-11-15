use super::segmenter::Segment;
use std::collections::VecDeque;
use std::sync::RwLock;

/// Hard upper bound for the sliding window. A typo in configuration must not
/// pin arbitrarily many segments (and their keyframes) in memory.
pub const MAX_SEGMENTS: usize = 128;

/// Bounded ring buffer for completed segments.
pub struct SegmentRing {
    segments: RwLock<VecDeque<Segment>>,
    /// Playlist window (`EXT-X-MEDIA-SEQUENCE` .. last).
    playlist_segments: usize,
    /// Total retained segments. Larger than the playlist window so a
    /// client that falls behind (background tab, slow segment fetch) can
    /// still fetch a segment that has just left the playlist instead of
    /// retrying a 404 forever.
    max_segments: usize,
}

impl SegmentRing {
    pub fn new(max_segments: usize) -> Self {
        Self::with_retention(max_segments, max_segments)
    }

    /// `playlist_segments` is the advertised window; `retained_segments`
    /// (at least the window) is kept for late segment requests.
    pub fn with_retention(playlist_segments: usize, retained_segments: usize) -> Self {
        let retained = retained_segments.clamp(1, MAX_SEGMENTS);
        let playlist = playlist_segments.clamp(1, retained);
        Self {
            segments: RwLock::new(VecDeque::with_capacity(retained)),
            playlist_segments: playlist,
            max_segments: retained,
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
        let mut snapshot: Vec<Segment> = segments
            .iter()
            .rev()
            .take(self.playlist_segments)
            .cloned()
            .collect();
        // Push already enforces monotonic order, but sorting makes the
        // playlist deterministic even if a future producer changes that.
        snapshot.sort_by_key(|segment| segment.sequence);
        snapshot
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
        // Do not index by (sequence - first): a dropped segment (e.g. a
        // broadcast lag in an older producer) leaves a gap and would make
        // every later segment 404, which hls.js retries indefinitely.
        segments
            .iter()
            .find(|segment| segment.sequence == sequence)
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

#[cfg(test)]
mod retention_tests {
    use super::*;
    use crate::segmenter::Segment;
    use bytes::Bytes;
    use std::time::Duration;

    fn segment(sequence: u64) -> Segment {
        Segment {
            sequence,
            duration: Duration::from_secs(1),
            init: Bytes::new(),
            data: Bytes::new(),
            starts_with_keyframe: true,
            discontinuity: false,
            discontinuity_sequence: None,
            timestamp: Duration::from_secs(sequence),
            format: crate::segmenter::SegmentFormat::Fmp4,
        }
    }

    #[test]
    fn playlist_window_is_smaller_than_retention() {
        let ring = SegmentRing::with_retention(2, 5);
        for sequence in 0..5 {
            ring.push(segment(sequence));
        }
        let playlist = ring.snapshot();
        assert_eq!(
            playlist.iter().map(|s| s.sequence).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(
            ring.get(0).is_some(),
            "evicted playlist segment stays fetchable"
        );
        assert_eq!(ring.len(), 5);
    }
}
