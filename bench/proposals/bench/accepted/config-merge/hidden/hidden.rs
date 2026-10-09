use bench_task::{merge::merge, val::Val};
use std::collections::BTreeMap;
fn m(items: Vec<(&str, Val)>) -> Val { Val::Map(items.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<BTreeMap<_, _>>()) }
#[test] fn nested_maps_merge() { let b = m(vec![("db", m(vec![("host", Val::Str("a".into())), ("port", Val::Num(1))]))]); let o = m(vec![("db", m(vec![("port", Val::Num(2))]))]); assert_eq!(merge(b, o), m(vec![("db", m(vec![("host", Val::Str("a".into())), ("port", Val::Num(2))]))])); }
#[test] fn lists_replace() { let b = m(vec![("xs", Val::List(vec![Val::Num(1), Val::Num(2)]))]); let o = m(vec![("xs", Val::List(vec![Val::Num(3)]))]); assert_eq!(merge(b, o), m(vec![("xs", Val::List(vec![Val::Num(3)]))])); }
#[test] fn null_deletes() { let b = m(vec![("a", Val::Num(1)), ("b", Val::Num(2))]); let o = m(vec![("a", Val::Null)]); assert_eq!(merge(b, o), m(vec![("b", Val::Num(2))])); }
#[test] fn deep_null_deletes() { let b = m(vec![("x", m(vec![("y", Val::Num(1)), ("z", Val::Num(2))]))]); let o = m(vec![("x", m(vec![("y", Val::Null)]))]); assert_eq!(merge(b, o), m(vec![("x", m(vec![("z", Val::Num(2))]))])); }
#[test] fn scalar_replaces() { assert_eq!(merge(Val::Num(1), Val::Str("s".into())), Val::Str("s".into())); }
