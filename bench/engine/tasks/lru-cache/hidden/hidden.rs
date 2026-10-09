use bench_task::lru::LruCache;
#[test] fn evicts_least_recent() { let mut c = LruCache::new(2); c.put("a", 1); c.put("b", 2); c.put("c", 3); assert_eq!(c.get("a"), None); assert_eq!(c.get("b"), Some(2)); assert_eq!(c.get("c"), Some(3)); }
#[test] fn get_is_a_use() { let mut c = LruCache::new(2); c.put("a", 1); c.put("b", 2); assert_eq!(c.get("a"), Some(1)); c.put("c", 3); assert_eq!(c.get("b"), None); assert_eq!(c.get("a"), Some(1)); }
#[test] fn update_does_not_evict() { let mut c = LruCache::new(2); c.put("a", 1); c.put("b", 2); c.put("a", 10); assert_eq!(c.len(), 2); assert_eq!(c.get("a"), Some(10)); assert_eq!(c.get("b"), Some(2)); }
#[test] fn zero_capacity() { let mut c = LruCache::new(0); c.put("a", 1); assert_eq!(c.len(), 0); assert_eq!(c.get("a"), None); }
