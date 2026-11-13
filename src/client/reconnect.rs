use super::*;

fn jitter_sample() -> u32 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        let mut bytes = [0u8; 8];
        state = if getrandom::getrandom(&mut bytes).is_ok() {
            u64::from_le_bytes(bytes)
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        };
        state = state.max(1);
        let _ = STATE.compare_exchange(0, state, Ordering::Relaxed, Ordering::Relaxed);
    }
    loop {
        let mut next = state;
        next ^= next << 13;
        next ^= next >> 7;
        next ^= next << 17;
        next = next.max(1);
        if STATE
            .compare_exchange(state, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return ((next >> 32) % 101) as u32;
        }
        state = STATE.load(Ordering::Relaxed).max(1);
    }
}

/// Exponential backoff with per-call random jitter (±25%). Random (not
/// attempt-deterministic) so clients that failed together do not reconnect
/// in one synchronized herd.
pub(super) fn jittered_backoff(delay: Duration, _attempt: u32) -> Duration {
    let sample = jitter_sample() as f32 / 100.0;
    delay.mul_f32(0.75 + 0.5 * sample)
}

pub struct ReconnectingPlaySession {
    pub description: MediaDescription,
    pub(super) id: SessionId,
    pub(super) receiver: super::queue::FrameReceiver,
    pub(super) cancel: CancellationToken,
    pub(super) task: JoinHandle<()>,
}

impl ReconnectingPlaySession {
    /// Stable identifier correlating this session's [`ClientEvent`]s.
    pub fn session_id(&self) -> SessionId {
        self.id
    }

    pub async fn next(&mut self) -> Option<Result<AccessUnit>> {
        self.receiver.recv().await
    }

    pub async fn teardown(self) -> crate::Result<()> {
        self.cancel.cancel();
        self.task.abort();
        Ok(())
    }
}

impl Drop for ReconnectingPlaySession {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
    }
}
