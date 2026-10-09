use std::collections::HashMap;

/// Count how often each word occurs.
pub fn word_counts(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word).or_insert(0) += 1;
    }
    counts
}

/// The most frequent word, if any.
pub fn most_common(text: &str) -> Option<String> {
    let counts = word_counts(text);
    counts.into_iter().max_by_key(|(w, c)| (*c, w.clone())).map(|(w, _)| w)
}
