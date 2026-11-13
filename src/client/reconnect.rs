use super::*;

/// Exponential backoff with deterministic jitter (±25%) so that many
/// clients failing at once do not reconnect in a synchronized herd.
pub(super) fn jittered_backoff(delay: Duration, attempt: u32) -> Duration {
    let sample = (attempt.wrapping_mul(1_103_515_245).wrapping_add(12_345) % 101) as f32 / 100.0;
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
