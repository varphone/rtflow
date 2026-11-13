//! Bounded media queue with drop-oldest policy.
//!
//! Tokio's mpsc drops the newest item when full, which keeps stale frames in
//! a slow consumer and increases end-to-end latency. Media is different:
//! dropping the oldest queued frame keeps playback close to the live edge.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

use tokio::sync::Notify;

use crate::{AccessUnit, Result};

struct State {
    queue: VecDeque<Result<AccessUnit>>,
    bytes: usize,
    capacity: usize,
    max_bytes: usize,
    senders: usize,
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    notify: Notify,
}

fn lock(inner: &Inner) -> MutexGuard<'_, State> {
    inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) struct FrameSender {
    inner: Arc<Inner>,
}

pub(super) struct FrameReceiver {
    inner: Arc<Inner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SendOutcome {
    /// Frames evicted to make room for the new frame.
    pub dropped: usize,
}

impl FrameSender {
    pub(super) fn try_send(
        &self,
        frame: Result<AccessUnit>,
    ) -> std::result::Result<SendOutcome, ()> {
        let mut state = lock(&self.inner);
        if state.closed {
            return Err(());
        }
        let size = frame.as_ref().map_or(0, |unit| unit.data.len());
        let mut dropped = 0;
        if size > state.max_bytes {
            // Never let one oversized frame evict the whole useful queue.
            return Ok(SendOutcome { dropped: 1 });
        }
        while !state.queue.is_empty()
            && (state.queue.len() >= state.capacity
                || state.bytes.saturating_add(size) > state.max_bytes)
        {
            if let Some(old) = state.queue.pop_front() {
                state.bytes = state
                    .bytes
                    .saturating_sub(old.as_ref().map_or(0, |unit| unit.data.len()));
                dropped += 1;
            }
        }
        state.bytes = state.bytes.saturating_add(size);
        state.queue.push_back(frame);
        drop(state);
        self.inner.notify.notify_one();
        Ok(SendOutcome { dropped })
    }
}

impl Clone for FrameSender {
    fn clone(&self) -> Self {
        lock(&self.inner).senders += 1;
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for FrameSender {
    fn drop(&mut self) {
        let mut state = lock(&self.inner);
        state.senders = state.senders.saturating_sub(1);
        let closed = state.senders == 0;
        drop(state);
        if closed {
            self.inner.notify.notify_waiters();
        }
    }
}

impl FrameReceiver {
    pub(super) fn new(capacity: usize, max_bytes: usize) -> (FrameSender, Self) {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                queue: VecDeque::with_capacity(capacity.max(1)),
                bytes: 0,
                capacity: capacity.max(1),
                max_bytes: max_bytes.max(1),
                senders: 1,
                closed: false,
            }),
            notify: Notify::new(),
        });
        (
            FrameSender {
                inner: inner.clone(),
            },
            Self { inner },
        )
    }

    pub(super) async fn recv(&mut self) -> Option<Result<AccessUnit>> {
        loop {
            // Register before checking state so a push between the check and
            // the await cannot be lost.
            let notified = self.inner.notify.notified();
            {
                let mut state = lock(&self.inner);
                if let Some(frame) = state.queue.pop_front() {
                    state.bytes = state
                        .bytes
                        .saturating_sub(frame.as_ref().map_or(0, |unit| unit.data.len()));
                    return Some(frame);
                }
                if state.closed || state.senders == 0 {
                    return None;
                }
            }
            notified.await;
        }
    }
}

impl Drop for FrameReceiver {
    fn drop(&mut self) {
        let mut state = lock(&self.inner);
        state.closed = true;
        state.queue.clear();
        state.bytes = 0;
        drop(state);
        self.inner.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccessUnit, Codec};
    use std::time::Duration;

    fn unit(timestamp: u32) -> AccessUnit {
        AccessUnit::video(
            0,
            Codec::H264,
            Duration::ZERO,
            timestamp,
            true,
            bytes::Bytes::from_static(&[1, 2, 3, 4]),
        )
    }

    #[tokio::test]
    async fn full_queue_evicts_oldest() {
        let (tx, mut rx) = FrameReceiver::new(2, 1 << 20);
        tx.try_send(Ok(unit(1))).unwrap();
        tx.try_send(Ok(unit(2))).unwrap();
        let outcome = tx.try_send(Ok(unit(3))).unwrap();
        assert_eq!(outcome.dropped, 1);
        assert_eq!(rx.recv().await.unwrap().unwrap().rtp_timestamp, 2);
        assert_eq!(rx.recv().await.unwrap().unwrap().rtp_timestamp, 3);
    }

    #[tokio::test]
    async fn byte_budget_evicts_and_closed_receiver_rejects() {
        let (tx, mut rx) = FrameReceiver::new(8, 6);
        tx.try_send(Ok(unit(1))).unwrap();
        let outcome = tx.try_send(Ok(unit(2))).unwrap();
        assert_eq!(outcome.dropped, 1);
        assert_eq!(rx.recv().await.unwrap().unwrap().rtp_timestamp, 2);
        drop(rx);
        assert!(tx.try_send(Ok(unit(3))).is_err());
    }
}
