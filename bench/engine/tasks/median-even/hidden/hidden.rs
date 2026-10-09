use bench_task::median;
#[test] fn even() { assert_eq!(median(&[10.0, 2.0]), 6.0); }
#[test] fn odd() { assert_eq!(median(&[5.0]), 5.0); assert_eq!(median(&[9.0, 1.0, 5.0]), 5.0); }
