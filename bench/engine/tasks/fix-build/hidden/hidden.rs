use bench_task::{most_common, word_counts};
#[test] fn counts() { let c = word_counts("a b a"); assert_eq!(c["a"], 2); assert_eq!(c["b"], 1); }
#[test] fn most() { assert_eq!(most_common("x y y z"), Some("y".to_string())); assert_eq!(most_common(""), None); }
