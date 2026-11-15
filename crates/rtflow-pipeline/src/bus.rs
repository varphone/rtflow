//! Bounded fan-out primitives. Slow consumers never backpressure a live source.
use rtflow_media::AccessUnit;
use tokio::sync::broadcast;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanoutOutcome {
    Delivered,
    NoConsumers,
    Lagged,
}
#[derive(Clone)]
pub struct FrameBus {
    tx: broadcast::Sender<AccessUnit>,
}
impl FrameBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<AccessUnit> {
        self.tx.subscribe()
    }
    pub fn receivers(&self) -> usize {
        self.tx.receiver_count()
    }
    pub fn publish(&self, unit: AccessUnit) -> FanoutOutcome {
        if self.tx.receiver_count() == 0 {
            return FanoutOutcome::NoConsumers;
        }
        match self.tx.send(unit) {
            Ok(_) => FanoutOutcome::Delivered,
            Err(_) => FanoutOutcome::Lagged,
        }
    }
}
