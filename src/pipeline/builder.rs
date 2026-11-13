use super::runtime::{EdgeRuntime, IngressPort, NodeCounters, NodeRuntime};
use super::*;

pub(super) struct NodeSpec {
    element: Arc<dyn Element>,
    input: QueueConfig,
}
#[derive(Clone, Copy)]
pub(super) struct LinkSpec {
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

pub(super) fn validate_queue(queue: QueueConfig) -> Result<()> {
    if queue.capacity == 0 {
        Err(Error::Limit("pipeline queue capacity"))
    } else if queue.block_timeout == std::time::Duration::ZERO {
        Err(Error::Limit("pipeline block timeout"))
    } else {
        Ok(())
    }
}

pub(super) fn validate_acyclic(nodes: usize, links: &[LinkSpec]) -> Result<()> {
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
