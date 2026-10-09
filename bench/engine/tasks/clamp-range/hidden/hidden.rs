use bench_task::clamp_to;
#[test] fn inside() { assert_eq!(clamp_to(5, 0, 10), 5); }
#[test] fn below_above() { assert_eq!(clamp_to(-3, 0, 10), 0); assert_eq!(clamp_to(30, 0, 10), 10); }
#[test] fn swapped() { assert_eq!(clamp_to(30, 10, 0), 10); assert_eq!(clamp_to(-1, 10, 0), 0); }
