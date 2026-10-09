/// Clamp `value` into [lo, hi].
pub fn clamp_to(value: i64, lo: i64, hi: i64) -> i64 {
    if value < lo {
        hi
    } else if value > hi {
        lo
    } else {
        value
    }
}
