/// Median of a non-empty slice.
pub fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    sorted[sorted.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn odd_length() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
    }
    #[test]
    fn even_length_averages_the_middle_pair() {
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
    }
}
