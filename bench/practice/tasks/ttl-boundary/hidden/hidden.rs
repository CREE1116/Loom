use bench_task::{cache::Cache, clock::ManualClock};

#[test]
fn zero_ttl_is_immediately_expired() {
    let clock = ManualClock::default();
    clock.set(100);
    let mut cache = Cache::new(clock);
    cache.insert("key", "value", 0);
    assert_eq!(cache.get("key"), None);
}

#[test]
fn deadlines_are_relative_to_insertion_and_independent() {
    let clock = ManualClock::default();
    clock.set(123);
    let mut cache = Cache::new(clock.clone());
    cache.insert("a", "first", 7);
    cache.insert("b", "second", 19);
    clock.set(129);
    assert_eq!(cache.get("a"), Some("first"));
    clock.set(130);
    assert_eq!(cache.get("a"), None);
    assert_eq!(cache.get("b"), Some("second"));
    clock.set(142);
    assert_eq!(cache.get("b"), None);
    assert_eq!(cache.get("missing"), None);
}

#[test]
fn refreshing_a_key_replaces_its_value_and_deadline() {
    let clock = ManualClock::default();
    let mut cache = Cache::new(clock.clone());
    cache.insert("key", "old", 10);
    clock.set(8);
    cache.insert("key", "new", 20);
    clock.set(10);
    assert_eq!(cache.get("key"), Some("new"));
    clock.set(28);
    assert_eq!(cache.get("key"), None);
}
