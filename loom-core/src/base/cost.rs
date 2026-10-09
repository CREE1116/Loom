use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Phase {
    Orient,
    Edit,
    Validate,
    Message,
}

/// Tokens are measured by the model adapter; an unknown price stays unknown.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub usd: Option<f64>,
    pub ms: u64,
}

impl Cost {
    pub fn accumulate(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.cached = self.cached.saturating_add(other.cached);
        self.output = self.output.saturating_add(other.output);
        self.ms = self.ms.saturating_add(other.ms);
        self.usd = self.usd.zip(other.usd).map(|(a, b)| a + b);
    }
}

/// A pure phase ledger. UI event classification belongs in the surface adapter.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PhaseLedger {
    pub costs: BTreeMap<Phase, Cost>,
}

impl PhaseLedger {
    pub fn record(&mut self, phase: Phase, cost: Cost) {
        match self.costs.get_mut(&phase) {
            Some(total) => total.accumulate(&cost),
            None => {
                self.costs.insert(phase, cost);
            }
        }
    }
    pub fn total(&self) -> Cost {
        self.costs.values().fold(
            Cost {
                usd: Some(0.0),
                ..Cost::default()
            },
            |mut total, cost| {
                total.accumulate(cost);
                total
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_and_unknown_costs_are_not_conflated() {
        let mut ledger = PhaseLedger::default();
        ledger.record(
            Phase::Orient,
            Cost {
                input: 10,
                usd: Some(0.0),
                ..Cost::default()
            },
        );
        ledger.record(
            Phase::Orient,
            Cost {
                input: 20,
                usd: Some(0.0),
                ..Cost::default()
            },
        );
        assert_eq!(ledger.total().input, 30);
        assert_eq!(ledger.total().usd, Some(0.0));
        ledger.record(Phase::Validate, Cost::default());
        assert_eq!(ledger.total().usd, None);
    }
}
