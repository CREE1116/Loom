use economy_fixture::{limits::clamp_score,names::slugify};
#[test] fn limits_contract() { for (a,b) in [(i64::MIN,0),(-1,0),(0,0),(17,17),(100,100),(101,100),(i64::MAX,100)] {assert_eq!(clamp_score(a),b);} }
#[test] fn names_contract() {for (a,b) in [(" Hello, WORLD! ","hello-world"),("a__--b","a-b"),("",""),("中文",""),("Éa42ßB","a42-b"),("one.two/three","one-two-three")] {assert_eq!(slugify(a),b);} }
