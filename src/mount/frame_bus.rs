//! Byte-budgeted broadcast bus for mount frames.
//!
//! `tokio::sync::broadcast` bounds the ring by frame count only. A 4K
//! keyframe and a tiny audio frame therefore consume very different amounts
//! of memory. This bus keeps both bounds: at most `max_frames` entries and at
//! most `max_bytes` of frame payload. Slow subscribers observe a `Lagged`
//! count and resume at the oldest retained frame, matching broadcast
//! semantics.
#![allow(dead_code)]

use crate::media::AccessUnit;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameRecvError {
    /// Number of frames evicted before the receiver could read them.
    Lagged(usize),
    Closed,
}

impl std::fmt::Display for FrameRecvError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lagged(count) => write!(formatter, "frame bus lagged by {count}"),
            Self::Closed => formatter.write_str("frame bus closed"),
        }
    }
}

impl std::error::Error for FrameRecvError {}

struct State {
    frames: VecDeque<Arc<AccessUnit>>,
    bytes: usize,
    front_sequence: u64,
    next_sequence: u64,
}

pub(crate) struct FrameBus {
    state: Mutex<State>,
    notify: Notify,
    receivers: AtomicUsize,
    closed: AtomicBool,
    max_frames: usize,
    max_bytes: usize,
}

impl FrameBus {
    pub(crate) fn new(max_frames: usize, max_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                frames: VecDeque::new(),
                bytes: 0,
                front_sequence: 0,
                next_sequence: 0,
            }),
            notify: Notify::new(),
            receivers: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            max_frames: max_frames.max(1),
            max_bytes: max_bytes.max(1),
        })
    }

    pub(crate) fn subscribe(self: &Arc<Self>) -> FrameReceiver {
        self.receivers.fetch_add(1, Ordering::AcqRel);
        let cursor = {
            let state = self.lock();
            state.next_sequence
        };
        FrameReceiver {
            bus: self.clone(),
            cursor,
            closed: false,
        }
    }

    pub(crate) fn receiver_count(&self) -> usize {
        self.receivers.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Publish one frame, evicting the oldest entries until both budgets fit.
    /// Returns the number of receivers that were notified (0 is valid).
    pub(crate) fn send(&self, frame: Arc<AccessUnit>) -> Result<(), FrameRecvError> {
        if self.is_closed() {
            return Err(FrameRecvError::Closed);
        }
        if frame.data.len() > self.max_bytes {
            return Err(FrameRecvError::Lagged(0));
        }
        {
            let mut state = self.lock();
            state.bytes = state.bytes.saturating_add(frame.data.len());
            state.frames.push_back(frame);
            state.next_sequence = state.next_sequence.saturating_add(1);
            while state.frames.len() > self.max_frames
                || (state.bytes > self.max_bytes && state.frames.len() > 1)
            {
                let Some(removed) = state.frames.pop_front() else {
                    break;
                };
                state.bytes = state.bytes.saturating_sub(removed.data.len());
                state.front_sequence = state.front_sequence.saturating_add(1);
            }
        }
        self.notify.notify_waiters();
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub struct FrameReceiver {
    bus: Arc<FrameBus>,
    cursor: u64,
    closed: bool,
}

impl FrameReceiver {
    pub async fn recv(&mut self) -> Result<Arc<AccessUnit>, FrameRecvError> {
        loop {
            // Register before inspecting state so a send between lock release
            // and await cannot be lost.
            let notified = self.bus.notify.notified();
            {
                let state = self.bus.lock();
                if self.cursor < state.front_sequence {
                    let dropped = (state.front_sequence - self.cursor) as usize;
                    self.cursor = state.front_sequence;
                    return Err(FrameRecvError::Lagged(dropped));
                }
                if self.cursor < state.next_sequence {
                    let index = (self.cursor - state.front_sequence) as usize;
                    let Some(frame) = state.frames.get(index).cloned() else {
                        self.closed = true;
                        return Err(FrameRecvError::Closed);
                    };
                    self.cursor = self.cursor.saturating_add(1);
                    return Ok(frame);
                }
                if self.bus.is_closed() {
                    self.closed = true;
                    return Err(FrameRecvError::Closed);
                }
            }
            notified.await;
        }
    }

    pub fn try_recv(&mut self) -> Result<Arc<AccessUnit>, FrameRecvError> {
        let state = self.bus.lock();
        if self.cursor < state.front_sequence {
            let dropped = (state.front_sequence - self.cursor) as usize;
            self.cursor = state.front_sequence;
            return Err(FrameRecvError::Lagged(dropped));
        }
        if self.cursor < state.next_sequence {
            let index = (self.cursor - state.front_sequence) as usize;
            let Some(frame) = state.frames.get(index).cloned() else {
                return Err(FrameRecvError::Closed);
            };
            self.cursor = self.cursor.saturating_add(1);
            return Ok(frame);
        }
        if self.bus.is_closed() {
            return Err(FrameRecvError::Closed);
        }
        Err(FrameRecvError::Lagged(0))
    }
}

impl Drop for FrameReceiver {
    fn drop(&mut self) {
        self.bus.receivers.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Codec;
    use std::time::Duration;

    fn frame(timestamp: u32, size: usize) -> AccessUnit {
        AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: timestamp,
            is_keyframe: true,
            data: bytes::Bytes::from(vec![0u8; size]),
        }
    }

    #[tokio::test]
    async fn byte_budget_evicts_oldest_and_reports_lag() {
        let bus = FrameBus::new(16, 10);
        let mut receiver = bus.subscribe();
        bus.send(Arc::new(frame(1, 6))).unwrap();
        bus.send(Arc::new(frame(2, 6))).unwrap();
        assert!(matches!(
            receiver.recv().await,
            Err(FrameRecvError::Lagged(1))
        ));
        let delivered = receiver.recv().await.unwrap();
        assert_eq!(delivered.rtp_timestamp, 2);
    }

    #[tokio::test]
    async fn recv_wakes_on_send_and_on_close() {
        let bus = FrameBus::new(4, 1024);
        let mut receiver = bus.subscribe();
        let sender = bus.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            sender.send(Arc::new(frame(7, 1))).unwrap();
        });
        let delivered = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered.rtp_timestamp, 7);

        let mut receiver = bus.subscribe();
        let sender = bus.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            sender.close();
        });
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap(),
            Err(FrameRecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_without_evicting() {
        let bus = FrameBus::new(4, 8);
        let mut receiver = bus.subscribe();
        assert!(bus.send(Arc::new(frame(1, 16))).is_err());
        bus.send(Arc::new(frame(2, 4))).unwrap();
        assert_eq!(receiver.recv().await.unwrap().rtp_timestamp, 2);
    }
}
