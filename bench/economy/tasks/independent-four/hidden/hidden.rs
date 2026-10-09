use economy_fixture::{limits::clamp_score,names::slugify};
#[test] fn limits_contract() { for (a,b) in [(i64::MIN,0),(-1,0),(0,0),(17,17),(100,100),(101,100),(i64::MAX,100)] {assert_eq!(clamp_score(a),b);} }
#[test] fn names_contract() {for (a,b) in [(" Hello, WORLD! ","hello-world"),("a__--b","a-b"),("",""),("中文",""),("Éa42ßB","a42-b"),("one.two/three","one-two-three")] {assert_eq!(slugify(a),b);} }
use economy_fixture::{stats::median_sorted,duration::parse_duration_ms};
#[test] fn stats_contract() {assert_eq!(median_sorted(&[]),None);assert_eq!(median_sorted(&[1.]),Some(1.));assert_eq!(median_sorted(&[1.,3.]),Some(2.));assert_eq!(median_sorted(&[1.,2.,5.]),Some(2.));assert_eq!(median_sorted(&[-2.,-1.,1.,2.]),Some(0.));}
#[test] fn duration_contract() {for (s,v) in [("0ms",Some(0)),(" 12s ",Some(12000)),("2m",Some(120000)),("10ms",Some(10)),("18446744073709551615ms",Some(u64::MAX)),("18446744073709551615s",None),("1.5s",None),("-1s",None),("4",None),("s",None),("1ss",None),("1 s",None)] {assert_eq!(parse_duration_ms(s),v,"{s}");}}
