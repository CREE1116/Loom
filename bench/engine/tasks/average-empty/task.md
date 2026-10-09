`average` panics on an empty slice. Change it to return `Option<f64>` (None for an empty slice) and update every caller in the crate so it builds and `report` prints "n/a" when there are no values.
