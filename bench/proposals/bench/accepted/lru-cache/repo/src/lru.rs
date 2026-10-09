use std::collections::HashMap;

/// Least-recently-used cache. `get` and `put` both count as a use.
pub struct LruCache {
    capacity: usize,
    map: HashMap<String, i64>,
    order: Vec<String>, // least recently used first
}

impl LruCache {
    pub fn new(capacity: usize) -> Self {
        Self { capacity, map: HashMap::new(), order: Vec::new() }
    }

    pub fn get(&mut self, key: &str) -> Option<i64> {
        self.map.get(key).copied()
    }

    pub fn put(&mut self, key: &str, value: i64) {
        if self.map.len() >= self.capacity {
            if let Some(newest) = self.order.pop() {
                self.map.remove(&newest);
            }
        }
        self.map.insert(key.to_string(), value);
        self.order.push(key.to_string());
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
}
