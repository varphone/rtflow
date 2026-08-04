//! Cooperative task supervision with bounded shutdown.
use crate::{
    Result,
    error::Operation,
    identity::TaskId,
    observe::{EventSink, FrameworkEvent, NoopEventSink, RuntimeEvent, emit},
};
use futures::FutureExt;
use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, oneshot},
    task::{AbortHandle, JoinHandle},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct TaskSpec {
    pub name: Arc<str>,
    pub operation: Operation,
}

impl TaskSpec {
    pub fn new(name: impl Into<Arc<str>>, operation: Operation) -> Self {
        Self {
            name: name.into(),
            operation,
        }
    }
}

#[derive(Clone)]
pub struct TaskHandle {
    id: TaskId,
    cancel: CancellationToken,
}

impl TaskHandle {
    pub fn id(&self) -> TaskId {
        self.id
    }
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub completed_tasks: usize,
    pub failed_tasks: usize,
    pub panicked_tasks: usize,
    pub cancelled_tasks: usize,
    pub forced_tasks: usize,
    pub timed_out_tasks: usize,
}

#[derive(Default)]
struct CompletionCounters {
    completed: AtomicUsize,
    failed: AtomicUsize,
    panicked: AtomicUsize,
    cancelled: AtomicUsize,
}

struct ActiveTask {
    abort: AbortHandle,
    name: Arc<str>,
}

struct Inner {
    cancel: CancellationToken,
    next_id: AtomicU64,
    active: Mutex<HashMap<TaskId, ActiveTask>>,
    notify: Notify,
    counters: CompletionCounters,
    sink: Arc<dyn EventSink>,
}

#[derive(Clone)]
pub struct TaskSupervisor {
    inner: Arc<Inner>,
}

impl Default for TaskSupervisor {
    fn default() -> Self {
        Self::new(Arc::new(NoopEventSink))
    }
}

impl TaskSupervisor {
    pub fn new(sink: Arc<dyn EventSink>) -> Self {
        Self {
            inner: Arc::new(Inner {
                cancel: CancellationToken::new(),
                next_id: AtomicU64::new(1),
                active: Mutex::new(HashMap::new()),
                notify: Notify::new(),
                counters: CompletionCounters::default(),
                sink,
            }),
        }
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }
    pub fn active_tasks(&self) -> usize {
        lock_active(&self.inner).len()
    }

    pub fn spawn<F>(&self, spec: TaskSpec, future: F) -> TaskHandle
    where
        F: Future<Output = Result<()>> + Send + 'static,
    {
        let id = TaskId::new(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let task_cancel = self.inner.cancel.child_token();
        let weak = Arc::downgrade(&self.inner);
        let task_name = spec.name.clone();
        let operation = spec.operation;
        let local_cancel = task_cancel.clone();
        let (registered_tx, registered_rx) = oneshot::channel();
        emit(
            &self.inner.sink,
            FrameworkEvent::Runtime(RuntimeEvent::TaskStarted {
                id,
                name: spec.name.clone(),
            }),
        );
        let join: JoinHandle<()> = tokio::spawn(async move {
            if registered_rx.await.is_err() {
                return;
            }
            let outcome = AssertUnwindSafe(async {
                tokio::select! {
                    _ = local_cancel.cancelled() => None,
                    result = future => Some(result),
                }
            })
            .catch_unwind()
            .await;
            finish_task(weak, id, task_name, operation, outcome);
        });
        lock_active(&self.inner).insert(
            id,
            ActiveTask {
                abort: join.abort_handle(),
                name: spec.name,
            },
        );
        let _ = registered_tx.send(());
        TaskHandle {
            id,
            cancel: task_cancel,
        }
    }

    pub async fn shutdown(&self, deadline: Instant) -> ShutdownReport {
        self.inner.cancel.cancel();
        while self.active_tasks() > 0 {
            let notified = self.inner.notify.notified();
            // Re-check after registering the waiter: a task may have
            // finished between the loop condition and waiter registration.
            // Combined with `notify_one` in `finish_task` (which retains a
            // permit when no waiter is present), this closes the lost-wakeup
            // window that previously made shutdown wait out the full grace.
            if self.active_tasks() == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                break;
            }
        }
        let remaining = {
            let mut active = lock_active(&self.inner);
            active.drain().collect::<Vec<_>>()
        };
        for (id, task) in &remaining {
            task.abort.abort();
            emit(
                &self.inner.sink,
                FrameworkEvent::Runtime(RuntimeEvent::TaskAborted {
                    id: *id,
                    name: task.name.clone(),
                }),
            );
        }
        ShutdownReport {
            completed_tasks: self.inner.counters.completed.load(Ordering::Relaxed),
            failed_tasks: self.inner.counters.failed.load(Ordering::Relaxed),
            panicked_tasks: self.inner.counters.panicked.load(Ordering::Relaxed),
            cancelled_tasks: self.inner.counters.cancelled.load(Ordering::Relaxed),
            forced_tasks: remaining.len(),
            timed_out_tasks: remaining.len(),
        }
    }

    pub async fn shutdown_after(&self, grace: Duration) -> ShutdownReport {
        self.shutdown(Instant::now() + grace).await
    }
}

fn lock_active(inner: &Inner) -> std::sync::MutexGuard<'_, HashMap<TaskId, ActiveTask>> {
    inner
        .active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn finish_task(
    weak: Weak<Inner>,
    id: TaskId,
    name: Arc<str>,
    operation: Operation,
    outcome: std::thread::Result<Option<Result<()>>>,
) {
    let Some(inner) = weak.upgrade() else {
        return;
    };
    match outcome {
        Ok(Some(Ok(()))) => {
            inner.counters.completed.fetch_add(1, Ordering::Relaxed);
            emit(
                &inner.sink,
                FrameworkEvent::Runtime(RuntimeEvent::TaskFinished { id, name }),
            );
        }
        Ok(None) => {
            inner.counters.cancelled.fetch_add(1, Ordering::Relaxed);
            emit(
                &inner.sink,
                FrameworkEvent::Runtime(RuntimeEvent::TaskCancelled { id, name }),
            );
        }
        Ok(Some(Err(error))) => {
            inner.counters.failed.fetch_add(1, Ordering::Relaxed);
            emit(
                &inner.sink,
                FrameworkEvent::Runtime(RuntimeEvent::TaskFailed {
                    id,
                    name,
                    operation,
                    code: error.code(),
                }),
            );
        }
        Err(_) => {
            inner.counters.panicked.fetch_add(1, Ordering::Relaxed);
            emit(
                &inner.sink,
                FrameworkEvent::Runtime(RuntimeEvent::TaskPanicked { id, name }),
            );
        }
    }
    lock_active(&inner).remove(&id);
    // notify_one retains a permit when there is no waiter yet, so a task
    // finishing between the shutdown loop's activity check and its waiter
    // registration cannot lose its wakeup.
    inner.notify.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[tokio::test]
    async fn reports_completed_failed_and_forced_tasks() {
        let supervisor = TaskSupervisor::default();
        supervisor.spawn(TaskSpec::new("ok", Operation::Unknown), async { Ok(()) });
        supervisor.spawn(TaskSpec::new("failed", Operation::ReadMedia), async {
            Err(Error::Closed)
        });
        supervisor.spawn(TaskSpec::new("pending", Operation::ReadMedia), async {
            std::future::pending().await
        });
        tokio::task::yield_now().await;
        let report = supervisor.shutdown_after(Duration::from_millis(20)).await;
        assert_eq!(report.completed_tasks, 1);
        assert_eq!(report.failed_tasks, 1);
        assert_eq!(report.cancelled_tasks, 1);
        assert_eq!(report.forced_tasks, 0);
    }
}
