pub mod report;

/// Arithmetic mean.
pub fn average(values: &[f64]) -> f64 {
    let sum: f64 = values.iter().sum();
    if values.is_empty() {
        panic!("average of nothing");
    }
    sum / values.len() as f64
}
