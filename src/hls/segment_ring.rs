use super::segmenter::Segment;
use std::collections::VecDeque;
use std::sync::RwLock;

/// Bounded ring buffer for completed segments.
pub struct SegmentRing {
    segments: RwLock<VecDeque<Segment>>,
    max_segments: usize,
}

impl SegmentRing {
    pub fn new(max_segments: usize) -> Self {
        Self {
            segments: RwLock::new(VecDeque::with_capacity(max_segments)),
            max_segments,
        }
    }

    /// Push a new segment, evicting the oldest if at capacity.
    pub fn push(&self, segment: Segment) {
        let mut segments = self.segments.write().unwrap();
        if segments.len() >= self.max_segments {
            segments.pop_front();
        }
        segments.push_back(segment);
    }

    /// Get a snapshot of all segments (for playlist generation).
    pub fn snapshot(&self) -> Vec<Segment> {
        let segments = self.segments.read().unwrap();
        segments.iter().cloned().collect()
    }

    /// Get the latest init segment data.
    pub fn latest_init(&self) -> Option<bytes::Bytes> {
        let segments = self.segments.read().unwrap();
        segments.back().map(|s| s.init.clone())
    }

    /// Get a specific segment by sequence number.
    pub fn get(&self, sequence: u64) -> Option<Segment> {
        let segments = self.segments.read().unwrap();
        segments.iter().find(|s| s.sequence == sequence).cloned()
    }

    /// Number of segments currently stored.
    pub fn len(&self) -> usize {
        self.segments.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.read().unwrap().is_empty()
    }
}
