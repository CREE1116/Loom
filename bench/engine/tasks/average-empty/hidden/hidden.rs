use bench_task::{average, report::report};
#[test] fn some() { assert_eq!(average(&[1.0, 2.0, 3.0]), Some(2.0)); }
#[test] fn none() { assert_eq!(average(&[]), None); }
#[test] fn reports() { assert_eq!(report(&[1.0, 3.0]), "2 values, mean 2.00"); assert_eq!(report(&[]), "0 values, mean n/a"); }
