use super::*;

#[derive(Clone)]
pub(super) struct IngressPort {
    pub(super) sender: mpsc::Sender<AccessUnit>,
    pub(super) queue: QueueConfig,
}

#[derive(Clone)]
pub struct PipelineIngress {
    inputs: Vec<IngressPort>,
}
impl PipelineIngress {
    pub async fn push(&self, target: ElementId, unit: AccessUnit) -> Result<IngressOutcome> {
        let port = self
            .inputs
            .get(target.0)
            .ok_or_else(|| Error::NotFound(format!("pipeline element {}", target.0)))?;
        deliver(&port.sender, port.queue, unit).await
    }

    pub fn try_push(&self, target: ElementId, unit: AccessUnit) -> Result<IngressOutcome> {
        let port = self
            .inputs
            .get(target.0)
            .ok_or_else(|| Error::NotFound(format!("pipeline element {}", target.0)))?;
        match port.sender.try_send(unit) {
            Ok(()) => Ok(IngressOutcome::Accepted),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(IngressOutcome::DroppedQueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Closed),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IngressOutcome {
    Accepted,
    DroppedQueueFull,
}

#[derive(Clone, Debug, Default)]
pub struct ElementStats {
    pub processed: u64,
    pub dropped: u64,
    pub errors: u64,
}

#[derive(Clone, Debug)]
pub struct PipelineSnapshot {
    pub state: PipelineState,
    pub elements: Vec<(ElementId, String, ElementStats)>,
}

pub struct Pipeline {
    pub(super) elements: Vec<Arc<dyn Element>>,
    pub(super) inputs: Vec<IngressPort>,
    pub(super) state: Mutex<PipelineState>,
    pub(super) transition_lock: tokio::sync::Mutex<()>,
    pub(super) state_tx: watch::Sender<PipelineState>,
    pub(super) cancel: CancellationToken,
    pub(super) supervisor: TaskSupervisor,
    pub(super) shutdown_grace: std::time::Duration,
    pub(super) stats: Vec<Arc<NodeCounters>>,
}
impl Pipeline {
    pub fn ingress(&self) -> PipelineIngress {
        PipelineIngress {
            inputs: self.inputs.clone(),
        }
    }

    pub fn state(&self) -> PipelineState {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub async fn set_state(&self, to: PipelineState) -> Result<()> {
        let _transition_guard = self.transition_lock.lock().await;
        let from = self.state();
        if from == to {
            return Ok(());
        }
        if !valid_transition(from, to) {
            return Err(Error::Conflict(format!(
                "invalid pipeline transition {from:?} -> {to:?}"
            )));
        }
        let transition = StateTransition { from, to };
        let mut changed: Vec<Arc<dyn Element>> = Vec::new();
        for element in &self.elements {
            if let Err(error) = element.change_state(transition).await {
                for element in changed.into_iter().rev() {
                    if let Err(rollback_error) = element
                        .change_state(StateTransition { from: to, to: from })
                        .await
                    {
                        tracing::error!(error=%rollback_error, "pipeline state rollback failed");
                    }
                }
                return Err(error);
            }
            changed.push(element.clone());
        }
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = to;
        self.state_tx.send_replace(to);
        if to == PipelineState::Stopped {
            self.cancel.cancel();
        }
        Ok(())
    }

    pub fn snapshot(&self) -> PipelineSnapshot {
        PipelineSnapshot {
            state: self.state(),
            elements: self
                .elements
                .iter()
                .zip(&self.stats)
                .enumerate()
                .map(|(index, (element, counters))| {
                    (
                        ElementId(index),
                        element.name().to_owned(),
                        ElementStats {
                            processed: counters.processed.load(Ordering::Relaxed),
                            dropped: counters.dropped.load(Ordering::Relaxed),
                            errors: counters.errors.load(Ordering::Relaxed),
                        },
                    )
                })
                .collect(),
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.shutdown_with_report().await.map(drop)
    }

    pub async fn shutdown_with_report(&self) -> Result<ShutdownReport> {
        self.set_state(PipelineState::Stopped).await?;
        Ok(self.supervisor.shutdown_after(self.shutdown_grace).await)
    }
}
impl Drop for Pipeline {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Default)]
pub(super) struct NodeCounters {
    pub(super) processed: AtomicU64,
    pub(super) dropped: AtomicU64,
    pub(super) errors: AtomicU64,
}
pub(super) struct EdgeRuntime {
    pub(super) sender: mpsc::Sender<AccessUnit>,
    pub(super) queue: QueueConfig,
    pub(super) dropped: Arc<AtomicU64>,
}
pub(super) struct NodeRuntime {
    pub(super) element: Arc<dyn Element>,
    pub(super) input: mpsc::Receiver<AccessUnit>,
    pub(super) outgoing: Vec<EdgeRuntime>,
    pub(super) state: watch::Receiver<PipelineState>,
    pub(super) cancel: CancellationToken,
    pub(super) counters: Arc<NodeCounters>,
    pub(super) event_sink: Arc<dyn EventSink>,
}
impl NodeRuntime {
    pub(super) async fn run(mut self) -> Result<()> {
        loop {
            while *self.state.borrow() != PipelineState::Playing {
                if *self.state.borrow() == PipelineState::Stopped {
                    return Ok(());
                }
                tokio::select! {
                    _ = self.cancel.cancelled() => return Ok(()),
                    changed = self.state.changed() => if changed.is_err() { return Ok(()); },
                }
            }
            let input = tokio::select! {
                _ = self.cancel.cancelled() => return Ok(()),
                changed = self.state.changed() => {
                    if changed.is_err() { return Ok(()); }
                    continue;
                }
                input = self.input.recv() => match input {
                    Some(input) => input,
                    None => return Ok(()),
                },
            };
            let output = match self.element.process(input).await {
                Ok(output) => output,
                Err(error) => {
                    self.counters.errors.fetch_add(1, Ordering::Relaxed);
                    emit(
                        &self.event_sink,
                        FrameworkEvent::Pipeline(PipelineEvent::ElementFailed {
                            element: Arc::from(self.element.name()),
                            code: error.code(),
                        }),
                    );
                    continue;
                }
            };
            self.counters.processed.fetch_add(1, Ordering::Relaxed);
            if matches!(output, ElementOutput::Drop) {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            for unit in output.into_units() {
                for edge in &self.outgoing {
                    match deliver(&edge.sender, edge.queue, unit.clone()).await {
                        Ok(IngressOutcome::Accepted) => {}
                        Ok(IngressOutcome::DroppedQueueFull) => {
                            edge.dropped.fetch_add(1, Ordering::Relaxed);
                            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        // Block-with-timeout expiry is also a drop: count it
                        // so overflow is observable under every policy.
                        Err(_) => {
                            edge.dropped.fetch_add(1, Ordering::Relaxed);
                            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }
}

pub(crate) async fn deliver(
    sender: &mpsc::Sender<AccessUnit>,
    queue: QueueConfig,
    unit: AccessUnit,
) -> Result<IngressOutcome> {
    match queue.overflow {
        OverflowPolicy::Block => tokio::time::timeout(queue.block_timeout, sender.send(unit))
            .await
            .map_err(|_| Error::Limit("pipeline blocking queue"))?
            .map(|_| IngressOutcome::Accepted)
            .map_err(|_| Error::Closed),
        OverflowPolicy::DropNewest => match sender.try_send(unit) {
            Ok(()) => Ok(IngressOutcome::Accepted),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(IngressOutcome::DroppedQueueFull),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Error::Closed),
        },
    }
}

pub(super) fn valid_transition(from: PipelineState, to: PipelineState) -> bool {
    matches!(
        (from, to),
        (PipelineState::Ready, PipelineState::Playing)
            | (PipelineState::Ready, PipelineState::Paused)
            | (PipelineState::Playing, PipelineState::Paused)
            | (PipelineState::Paused, PipelineState::Playing)
            | (PipelineState::Ready, PipelineState::Stopped)
            | (PipelineState::Playing, PipelineState::Stopped)
            | (PipelineState::Paused, PipelineState::Stopped)
    )
}
