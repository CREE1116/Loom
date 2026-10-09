use crate::val::Val;

/// Merge `overlay` onto `base` (configuration layering).
pub fn merge(base: Val, overlay: Val) -> Val {
    match (base, overlay) {
        (Val::Map(mut b), Val::Map(o)) => {
            for (k, v) in o {
                b.insert(k, v);
            }
            Val::Map(b)
        }
        (Val::List(mut b), Val::List(o)) => {
            b.extend(o);
            Val::List(b)
        }
        (_, o) => o,
    }
}
