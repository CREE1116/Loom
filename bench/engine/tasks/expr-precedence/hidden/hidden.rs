use bench_task::eval;
#[test] fn precedence() { assert_eq!(eval("2+3*4"), Ok(14.0)); assert_eq!(eval("10-4/2"), Ok(8.0)); assert_eq!(eval("2*3+4*5"), Ok(26.0)); }
#[test] fn left_assoc() { assert_eq!(eval("10-3-2"), Ok(5.0)); assert_eq!(eval("16/4/2"), Ok(2.0)); }
#[test] fn parens() { assert_eq!(eval("(2+3)*4"), Ok(20.0)); }
#[test] fn unary() { assert_eq!(eval("-3"), Ok(-3.0)); assert_eq!(eval("2*-(1+1)"), Ok(-4.0)); assert_eq!(eval("--2"), Ok(2.0)); }
#[test] fn errors() { assert!(eval("1/0").is_err()); assert!(eval("(1+2").is_err()); assert!(eval("1+").is_err()); }
