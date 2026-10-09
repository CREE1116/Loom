/// The last `n` items of `items`, or all items if there are fewer than `n`.
pub fn last_n<T: Clone>(items: &[T], n: usize) -> Vec<T> {
    let start = items.len().saturating_sub(n) + 1;
    items.get(start..).unwrap_or(&[]).to_vec()
}
