//! Per-session seekable VOD scheduling.
use crate::{
    Error, Result,
    error::Operation,
    runtime::{TaskHandle, TaskSpec, TaskSupervisor},
    storage::VodSource,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

pub(super) struct VodPlayback {
    task: TaskHandle,
    position_us: Arc<AtomicU64>,
}

impl VodPlayback {
    pub(super) fn stop(self) -> Duration {
        self.task.cancel();
        Duration::from_micros(self.position_us.load(Ordering::Acquire))
    }
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) fn spawn_vod_playback(
    supervisor: &TaskSupervisor,
    source: Arc<dyn VodSource>,
    output: broadcast::Sender<crate::AccessUnit>,
    start: Duration,
    end: Option<Duration>,
    prefetch: usize,
) -> Result<VodPlayback> {
    if prefetch == 0 {
        return Err(Error::Limit("VOD prefetch"));
    }
    let position_us = Arc::new(AtomicU64::new(duration_us(start)));
    let task_position = position_us.clone();
    let task = supervisor.spawn(
        TaskSpec::new("vod-playback", Operation::ReadMedia),
        async move {
            let worker_cancel = CancellationToken::new();
            let cancel_guard = CancelOnDrop(worker_cancel.clone());
            let (sender, mut receiver) = mpsc::channel(prefetch);
            let worker = tokio::task::spawn_blocking(move || -> Result<()> {
                let mut demuxer = source.open()?;
                demuxer.seek(start)?;
                while !worker_cancel.is_cancelled() {
                    let Some(sample) = demuxer.next_sample()? else {
                        break;
                    };
                    if sender.blocking_send(sample).is_err() {
                        break;
                    }
                }
                Ok(())
            });
            let wall_origin = tokio::time::Instant::now();
            while let Some(sample) = receiver.recv().await {
                let position = sample.unit.pts;
                if end.is_some_and(|end| position > end) {
                    break;
                }
                let deadline = wall_origin
                    .checked_add(position.saturating_sub(start))
                    .ok_or(Error::Limit("VOD playback deadline"))?;
                tokio::time::sleep_until(deadline).await;
                task_position.store(duration_us(position).saturating_add(1), Ordering::Release);
                if output.send(sample.unit).is_err() {
                    break;
                }
            }
            cancel_guard.0.cancel();
            match worker.await {
                Ok(result) => result?,
                Err(error) => return Err(Error::Media(format!("VOD worker failed: {error}"))),
            }
            Ok(())
        },
    );
    Ok(VodPlayback { task, position_us })
}

fn duration_us(value: Duration) -> u64 {
    u64::try_from(value.as_micros()).unwrap_or(u64::MAX)
}
