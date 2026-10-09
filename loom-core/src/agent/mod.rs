//! V5 owns session lifecycle; World/query/worker/orchestration have independent responsibilities.
mod control;
mod conversation;
pub mod events;
pub mod orchestration;
pub mod query;
mod tools;
mod worker;
pub mod world;
pub use control::{Core, Intent, Status, Task};
use crate::{
    base::Revision,
    effects::ExecutionPolicy,
    models::{Message, Usage},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct LiveView {
    pub task_id: String,
    pub revision: Revision,
    pub goal: String,
    pub stage: String,
}
#[derive(Clone, Default)]
pub struct InputControl {
    answers: Arc<Mutex<BTreeMap<String, String>>>,
    cancelled: Arc<Mutex<std::collections::BTreeSet<String>>>,
    live: Arc<Mutex<Option<LiveView>>>,
    approval_policy: ApprovalPolicy,
}

/// Approval for applying the merged, validated proposal to the real workspace.
/// Both prompting modes require explicit acceptance of that patch. Commands in
/// the isolated copy remain subject to the independent execution sandbox.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    #[default]
    OnRequest,
    Untrusted,
    Never,
}
impl ApprovalPolicy {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "on-request" => Ok(Self::OnRequest),
            "untrusted" => Ok(Self::Untrusted),
            "never" => Ok(Self::Never),
            _ => anyhow::bail!("unknown approval policy: {value}"),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnRequest => "on-request",
            Self::Untrusted => "untrusted",
            Self::Never => "never",
        }
    }
}
impl InputControl {
    pub fn approval_policy(&self) -> ApprovalPolicy {
        self.approval_policy
    }
    /// A cloned control snapshots this choice for its task; changing the next
    /// task's choice cannot silently grant approval to an already running task.
    pub fn set_approval_policy(&mut self, policy: ApprovalPolicy) {
        self.approval_policy = policy;
    }
    pub fn snapshot(&self) -> Option<LiveView> {
        self.live.lock().unwrap().clone()
    }
    pub(crate) fn begin(&self, revision: Revision, goal: &str, task_id: &str) {
        *self.live.lock().unwrap() = Some(LiveView {
            task_id: task_id.into(),
            revision,
            goal: goal.into(),
            stage: "preparing".into(),
        });
    }
    pub(crate) fn publish(&self, revision: Revision, goal: &str, stage: &str) {
        let mut live = self.live.lock().unwrap();
        let task_id = live
            .as_ref()
            .map(|view| view.task_id.clone())
            .unwrap_or_default();
        *live = Some(LiveView {
            task_id,
            revision,
            goal: goal.into(),
            stage: stage.into(),
        });
    }
    pub(crate) fn observe(&self, event: &events::Event) {
        if let Some(live) = self.live.lock().unwrap().as_mut() {
            if event.stage == events::Stage::Merge {
                if let Ok(revision) = serde_json::from_value(event.detail["revision"].clone()) {
                    live.revision = revision;
                }
            }
            if event.worker == "main" || event.stage == events::Stage::Merge {
                live.stage = format!("{:?}: {}", event.stage, event.target);
            }
        }
    }
    pub fn answer(&self, id: String, text: String) {
        self.answers.lock().unwrap().insert(id, text);
    }
    pub fn cancel(&self, id: String) {
        self.cancelled.lock().unwrap().insert(id);
    }
    pub(crate) fn ask(
        &self,
        id: &str,
        question: &str,
        shared: &worker::Shared<'_>,
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(!question.trim().is_empty(), "question required");
        let request_id = format!(
            "{}-{}-question-{}",
            shared.run_id,
            id,
            shared.metrics().calls
        );
        shared.emit(events::Event::new(
            id,
            events::Stage::Approval,
            question,
            serde_json::json!({"input_request":{"id":request_id,"question":question}}),
        ));
        while !shared.stopped() {
            if self.cancelled.lock().unwrap().remove(&request_id) {
                return Ok(serde_json::json!({"cancelled":true,"reason":"user cancelled the question; no answer was chosen"}));
            }
            if let Some(answer) = self.answers.lock().unwrap().remove(&request_id) {
                return Ok(serde_json::json!({"answer":answer}));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!("question cancelled or timed out")
    }
}
#[derive(Clone)]
pub struct Options {
    pub history: Vec<Message>,
    pub pinned: Option<Revision>,
    pub inquiry: bool,
    pub read_only: bool,
    pub plan: bool,
    pub checks: Vec<String>,
    pub steps: usize,
    pub seconds: u64,
    pub tokens: u64,
    pub call_seconds: u64,
    /// Total attempts for one logical model request, including the initial call.
    pub model_attempts: usize,
    pub context_bytes: usize,
    pub validation_reserve: u64,
    pub parallelism: usize,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub policy: ExecutionPolicy,
    pub auto_apply: bool,
    pub control: InputControl,
    /// SKILL.md files the user attached to this request.
    pub skills: Vec<std::path::PathBuf>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            history: vec![],
            pinned: None,
            inquiry: false,
            read_only: false,
            plan: false,
            checks: vec![],
            steps: 32,
            seconds: 300,
            tokens: 160000,
            call_seconds: 60,
            model_attempts: 3,
            context_bytes: 96000,
            validation_reserve: 30,
            parallelism: 4,
            model: None,
            reasoning_effort: None,
            policy: Default::default(),
            auto_apply: false,
            control: Default::default(),
            skills: vec![],
        }
    }
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct IndexStats {
    pub files: usize,
    pub symbols: usize,
    pub reused: bool,
}
pub fn index_stats(root: &std::path::Path) -> anyhow::Result<IndexStats> {
    let hub = world::WorldHub::default();
    let view = hub.view(root)?;
    Ok(IndexStats {
        files: view.world.snapshot.files.entries().count(),
        symbols: view.world.graph.len(),
        reused: hub.parses() == 0,
    })
}
// Usage remains optional when a provider omits observations.
pub type ObservedUsage = Usage;
