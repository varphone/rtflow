use super::*;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ElementId(pub(super) usize);
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
    pub(super) fn into_units(self) -> Vec<AccessUnit> {
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
