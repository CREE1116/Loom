use bench_task::{bucket::Bucket, clock::Clock};
use std::{cell::Cell, rc::Rc};
#[derive(Clone)] struct Fake(Rc<Cell<u64>>);
impl Clock for Fake { fn now_ms(&self) -> u64 { self.0.get() } }
fn setup(cap: u64, rate: u64) -> (Fake, Bucket<Fake>) { let f = Fake(Rc::new(Cell::new(0))); (f.clone(), Bucket::new(f, cap, rate)) }
#[test] fn starts_full() { let (_, mut b) = setup(3, 1); assert!(b.try_take(3)); assert!(!b.try_take(1)); }
#[test] fn fractional_refill() { let (c, mut b) = setup(1, 2); assert!(b.try_take(1)); for _ in 0..4 { c.0.set(c.0.get() + 500); assert!(b.try_take(1), "a token every 500ms at 2/s"); } }
#[test] fn accumulates_remainders() { let (c, mut b) = setup(5, 1); assert!(b.try_take(5)); for _ in 0..3 { c.0.set(c.0.get() + 300); assert!(!b.try_take(1)); } c.0.set(c.0.get() + 300); assert!(b.try_take(1), "1.2s of 300ms steps earns one token"); }
#[test] fn capped_at_capacity() { let (c, mut b) = setup(2, 1); assert!(b.try_take(2)); c.0.set(100_000); assert!(b.try_take(2)); assert!(!b.try_take(1)); }
