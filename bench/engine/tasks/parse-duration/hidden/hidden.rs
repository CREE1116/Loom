use bench_task::parse_duration;
#[test] fn hours_minutes() { assert_eq!(parse_duration("1h30m"), Some(5400)); }
#[test] fn minutes() { assert_eq!(parse_duration("45m"), Some(2700)); }
#[test] fn seconds() { assert_eq!(parse_duration("90s"), Some(90)); }
#[test] fn all() { assert_eq!(parse_duration("2h1m5s"), Some(7265)); }
#[test] fn invalid() { assert_eq!(parse_duration("5x"), None); assert_eq!(parse_duration(""), None); assert_eq!(parse_duration("12"), None); }
