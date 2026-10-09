use crate::clock::Clock;

/// Token bucket: holds up to `capacity` tokens, refilled at `per_second` tokens/s.
pub struct Bucket<C: Clock> {
    clock: C,
    capacity: u64,
    per_second: u64,
    tokens: u64,
    last_ms: u64,
}

impl<C: Clock> Bucket<C> {
    pub fn new(clock: C, capacity: u64, per_second: u64) -> Self {
        let last_ms = clock.now_ms();
        Self { clock, capacity, per_second, tokens: capacity, last_ms }
    }

    fn refill(&mut self) {
        let now = self.clock.now_ms();
        let elapsed_s = (now - self.last_ms) / 1000;
        self.tokens += elapsed_s * self.per_second;
        self.last_ms = now;
    }

    /// Take `n` tokens if available.
    pub fn try_take(&mut self, n: u64) -> bool {
        self.refill();
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}
