use crate::clock::Clock;
use std::collections::HashMap;

pub struct Cache<C> {
    clock: C,
    entries: HashMap<String, (String, u64)>,
}

impl<C: Clock> Cache<C> {
    pub fn new(clock: C) -> Self {
        Self {
            clock,
            entries: HashMap::new(),
        }
    }
    pub fn insert(&mut self, key: &str, value: &str, ttl_ms: u64) {
        let deadline = self.clock.now_ms().saturating_add(ttl_ms);
        self.entries.insert(key.into(), (value.into(), deadline));
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        let (value, deadline) = self.entries.get(key)?;
        if self.clock.now_ms() >= *deadline {
            None
        } else {
            Some(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    #[test]
    fn expires_at_the_deadline() {
        let clock = ManualClock::default();
        let mut cache = Cache::new(clock.clone());
        cache.insert("session", "value", 10);
        clock.set(9);
        assert_eq!(cache.get("session"), Some("value"));
        clock.set(10);
        assert_eq!(cache.get("session"), None);
    }
}
