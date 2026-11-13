//! Extensible bounded media pipeline inspired by GStreamer element graphs.
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

mod builder;
mod element;
mod runtime;
#[cfg(test)]
mod tests;

pub use builder::PipelineBuilder;
pub use element::{
    Element, ElementId, ElementOutput, IdentityElement, OverflowPolicy, PipelineState, QueueConfig,
    StateTransition,
};
#[cfg(test)]
pub(crate) use runtime::deliver;
pub use runtime::{ElementStats, IngressOutcome, Pipeline, PipelineIngress, PipelineSnapshot};
