use crate::average;

/// One-line summary like "3 values, mean 2.00".
pub fn report(values: &[f64]) -> String {
    format!("{} values, mean {:.2}", values.len(), average(values))
}
