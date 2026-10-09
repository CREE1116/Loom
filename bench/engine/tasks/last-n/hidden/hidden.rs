use bench_task::last_n;
#[test] fn basic() { assert_eq!(last_n(&[1, 2, 3, 4], 2), vec![3, 4]); }
#[test] fn larger_than_len() { assert_eq!(last_n(&[1, 2], 5), vec![1, 2]); }
#[test] fn zero() { assert!(last_n(&[1, 2], 0).is_empty()); }
