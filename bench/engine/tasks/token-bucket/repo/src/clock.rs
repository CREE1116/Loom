/// Time source in milliseconds, injectable for tests.
pub trait Clock {
    fn now_ms(&self) -> u64;
}
