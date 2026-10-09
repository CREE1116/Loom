use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Null,
    Num(i64),
    Str(String),
    List(Vec<Val>),
    Map(BTreeMap<String, Val>),
}
