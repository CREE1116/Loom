use bench_task::top_words;
#[test] fn normalized() { assert_eq!(top_words("Hello, hello! World.", 1), vec![("hello".to_string(), 2)]); }
#[test] fn ties() { assert_eq!(top_words("b a", 2), vec![("a".to_string(), 1), ("b".to_string(), 1)]); }
#[test] fn inner_apostrophe_kept() { assert_eq!(top_words("don't Don't", 1), vec![("don't".to_string(), 2)]); }
