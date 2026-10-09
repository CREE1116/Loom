//! Compatibility DTOs for saved activity traces; no legacy execution runtime.
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelErrorKind { Cancelled, TimedOut, Auth, Quota, RateLimited, Transient, BadRequest }


#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    Started,
    Commentary,
    ModelRequest,
    ModelResponse,
    GraphObserved,
    ToolStarted,
    ToolSucceeded,
    ToolFailed,
    Stopped,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub worker: String,
    pub kind: EventKind,
    pub tool: Option<String>,
    pub path: Option<String>,
    pub text: String,
    pub detail: String,
}
impl Event {
    pub fn correction(stage: &str, attempt: usize, error: &str) -> Self {
        let mut event = Self::new("coordinator", EventKind::Commentary, format!("{stage} 형식을 수정 중입니다."));
        event.detail = serde_json::json!({"kind":"Correction","stage":stage,"attempt":attempt,"error":error}).to_string();
        event
    }
    pub fn graph(worker: &str, evidence: GraphEvidence) -> Self {
        let mut event = Self::new(worker, EventKind::GraphObserved, "AST evidence");
        event.detail = serde_json::to_string(&evidence).unwrap();
        event
    }
    pub fn new(worker: &str, kind: EventKind, text: impl Into<String>) -> Self {
        Self {
            worker: worker.into(),
            kind,
            tool: None,
            path: None,
            text: text.into(),
            detail: String::new(),
        }
    }
    // Stable typed transport through the existing progress callback. The UI
    // never parses English diagnostic prose to infer tool identity or status.
    pub fn wire(&self) -> String {
        format!("@loom-event {}", serde_json::to_string(self).unwrap())
    }
    pub fn decode(text: &str) -> Option<Self> {
        serde_json::from_str(text.strip_prefix("@loom-event ")?).ok()
    }
}

#[derive(Deserialize)]
pub struct Correction {
    kind: String,
    pub stage: String,
    pub attempt: usize,
    pub error: String,
}
impl Correction {
    pub fn decode(event: &Event) -> Option<Self> {
        if event.kind != EventKind::Commentary {return None;}
        let value: Self = serde_json::from_str(&event.detail).ok()?;
        (value.kind == "Correction").then_some(value)
    }
}

#[derive(Clone,Debug,Serialize,Deserialize)]
pub struct ModelIssue {
    kind:String,
    pub category:Option<ModelErrorKind>,
    pub retrying:bool,
    pub attempt:usize,
    pub delay_ms:u64,
    pub error:String,
}
impl ModelIssue {
    pub fn event(worker:&str,category:Option<ModelErrorKind>,retrying:bool,attempt:usize,delay:std::time::Duration,error:&str)->Event {
        let issue=Self {kind:"ModelIssue".into(),category,retrying,attempt,delay_ms:delay.as_millis() as u64,error:error.into()};
        let mut event=Event::new(worker,EventKind::Commentary,if retrying {"모델 서버가 잠시 응답하지 않아 다시 연결하고 있습니다."} else {"모델 응답을 받지 못해 작업을 멈췄습니다."});
        event.detail=serde_json::to_string(&issue).unwrap();event
    }
    pub fn decode(event:&Event)->Option<Self> {
        if event.kind!=EventKind::Commentary {return None;}
        let value:Self=serde_json::from_str(&event.detail).ok()?;
        (value.kind=="ModelIssue").then_some(value)
    }
}

/// Observations from Core, never inferred from model commentary or tool prose.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum GraphEvidence {
    Index { files: usize, symbols: usize, hashed: usize, reused: usize, graph_reused: bool },
    Context { bytes: usize, symbol_count: usize, symbols: Vec<ContextSymbol>, truncated: bool },
    Plan { dependency_count: usize, dependencies: Vec<(String, String)>, read_count: usize, reads: Vec<(String, String)> },
    Skipped { reason: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextSymbol {
    pub name: String,
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub role: String,
    pub bytes: usize,
    pub partial: bool,
}
impl GraphEvidence {
    pub fn decode(event: &Event) -> Option<Self> {
        if event.kind != EventKind::GraphObserved { return None; }
        serde_json::from_str(&event.detail).ok()
    }
}
