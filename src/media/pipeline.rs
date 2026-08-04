//! Extensible bounded media pipeline inspired by GStreamer element graphs.
#[path = "bus.rs"]
mod bus;
pub use bus::{FanoutOutcome, FrameBus};

use crate::{
    Error, Result,
    error::Operation,
    media::AccessUnit,
    observe::{EventSink, FrameworkEvent, NoopEventSink, PipelineEvent, emit},
    runtime::{ShutdownReport, TaskSpec, TaskSupervisor},
};
use futures::future::{BoxFuture, ready};
use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ElementId(usize);
impl ElementId {
    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipelineState {
    Created,
    Ready,
    Playing,
    Paused,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StateTransition {
    pub from: PipelineState,
    pub to: PipelineState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverflowPolicy {
    /// Preserve every frame and propagate backpressure to the upstream element.
    Block,
    /// Preserve real-time latency by dropping a new frame when the queue is full.
    DropNewest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueConfig {
    pub capacity: usize,
    pub overflow: OverflowPolicy,
    /// Maximum time an upstream element may wait for a full blocking queue.
    pub block_timeout: std::time::Duration,
}
impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            capacity: 32,
            overflow: OverflowPolicy::DropNewest,
            block_timeout: std::time::Duration::from_millis(100),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ElementOutput {
    Drop,
    One(AccessUnit),
    Many(Vec<AccessUnit>),
}
impl ElementOutput {
    fn into_units(self) -> Vec<AccessUnit> {
        match self {
            Self::Drop => Vec::new(),
            Self::One(unit) => vec![unit],
            Self::Many(units) => units,
        }
    }
}

/// A pipeline node. Implementations must keep `process` non-blocking; hardware,
/// disk, or CPU-heavy work should be delegated to a dedicated worker.
pub trait Element: Send + Sync {
    fn name(&self) -> &str;

    fn change_state<'a>(&'a self, _transition: StateTransition) -> BoxFuture<'a, Result<()>> {
        Box::pin(ready(Ok(())))
    }

    fn process<'a>(&'a self, input: AccessUnit) -> BoxFuture<'a, Result<ElementOutput>>;
}

#[derive(Default)]
pub struct IdentityElement {
    name: String,
}
impl IdentityElement {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}
impl Element for IdentityElement {
    fn name(&self) -> &str {
        &self.name
    }

    fn process<'a>(&'a self, input: AccessUnit) -> BoxFuture<'a, Result<ElementOutput>> {
        Box::pin(ready(Ok(ElementOutput::One(input))))
    }
}

struct NodeSpec {
    element: Arc<dyn Element>,
    input: QueueConfig,
}
#[derive(Clone, Copy)]
struct LinkSpec {
    from: ElementId,
    to: ElementId,
    queue: QueueConfig,
}

pub struct PipelineBuilder {
    nodes: Vec<NodeSpec>,
    links: Vec<LinkSpec>,
    event_sink: Arc<dyn EventSink>,
    shutdown_grace: std::time::Duration,
}
impl Default for PipelineBuilder {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            links: Vec::new(),
            event_sink: Arc::new(NoopEventSink),
            shutdown_grace: std::time::Duration::from_secs(5),
        }
    }
}
impl PipelineBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_element(
        &mut self,
        element: Arc<dyn Element>,
        input: QueueConfig,
    ) -> Result<ElementId> {
        validate_queue(input)?;
        let id = ElementId(self.nodes.len());
        self.nodes.push(NodeSpec { element, input });
        Ok(id)
    }

    pub fn event_sink(&mut self, event_sink: Arc<dyn EventSink>) -> &mut Self {
        self.event_sink = event_sink;
        self
    }

    pub fn shutdown_grace(&mut self, grace: std::time::Duration) -> Result<&mut Self> {
        if grace == std::time::Duration::ZERO {
            return Err(Error::Limit("pipeline shutdown grace"));
        }
        self.shutdown_grace = grace;
        Ok(self)
    }

    pub fn link(
        &mut self,
        from: ElementId,
        to: ElementId,
        queue: QueueConfig,
    ) -> Result<&mut Self> {
        validate_queue(queue)?;
        if from == to || from.0 >= self.nodes.len() || to.0 >= self.nodes.len() {
            return Err(Error::Media("invalid pipeline link endpoints".into()));
        }
        if self
            .links
            .iter()
            .any(|link| link.from == from && link.to == to)
        {
            return Err(Error::Conflict("pipeline link already exists".into()));
        }
        self.links.push(LinkSpec { from, to, queue });
        Ok(self)
    }

    pub async fn build(self) -> Result<Pipeline> {
        if self.nodes.is_empty() {
            return Err(Error::Media("pipeline has no elements".into()));
        }
        validate_acyclic(self.nodes.len(), &self.links)?;

        let transition = StateTransition {
            from: PipelineState::Created,
            to: PipelineState::Ready,
        };
        let mut prepared: Vec<Arc<dyn Element>> = Vec::new();
        for node in &self.nodes {
            if let Err(error) = node.element.change_state(transition).await {
                for element in prepared.into_iter().rev() {
                    if let Err(rollback_error) = element
                        .change_state(StateTransition {
                            from: PipelineState::Ready,
                            to: PipelineState::Stopped,
                        })
                        .await
                    {
                        tracing::error!(error=%rollback_error, "pipeline element rollback failed");
                    }
                }
                return Err(error);
            }
            prepared.push(node.element.clone());
        }

        let supervisor = TaskSupervisor::new(self.event_sink.clone());
        let cancel = supervisor.cancellation_token();
        let (state_tx, state_rx) = watch::channel(PipelineState::Ready);
        let mut inputs = Vec::with_capacity(self.nodes.len());
        let mut receivers = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let (sender, receiver) = mpsc::channel(node.input.capacity);
            inputs.push(IngressPort {
                sender,
                queue: node.input,
            });
            receivers.push(Some(receiver));
        }

        let stats = (0..self.nodes.len())
            .map(|_| Arc::new(NodeCounters::default()))
            .collect::<Vec<_>>();
        let mut outgoing_by_node = (0..self.nodes.len())
            .map(|_| Vec::new())
            .collect::<Vec<_>>();
        for link in &self.links {
            let (edge_sender, mut edge_receiver) = mpsc::channel(link.queue.capacity);
            outgoing_by_node[link.from.0].push(EdgeRuntime {
                sender: edge_sender,
                queue: link.queue,
                dropped: Arc::new(AtomicU64::new(0)),
            });
            let target = inputs[link.to.0].sender.clone();
            let edge_cancel = cancel.clone();
            supervisor.spawn(
                TaskSpec::new(
                    format!("pipeline-edge-{}-{}", link.from.0, link.to.0),
                    Operation::PipelineProcess,
                ),
                async move {
                    loop {
                        let unit = tokio::select! {
                            _ = edge_cancel.cancelled() => return Ok(()),
                            unit = edge_receiver.recv() => match unit {
                                Some(unit) => unit,
                                None => return Ok(()),
                            },
                        };
                        if target.send(unit).await.is_err() {
                            return Ok(());
                        }
                    }
                },
            );
        }
        for (index, node) in self.nodes.iter().enumerate() {
            let outgoing = std::mem::take(&mut outgoing_by_node[index]);
            let runtime = NodeRuntime {
                element: node.element.clone(),
                input: receivers[index]
                    .take()
                    .ok_or_else(|| Error::Media("pipeline input state is inconsistent".into()))?,
                outgoing,
                state: state_rx.clone(),
                cancel: cancel.clone(),
                counters: stats[index].clone(),
                event_sink: self.event_sink.clone(),
            };
            supervisor.spawn(
                TaskSpec::new(
                    format!("pipeline-element-{}-{}", index, node.element.name()),
                    Operation::PipelineProcess,
                ),
                runtime.run(),
            );
        }

        Ok(Pipeline {
            elements: self.nodes.into_iter().map(|node| node.element).collect(),
            inputs,
            state: Mutex::new(PipelineState::Ready),
            transition_lock: tokio::sync::Mutex::new(()),
            state_tx,
            cancel,
            supervisor,
            shutdown_grace: self.shutdown_grace,
            stats,
        })
    }
}

#[derive(Clone)]
struct IngressPort {
    sender: mpsc::Sender<AccessUnit>,
    queue: QueueConfig,
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
    elements: Vec<Arc<dyn Element>>,
    inputs: Vec<IngressPort>,
    state: Mutex<PipelineState>,
    transition_lock: tokio::sync::Mutex<()>,
    state_tx: watch::Sender<PipelineState>,
    cancel: CancellationToken,
    supervisor: TaskSupervisor,
    shutdown_grace: std::time::Duration,
    stats: Vec<Arc<NodeCounters>>,
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
struct NodeCounters {
    processed: AtomicU64,
    dropped: AtomicU64,
    errors: AtomicU64,
}
struct EdgeRuntime {
    sender: mpsc::Sender<AccessUnit>,
    queue: QueueConfig,
    dropped: Arc<AtomicU64>,
}
struct NodeRuntime {
    element: Arc<dyn Element>,
    input: mpsc::Receiver<AccessUnit>,
    outgoing: Vec<EdgeRuntime>,
    state: watch::Receiver<PipelineState>,
    cancel: CancellationToken,
    counters: Arc<NodeCounters>,
    event_sink: Arc<dyn EventSink>,
}
impl NodeRuntime {
    async fn run(mut self) -> Result<()> {
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

async fn deliver(
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

fn validate_queue(queue: QueueConfig) -> Result<()> {
    if queue.capacity == 0 {
        Err(Error::Limit("pipeline queue capacity"))
    } else if queue.block_timeout == std::time::Duration::ZERO {
        Err(Error::Limit("pipeline block timeout"))
    } else {
        Ok(())
    }
}

fn validate_acyclic(nodes: usize, links: &[LinkSpec]) -> Result<()> {
    let mut outgoing = vec![Vec::new(); nodes];
    let mut indegree = vec![0usize; nodes];
    let mut unique = HashSet::new();
    for link in links {
        if !unique.insert((link.from, link.to)) {
            return Err(Error::Conflict("pipeline link already exists".into()));
        }
        outgoing[link.from.0].push(link.to.0);
        indegree[link.to.0] += 1;
    }
    let mut ready = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| (*degree == 0).then_some(index))
        .collect::<VecDeque<_>>();
    let mut visited = 0;
    while let Some(node) = ready.pop_front() {
        visited += 1;
        for &next in &outgoing[node] {
            indegree[next] -= 1;
            if indegree[next] == 0 {
                ready.push_back(next);
            }
        }
    }
    if visited == nodes {
        Ok(())
    } else {
        Err(Error::Conflict("pipeline graph contains a cycle".into()))
    }
}

fn valid_transition(from: PipelineState, to: PipelineState) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Codec;
    use bytes::Bytes;
    use std::time::Duration;

    struct Sink {
        frames: Arc<Mutex<Vec<AccessUnit>>>,
    }
    impl Element for Sink {
        fn name(&self) -> &str {
            "sink"
        }

        fn process<'a>(&'a self, input: AccessUnit) -> BoxFuture<'a, Result<ElementOutput>> {
            self.frames.lock().unwrap().push(input);
            Box::pin(ready(Ok(ElementOutput::Drop)))
        }
    }

    fn frame() -> AccessUnit {
        AccessUnit {
            track_id: 0,
            codec: Codec::H264,
            pts: Duration::ZERO,
            dts: None,
            rtp_timestamp: 1,
            is_keyframe: true,
            data: Bytes::from_static(&[0x65, 1]),
        }
    }

    #[tokio::test]
    async fn graph_forwards_and_preserves_bytes_storage() {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let mut builder = PipelineBuilder::new();
        let source = builder
            .add_element(
                Arc::new(IdentityElement::new("source")),
                QueueConfig::default(),
            )
            .unwrap();
        let sink = builder
            .add_element(
                Arc::new(Sink {
                    frames: frames.clone(),
                }),
                QueueConfig::default(),
            )
            .unwrap();
        builder.link(source, sink, QueueConfig::default()).unwrap();
        let pipeline = builder.build().await.unwrap();
        pipeline.set_state(PipelineState::Playing).await.unwrap();
        let input = frame();
        let pointer = input.data.as_ptr();
        assert_eq!(
            pipeline.ingress().push(source, input).await.unwrap(),
            IngressOutcome::Accepted
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while frames.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(frames.lock().unwrap()[0].data.as_ptr(), pointer);
        assert_eq!(pipeline.snapshot().elements[0].2.processed, 1);
        pipeline.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn blocking_queue_has_a_deadline() {
        let (sender, _receiver) = mpsc::channel(1);
        sender.send(frame()).await.unwrap();
        let result = deliver(
            &sender,
            QueueConfig {
                capacity: 1,
                overflow: OverflowPolicy::Block,
                block_timeout: Duration::from_millis(1),
            },
            frame(),
        )
        .await;
        assert!(matches!(
            result,
            Err(Error::Limit("pipeline blocking queue"))
        ));
    }

    #[tokio::test]
    async fn rejects_cycles_and_bounded_ingress_drops() {
        let mut builder = PipelineBuilder::new();
        let queue = QueueConfig {
            capacity: 1,
            overflow: OverflowPolicy::DropNewest,
            block_timeout: Duration::from_millis(100),
        };
        let first = builder
            .add_element(Arc::new(IdentityElement::new("a")), queue)
            .unwrap();
        let second = builder
            .add_element(Arc::new(IdentityElement::new("b")), queue)
            .unwrap();
        builder.link(first, second, queue).unwrap();
        assert!(builder.link(second, first, queue).is_ok());
        assert!(builder.build().await.is_err());

        let mut builder = PipelineBuilder::new();
        let node = builder
            .add_element(Arc::new(IdentityElement::new("paused")), queue)
            .unwrap();
        let pipeline = builder.build().await.unwrap();
        assert_eq!(
            pipeline.ingress().try_push(node, frame()).unwrap(),
            IngressOutcome::Accepted
        );
        assert_eq!(
            pipeline.ingress().try_push(node, frame()).unwrap(),
            IngressOutcome::DroppedQueueFull
        );
    }
}
