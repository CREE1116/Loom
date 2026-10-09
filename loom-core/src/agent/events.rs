use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Direct,
    Planning,
    Orchestrating,
    Navigation,
    Source,
    Change,
    Run,
    AgentStarted,
    AgentFinished,
    Merge,
    Check,
    Approval,
    Apply,
    Error,
    Answer,
}

/// Domain events contain facts. Formatting, folding and language belong to the surface.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub worker: String,
    pub stage: Stage,
    pub target: String,
    pub detail: Value,
    pub failed: bool,
}
pub type Observer<'a> = dyn Fn(Event) + Send + Sync + 'a;
impl Event {
    pub fn new(worker: &str, stage: Stage, target: impl Into<String>, detail: Value) -> Self {
        Self {
            worker: worker.into(),
            stage,
            target: target.into(),
            detail,
            failed: false,
        }
    }
}
