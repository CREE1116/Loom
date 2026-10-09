use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

pub trait Clock {
    fn now_ms(&self) -> u64;
}

#[derive(Clone, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    pub fn set(&self, time: u64) {
        self.0.store(time, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
