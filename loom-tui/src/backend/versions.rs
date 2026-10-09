//! Asynchronous UI projection for Loom's native Core.
use super::version_core::{self as core, Intent, Outcome, Progress, Settings, Version};
use crate::agent::*;
use anyhow::{Context, Result, ensure};
use loom_core::models::{self, ModelPort};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Keeps requests submitted during the initial provider scan alive without blocking
/// terminal creation. Only the worker performing the model call waits here.
#[derive(Default)]
struct LoadingModel {
    result: Mutex<Option<Result<Arc<dyn ModelPort>, String>>>,
    ready: Condvar,
}

impl LoadingModel {
    fn resolve(&self, result: Result<Arc<dyn ModelPort>, String>) {
        if let Ok(mut guard) = self.result.lock() {
            *guard = Some(result);
            self.ready.notify_all();
        }
    }

    fn wait_for_model(&self, request: &models::Request) -> Result<Arc<dyn ModelPort>, models::ProviderError> {
        use models::{ErrorKind, ProviderError};
        let start = Instant::now();
        let mut state = self.result.lock().map_err(|_| ProviderError::new(ErrorKind::Transient, "model loader lock poisoned"))?;
        loop {
            if request.cancel.as_ref().is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
                return Err(ProviderError::new(ErrorKind::Cancelled, "model request cancelled"));
            }
            if let Some(result) = state.as_ref() {
                return result.clone().map_err(|error| ProviderError::new(ErrorKind::Transient, error));
            }
            if request.timeout.is_some_and(|timeout| start.elapsed() >= timeout) {
                return Err(ProviderError::new(ErrorKind::TimedOut, "model discovery timed out"));
            }
            let wait = request.timeout.map(|timeout| timeout.saturating_sub(start.elapsed()).min(Duration::from_millis(100)))
                .unwrap_or(Duration::from_millis(100));
            state = self.ready.wait_timeout(state, wait)
                .map_err(|_| ProviderError::new(ErrorKind::Transient, "model loader wait failed"))?.0;
        }
    }
}

impl ModelPort for LoadingModel {
    fn complete(&self, need: models::Need, request: &models::Request) -> Result<models::Served, models::ProviderError> {
        self.wait_for_model(request)?.complete(need, request)
    }

    fn complete_stream(&self, need: models::Need, request: &models::Request, observe: &mut dyn FnMut(models::StreamEvent)) -> Result<models::Served, models::ProviderError> {
        self.wait_for_model(request)?.complete_stream(need, request, observe)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StoredSession {
    pub id: String,
    pub title: String,
    pub updated_at: u64,
    pub task_ids: Vec<String>,
    /// `/compact`: a model-written summary standing in for the first `compacted` turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_summary: Option<String>,
    #[serde(default)]
    pub compacted: usize,
    /// Live turn id → Core task id (restored turns use the task id itself).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub turn_tasks: std::collections::BTreeMap<String, String>,
    /// Task id → repository revision once that turn (and any approved change) finished,
    /// the point "rewind conversation and code" restores.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub checkpoints: std::collections::BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredApprovalPolicy {
    approval_policy: String,
}

enum Message {
    ModelsReady { generation: u64, result: Result<Arc<dyn ModelPort>, String> },
    ModelSources { generation: u64, sources: Vec<models::discover::Source> },
    IndexReady(Result<(usize, usize, bool), String>),
    Progress(Progress),
    Done(Result<Outcome, String>),
    Approved(Result<Outcome, String>),
    InquiryDone {
        id: String,
        result: Result<loom_core::agent::Task, String>,
    },
    Compacted {
        session: String,
        through: usize,
        result: Result<String, String>,
    },
    /// The repository revision after a task finished or its change was applied.
    Checkpoint {
        session: String,
        task: String,
        revision: String,
    },
}

fn send_checkpoint(tx: &mpsc::Sender<Message>, root: &Path, session: &str, task: &str) {
    if let Ok(revision) = loom_core::base::Revision::capture(root) {
        let _ = tx.send(Message::Checkpoint {
            session: session.to_owned(),
            task: task.to_owned(),
            revision: revision.as_str().to_owned(),
        });
    }
}

const COMPACT_REQUEST: &str = "Summarize the conversation so far so the work can continue without it. Keep: the user's goals and preferences, decisions made, files and symbols changed or examined (with paths), commands and their results that matter, and unresolved problems or next steps. Write in the user's language. Be concise; no preamble.";

/// The summary of turns folded by `/compact`, as the opening exchange of the history.
fn compacted_history(summary: &str) -> Vec<models::Message> {
    vec![
        models::Message::user("Summary of the earlier conversation (compacted):"),
        models::Message {
            role: models::Role::Assistant,
            parts: vec![models::Part::Text(summary.to_owned())],
        },
    ]
}

pub(crate) fn display_progress_summary(operation: &str, summary: &str) -> String {
    let prefix = format!("{operation} · ");
    let localized = summary.strip_prefix(&prefix).unwrap_or(summary);
    if localized == "Result: edit complete" {
        return "결과: 수정 완료".into();
    }
    if localized == "Result: file created" {
        return "결과: 파일 생성".into();
    }
    if localized == "Result: no matching code" {
        return "결과: 일치하는 코드 없음".into();
    }
    if localized == "Result: delivered" {
        return "결과: 전달 완료".into();
    }
    if localized == "Reused previous evidence" {
        return "이전 근거 재사용".into();
    }
    if let Some(count) = localized
        .strip_prefix("Result: read ")
        .and_then(|rest| rest.strip_suffix(" lines"))
    {
        return format!("결과: {count}줄 읽음");
    }
    if operation == "map"
        && let Some((files, symbols)) = localized.split_once(" files · ")
        && let Some(symbols) = symbols.strip_suffix(" symbols")
    {
        return format!("파일 {files}개 · 심볼 {symbols}개");
    }
    match operation {
        "ast" if localized == "Preparing code exploration with AST index" => {
            "AST 인덱스로 코드 탐색 준비".into()
        }
        "apply" if localized == "Applying validated changes" => "검증된 변경 적용".into(),
        "map" => localized.replacen("Repository structure", "저장소 구조 확인", 1),
        "symbols" => localized.replacen("AST symbol search", "AST 심볼 검색", 1),
        "symbol_context" => localized.replacen("AST context", "AST 문맥", 1),
        "inspect_symbol" => localized.replacen("AST inspect", "AST 심볼 확인", 1),
        "file_context" => localized.replacen("File graph", "파일 구조", 1),
        "symbol_impact" => localized.replacen("AST impact", "AST 영향 범위", 1),
        "refs" => localized.replacen("AST references", "AST 참조", 1),
        "git_show" => localized.replacen("Git show", "Git 변경 이력", 1),
        "git_blame" => localized.replacen("Git blame", "Git 줄 이력", 1),
        "diagnostics" => localized.replacen("Fast diagnostics", "빠른 진단", 1),
        "read_many" => {
            if let Some(count) = localized
                .strip_prefix("Batch code read · ")
                .and_then(|rest| rest.strip_suffix(" items"))
            {
                format!("코드 일괄 읽기 · {count}개")
            } else {
                summary.to_owned()
            }
        }
        "check" => localized.replacen("Build and tests", "빌드·테스트", 1),
        "finish" if localized == "Deliver result" => "작업 결과 전달".into(),
        "progress" if localized == "Work in progress" => "작업 진행 중".into(),
        "closing" => "현재 근거로 수정·검증 또는 미완료 설명을 마무리합니다.".into(),
        "waiting" if localized.starts_with("No model response was received") => {
            "모델 응답을 기다리고 있습니다.".into()
        }
        "tool_recovery" => "도구 호출을 교정하고 있습니다.".into(),
        "recovery" => "확보한 근거로 작업을 정리하고 있습니다.".into(),
        _ => summary.to_owned(),
    }
}

fn progress_operation_id(detail: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(detail).ok()?;
    value["operation_id"]
        .as_str()
        .or_else(|| value["tool_started"]["operation_id"].as_str())
        .map(ToOwned::to_owned)
}

fn progress_is_started(detail: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(detail)
        .ok()
        .is_some_and(|value| value.get("tool_started").is_some())
}

pub(crate) fn progress_entry_body(operation: &str, display_summary: &str, detail: &str) -> String {
    if operation == "finish" {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(detail) else {
        return detail.to_owned();
    };
    let payload = value.get("tool_started").unwrap_or(&value);
    let mut lines = Vec::new();
    if let Some(args) = payload["args"].as_object().filter(|args| !args.is_empty()) {
        lines.push("입력".to_owned());
        for (key, value) in args {
            let value = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            lines.push(format!("{key}: {value}"));
        }
    }
    if value.get("tool_started").is_some() {
        if lines.is_empty() {
            lines.push(format!("실행 중 · {display_summary}"));
        }
        return lines.join("\n");
    }
    if let Some(result_summary) = value["result_summary"].as_str() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!(
            "결과 · {}",
            display_progress_summary(operation, result_summary)
        ));
    }
    if let Some(result) = value["result"].as_str().filter(|result| !result.is_empty()) {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.push(result.to_owned());
    }
    if lines.is_empty() {
        display_summary.to_owned()
    } else {
        lines.join("\n")
    }
}

#[cfg(test)]
mod progress_projection_tests {
    use super::{
        display_progress_summary, progress_entry_body, progress_is_started, progress_operation_id,
    };

    #[test]
    fn internal_english_progress_is_localized_only_at_the_tui_boundary() {
        assert_eq!(
            display_progress_summary(
                "symbol_context",
                "symbol_context · AST context · src/lib.rs::value"
            ),
            "AST 문맥 · src/lib.rs::value"
        );
        assert_eq!(
            display_progress_summary("refs", "refs · AST references · src/lib.rs::value"),
            "AST 참조 · src/lib.rs::value"
        );
        assert_eq!(
            display_progress_summary("read_many", "read_many · Batch code read · 2 items"),
            "코드 일괄 읽기 · 2개"
        );
        assert_eq!(
            display_progress_summary("map", "map · 1 files · 2 symbols"),
            "파일 1개 · 심볼 2개"
        );
        assert_eq!(
            display_progress_summary(
                "inspect_symbol",
                "inspect_symbol · AST inspect · src/lib.rs::value"
            ),
            "AST 심볼 확인 · src/lib.rs::value"
        );
        assert_eq!(
            display_progress_summary("file_context", "file_context · File graph · src/lib.rs"),
            "파일 구조 · src/lib.rs"
        );
        assert_eq!(
            display_progress_summary(
                "symbol_impact",
                "symbol_impact · AST impact · src/lib.rs::value"
            ),
            "AST 영향 범위 · src/lib.rs::value"
        );
        assert_eq!(
            display_progress_summary("diagnostics", "diagnostics · Fast diagnostics · project"),
            "빠른 진단 · project"
        );
    }

    #[test]
    fn v4_tool_progress_keeps_inputs_and_results_instead_of_raw_event_envelopes() {
        let started = r#"{"tool_started":{"operation_id":"op-1","step":1,"tool":"map","description":"Repository structure","args":{"path":"."}}}"#;
        assert_eq!(progress_operation_id(started).as_deref(), Some("op-1"));
        assert!(progress_is_started(started));
        let body = progress_entry_body("map", "저장소 구조 확인", started);
        assert!(body.contains("path: ."), "{body}");
        assert!(!body.contains("tool_started"), "{body}");

        let completed = r#"{"operation_id":"op-1","step":1,"tool":"map","description":"Repository structure","result_summary":"4 files · 12 symbols","args":{"path":"."},"error":false,"result":"map of the repository · 4 files · 12 symbols"}"#;
        assert_eq!(progress_operation_id(completed).as_deref(), Some("op-1"));
        assert!(!progress_is_started(completed));
        let body = progress_entry_body("map", "저장소 구조 확인 · 4 files · 12 symbols", completed);
        assert!(body.contains("path: ."), "{body}");
        assert!(body.contains("4 files · 12 symbols"), "{body}");
        assert!(body.contains("map of the repository"), "{body}");
        assert!(!body.contains("operation_id"), "{body}");
    }

    #[test]
    fn finish_progress_does_not_echo_the_final_answer_as_tool_detail() {
        let detail = r#"{"operation_id":"op-finish","tool":"finish","args":{"answer":true,"summary":"final answer text"},"result_summary":"Result: delivered","result":"delivered"}"#;
        assert_eq!(
            progress_entry_body("finish", "작업 결과 전달", detail),
            "",
            "finish is a terminal control signal; its summary is rendered as the assistant answer"
        );
    }
}

pub struct VersionBackend {
    version: Version,
    root: PathBuf,
    settings: Settings,
    model: Arc<dyn ModelPort>,
    managed_models: bool,
    loading_model: Option<Arc<LoadingModel>>,
    reload_generation: u64,
    pending_model: Option<Arc<dyn ModelPort>>,
    events: VecDeque<AgentEvent>,
    tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Message>,
    cancel: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    active: bool,
    turn: Option<String>,
    pending: Option<Outcome>,
    trusted: bool,
    sequence: u64,
    session: String,
    input: Option<u64>,
    output: Option<u64>,
    stopped: bool,
    conversation: super::version_conversation::Conversation,
    v5_projection: super::v5_projection::Projection,
    inquiry_active: bool,
    inquiry_cancel: Arc<AtomicBool>,
    inquiry_thread: Option<thread::JoinHandle<()>>,
}

impl VersionBackend {
    pub fn new(
        version: Version,
        root: &Path,
        script: Option<&Path>,
        settings: Settings,
    ) -> Result<Self> {
        Self::new_with_session(version, root, script, settings, None)
    }

    pub fn new_with_session(
        version: Version,
        root: &Path,
        script: Option<&Path>,
        settings: Settings,
        initial_session: Option<String>,
    ) -> Result<Self> {
        let Some(script) = script else {
            let loading = Arc::new(LoadingModel::default());
            let mut backend = Self::with_model_and_session(
                version, root, loading.clone(), settings, initial_session,
            )?;
            backend.managed_models = true;
            backend.loading_model = Some(loading);
            backend.reload_providers()?;
            return Ok(backend);
        };
        let backend = Self::with_model_and_session(
            version, root, loom_core::surfaces::model(Some(script))?, settings, initial_session,
        )?;
        Ok(backend)
    }

    pub fn with_model(
        version: Version,
        root: &Path,
        model: Arc<dyn ModelPort>,
        settings: Settings,
    ) -> Result<Self> {
        Self::with_model_and_session(version, root, model, settings, None)
    }

    pub fn with_model_and_session(
        version: Version,
        root: &Path,
        model: Arc<dyn ModelPort>,
        settings: Settings,
        initial_session: Option<String>,
    ) -> Result<Self> {
        let mut settings = settings;
        if settings.readonly || settings.policy.sandbox == loom_core::effects::SandboxMode::ReadOnly
        {
            settings.readonly = true;
            settings.policy.sandbox = loom_core::effects::SandboxMode::ReadOnly;
        }
        let root = root.canonicalize()?;
        Self::restore_approval_policy(&root, &mut settings)?;
        let (tx, rx) = mpsc::channel();
        let mut backend = Self {
            version,
            root,
            settings,
            model,
            managed_models: false,
            loading_model: None,
            reload_generation: 0,
            pending_model: None,
            events: VecDeque::new(),
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            thread: None,
            active: false,
            turn: None,
            pending: None,
            trusted: true,
            sequence: 0,
            session: String::new(),
            input: Some(0),
            output: Some(0),
            stopped: false,
            conversation: Default::default(),
            v5_projection: Default::default(),
            inquiry_active: false,
            inquiry_cancel: Arc::new(AtomicBool::new(false)),
            inquiry_thread: None,
        };

        let target_session = match initial_session.as_deref() {
            Some("new") => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let new_id = format!(
                    "session-{:x}-{}",
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos(),
                    std::process::id()
                );
                let s = StoredSession {
                    id: new_id.clone(),
                    title: "새 대화".into(),
                    updated_at: now,
                    task_ids: vec![],
                    ..Default::default()
                };
                backend.save_stored_session(&s);
                new_id
            }
            Some(id) => id.to_string(),
            None => {
                let existing = backend.list_stored_sessions();
                if let Some(first) = existing.first() {
                    first.id.clone()
                } else {
                    format!("loom-{}", session_key(version))
                }
            }
        };

        let pending_proposal = backend.load_session(&target_session)?;
        let root = backend.root.clone();
        let tx = backend.tx.clone();
        backend.emit(AgentUpdate::Notice("프로젝트 인덱스 확인 중…".into()));
        thread::spawn(move || {
            let result = loom_core::agent::index_stats(&root)
                .map(|stats| (stats.files, stats.symbols, stats.reused))
                .map_err(|error| error.to_string());
            let _ = tx.send(Message::IndexReady(result));
        });
        backend.refresh_models();
        backend.emit(AgentUpdate::Notice(
            "Loom · /plan 목표로 작업 분할 · /ask 병렬 질문".into(),
        ));

        // The first history read already found any pending proposal in this session.
        if let Some(proposal) = pending_proposal {
            backend.turn = Some(proposal.id.clone());
            backend.request_approval(proposal);
        }

        Ok(backend)
    }

    fn sessions_dir(&self) -> PathBuf {
        let current = self.root.join(".loom/sessions");
        let legacy = self.root.join(".loom/v5/sessions");
        if legacy.exists() && std::fs::create_dir_all(&current).is_ok() {
            if let Ok(entries) = std::fs::read_dir(&legacy) {
                for entry in entries.flatten() {
                    let target = current.join(entry.file_name());
                    if !target.exists() {
                        let _ = std::fs::rename(entry.path(), target);
                    }
                }
            }
        }
        current
    }

    fn list_stored_sessions(&self) -> Vec<StoredSession> {
        let dir = self.sessions_dir();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut list = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    if let Ok(bytes) = std::fs::read(path) {
                        if let Ok(s) = serde_json::from_slice::<StoredSession>(&bytes) {
                            list.push(s);
                        }
                    }
                }
            }
            list.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
            list
        } else {
            Vec::new()
        }
    }

    fn save_stored_session(&self, session: &StoredSession) {
        let dir = self.sessions_dir();
        if std::fs::create_dir_all(&dir).is_ok() {
            let path = dir.join(format!("{}.json", session.id));
            if let Ok(bytes) = serde_json::to_vec_pretty(session) {
                let _ = std::fs::write(path, bytes);
            }
        }
    }

    /// A new session holding this session's turns up to and including `turn`; the
    /// original stays in the session list.
    fn branch_session(&self, turn: &str) -> Result<String> {
        let stored = self.get_stored_session(&self.session).context("현재 대화를 찾을 수 없습니다")?;
        let task = stored.turn_tasks.get(turn).map(String::as_str).unwrap_or(turn);
        let index = stored.task_ids.iter().position(|id| id == task).context("되돌릴 답변을 찾을 수 없습니다")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let kept: Vec<String> = stored.task_ids[..=index].to_vec();
        let branch = StoredSession {
            id: format!("loom-branch-{:x}", now.as_nanos()),
            title: format!("분기 · {}", stored.title),
            updated_at: now.as_secs(),
            // A summary made from later turns would describe things that never happened.
            compact_summary: stored.compact_summary.clone().filter(|_| stored.compacted <= kept.len()),
            compacted: if stored.compacted <= kept.len() { stored.compacted } else { 0 },
            turn_tasks: stored.turn_tasks.iter().filter(|(_, task)| kept.contains(task)).map(|(t, k)| (t.clone(), k.clone())).collect(),
            checkpoints: stored.checkpoints.iter().filter(|(task, _)| kept.contains(task)).map(|(t, r)| (t.clone(), r.clone())).collect(),
            task_ids: kept,
        };
        self.save_stored_session(&branch);
        Ok(branch.id)
    }

    /// The change that restores the repository to its state after `turn`. Refused when
    /// files changed since Loom's last recorded state, so user edits are never overwritten.
    fn code_rewind(&self, turn: &str) -> Result<loom_core::effects::Patch> {
        use loom_core::base::Revision;
        ensure!(!self.active && self.pending.is_none(), "현재 작업과 승인을 마친 뒤 코드를 되돌릴 수 있습니다.");
        let stored = self.get_stored_session(&self.session).context("현재 대화를 찾을 수 없습니다")?;
        let task = stored.turn_tasks.get(turn).map(String::as_str).unwrap_or(turn);
        let target = stored.checkpoints.get(task).context(
            "이 답변에는 코드 복원 기록이 없습니다 (복원 기록을 남기기 전에 만든 대화). 대화만 되돌릴 수 있습니다.",
        )?;
        let latest = stored
            .task_ids
            .iter()
            .rev()
            .find_map(|id| stored.checkpoints.get(id))
            .context("코드 복원 기록이 없습니다")?;
        let expected = Revision::try_from(latest.clone()).map_err(anyhow::Error::msg)?;
        let target = Revision::try_from(target.clone()).map_err(anyhow::Error::msg)?;
        ensure!(
            Revision::capture(&self.root)? == expected,
            "마지막 Loom 작업 이후 파일이 바뀌었습니다. 코드를 되돌리면 그 변경을 덮어쓰므로 대화만 되돌릴 수 있습니다."
        );
        loom_core::effects::Patch::between(&self.root, &expected, &target)
    }

    fn get_stored_session(&self, id: &str) -> Option<StoredSession> {
        let dir = self.sessions_dir();
        let path = dir.join(format!("{id}.json"));
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    fn refresh_sessions(&mut self) {
        let mut stored = self.list_stored_sessions();
        if stored.is_empty() {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let initial = StoredSession {
                id: format!("loom-{}", session_key(self.version)),
                title: format!("Loom {}", self.version.label()),
                updated_at: now,
                task_ids: vec![],
                ..Default::default()
            };
            self.save_stored_session(&initial);
            stored.push(initial);
        }
        let summaries: Vec<SessionSummary> = stored
            .into_iter()
            .map(|s| SessionSummary {
                id: s.id,
                title: s.title,
                status: "idle".into(),
                updated_at: Some(s.updated_at),
            })
            .collect();
        self.events
            .push_back(AgentEvent::global(AgentUpdate::SessionsLoaded {
                sessions: summaries,
                cursor: None,
                append: false,
            }));
    }

    fn load_session(&mut self, session_id: &str) -> Result<Option<Outcome>> {
        let history = core::history(self.version, &self.root, &self.settings)?;
        let stored = self.get_stored_session(session_id).unwrap_or_else(|| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let s = StoredSession {
                id: session_id.to_string(),
                title: format!("Loom {}", self.version.label()),
                updated_at: now,
                task_ids: history.iter().map(|o| o.id.clone()).collect(),
                ..Default::default()
            };
            self.save_stored_session(&s);
            s
        });
        self.session = stored.id.clone();

        let session_ids: HashSet<&str> = stored.task_ids.iter().map(String::as_str).collect();
        let session_tasks: Vec<_> = history
            .into_iter()
            .filter(|o| session_ids.contains(o.id.as_str()))
            .collect();

        let turns: Vec<TurnSnapshot> = session_tasks
            .iter()
            .map(|o| TurnSnapshot {
                id: Some(o.id.clone()),
                status: if o.success() {
                    "completed"
                } else if o.pending() {
                    "inProgress"
                } else {
                    "failed"
                }
                .into(),
                entries: {
                    let mut entries = vec![
                        Entry {
                            id: format!("{}-user", o.id),
                            kind: Kind::User,
                            title: String::new(),
                            body: o.goal.clone(),
                            status: "completed".into(),
                            expanded: false,
                        },
                        Entry {
                            id: format!("{}-assistant", o.id),
                            kind: Kind::Assistant,
                            title: self.version.label().into(),
                            body: Self::report(o),
                            status: if o.success() || o.pending() {
                                "completed"
                            } else {
                                "failed"
                            }
                            .into(),
                            expanded: false,
                        },
                    ];
                    if let Some(change) = Self::change_entry(o, &o.id) {
                        entries.push(change);
                    }
                    let mut conversation = super::version_conversation::Conversation::default();
                    for event in &o.events {
                        conversation.observe(event);
                    }
                    if let Some(log) = conversation.log(&o.id, true) {
                        entries.push(log);
                    }
                    if let Some(log) = conversation.graph_log(&o.id) {
                        entries.push(log);
                    }
                    let answer = entries.remove(1);
                    entries.extend(Self::v5_logs(o));
                    entries.push(answer);
                    entries
                },
                activities: vec![],
            })
            .collect();

        self.events
            .push_back(AgentEvent::global(AgentUpdate::SessionLoaded {
                snapshot: SessionSnapshot {
                    id: stored.id.clone(),
                    title: Some(stored.title.clone()),
                    status: Some("idle".into()),
                    turns: Some(turns),
                },
                switching: true,
                model: Some(self.settings.model.clone()),
                permissions: self.permissions(),
            }));

        self.refresh_sessions();
        for o in &session_tasks {
            self.project_task(o);
        }
        Ok(session_tasks.into_iter().rev().find(|outcome| outcome.pending()))
    }

    fn emit(&mut self, update: AgentUpdate) {
        self.events.push_back(
            if matches!(
                &update,
                AgentUpdate::SessionLoaded { .. }
                    | AgentUpdate::SessionsLoaded { .. }
                    | AgentUpdate::ModelsLoaded(_)
                    | AgentUpdate::ModelProfilesLoaded(_)
                    | AgentUpdate::ModelCatalogLoaded(_)
            ) {
                AgentEvent::global(update)
            } else {
                AgentEvent::scoped(self.session.clone(), update)
            },
        );
    }

    fn refresh_models(&mut self) {
        let profiles = self.model.model_profiles();
        let ceiling = models::cost_ceiling();
        let by_id: HashMap<String, &models::ModelProfile> = profiles
            .iter()
            .rev()
            .map(|p| (format!("{}/{}", p.candidate.endpoint, p.candidate.model), p))
            .collect();
        let mut names = vec!["auto".into()];
        names.extend(
            profiles
                .iter()
                .filter(|p| p.candidate.cost <= ceiling)
                .map(|p| format!("{}/{}", p.candidate.endpoint, p.candidate.model)),
        );
        if !names.contains(&self.settings.model) {
            names.push(self.settings.model.clone());
        }
        let ui_profiles = names
            .iter()
            .map(|id| ModelProfile {
                id: id.clone(),
                efforts: by_id.get(id)
                    .map(|p| {
                        p.supported_efforts
                            .iter()
                            .map(|value| EffortOption {
                                value: value.clone(),
                                description: format!("{value} reasoning effort"),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                default_effort: by_id.get(id)
                    .and_then(|p| p.default_effort.clone()),
            })
            .collect();
        self.events
            .push_back(AgentEvent::global(AgentUpdate::ModelsLoaded(names)));
        self.events.push_back(AgentEvent::global(AgentUpdate::ModelCatalogLoaded(
            profiles.iter().map(|p| ModelCatalogEntry {
                id: format!("{}/{}", p.candidate.endpoint, p.candidate.model),
                cost: format!("{:?}", p.candidate.cost).to_ascii_lowercase(),
                price: p.candidate.price,
                tools: p.candidate.tools,
                context: p.candidate.context,
                name: (p.candidate.endpoint == "lmx")
                    .then(|| loom_core::models::lmx::display_name(&p.candidate.model))
                    .flatten(),
            }).collect(),
        )));
        self.events
            .push_back(AgentEvent::global(AgentUpdate::ModelProfilesLoaded(
                ui_profiles,
            )));
    }

    fn approval_policy_path(root: &Path, settings: &Settings) -> PathBuf {
        let workspace = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        core::state_directory(settings)
            .join("permissions")
            .join(format!("{workspace}.json"))
    }

    fn restore_approval_policy(root: &Path, settings: &mut Settings) -> Result<()> {
        let path = Self::approval_policy_path(root, settings);
        let policy = match std::fs::read(&path) {
            Ok(bytes) => {
                let stored: StoredApprovalPolicy = serde_json::from_slice(&bytes)
                    .with_context(|| format!("invalid approval policy in {}", path.display()))?;
                loom_core::agent::ApprovalPolicy::parse(&stored.approval_policy)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                settings.input_control.approval_policy()
            }
            Err(error) => return Err(error).context("read runtime approval policy"),
        };
        settings.input_control.set_approval_policy(policy);
        Ok(())
    }

    fn save_approval_policy(&self, policy: loom_core::agent::ApprovalPolicy) -> Result<()> {
        use std::io::Write;

        let path = Self::approval_policy_path(&self.root, &self.settings);
        let directory = path.parent().context("runtime approval policy directory")?;
        std::fs::create_dir_all(directory).context("create runtime approval policy directory")?;
        let stored = StoredApprovalPolicy {
            approval_policy: policy.as_str().into(),
        };
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(&serde_json::to_vec_pretty(&stored)?)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .context("save runtime approval policy")?;
        Ok(())
    }

    fn permissions(&self) -> Permissions {
        Permissions {
            approval: Some(self.settings.input_control.approval_policy().as_str().into()),
            sandbox: Some(
                match self.settings.policy.sandbox {
                    loom_core::effects::SandboxMode::ReadOnly => "readOnly",
                    loom_core::effects::SandboxMode::WorkspaceWrite => "workspaceWrite",
                    loom_core::effects::SandboxMode::FullAccess => "dangerFullAccess",
                }
                .into(),
            ),
            network: Some(if self.settings.policy.network { "allow" } else { "deny" }.into()),
        }
    }
    pub(crate) fn report(o: &Outcome) -> String {
        if o.cancelled() {
            return "작업을 중지했습니다. 진행한 내용은 작업 기록에서 확인할 수 있습니다.".into();
        }
        if o.pending() {
            return format!(
                "{}\n\n수정 내용을 검증했습니다. 승인하면 변경을 적용합니다.",
                o.summary
            );
        }
        if o.success() {
            return o.summary.clone();
        }
        if let Some(reason) = super::v5_projection::failure_summary(&o.summary) {
            return reason.into();
        }
        if let Some(issue) = o
            .events
            .iter()
            .rev()
            .filter_map(crate::backend::activity_events::ModelIssue::decode)
            .find(|issue| !issue.retrying)
        {
            use crate::backend::activity_events::ModelErrorKind;
            let reason = match issue.category {
                Some(ModelErrorKind::Transient) => {
                    "모델 연결이 계속 실패해 작업을 마무리하지 못했습니다."
                }
                Some(ModelErrorKind::TimedOut) => "작업 시간이 초과되었습니다.",
                Some(ModelErrorKind::RateLimited) => {
                    "모델 서버의 요청 제한으로 답변을 받지 못했습니다."
                }
                Some(ModelErrorKind::Auth) => "모델 연결의 인증을 확인하지 못해 작업을 멈췄습니다.",
                Some(ModelErrorKind::Quota) => {
                    "현재 설정으로 사용할 수 있는 모델이 없어 작업을 멈췄습니다."
                }
                Some(ModelErrorKind::BadRequest) => {
                    "모델이 요청을 받아들이지 못해 작업을 멈췄습니다."
                }
                Some(ModelErrorKind::Cancelled) => "작업을 중지했습니다.",
                None => "모델 응답을 받지 못해 작업을 멈췄습니다.",
            };
            return reason.into();
        }
        let stage = if o.events.iter().any(|e| e.worker == "validation") {
            "작업 결과를 확인하다 멈췄습니다."
        } else if o.events.iter().any(|e| {
            matches!(
                e.kind,
                crate::backend::activity_events::EventKind::ToolStarted
                    | crate::backend::activity_events::EventKind::ToolSucceeded
                    | crate::backend::activity_events::EventKind::ToolFailed
            )
        }) {
            "작업을 진행했지만 끝까지 마무리하지 못했습니다."
        } else if o.calls > 0
            && !o.events.iter().any(|e| {
                e.worker != "coordinator"
                    && e.kind == crate::backend::activity_events::EventKind::Started
            })
        {
            "실행 계획을 준비하다 멈췄습니다."
        } else {
            "작업을 마무리하지 못했습니다."
        };
        stage.into()
    }
    fn project_task(&mut self, o: &Outcome) {
        self.emit(AgentUpdate::TaskUpdated(ActivityTask {
            id: o.id.clone(),
            title: o.goal.clone(),
            model: o.model_calls.iter().rev().find_map(|call| call.served.clone()),
            status: if o.pending() {
                TaskStatus::NeedsReview
            } else if o.success() {
                TaskStatus::Completed
            } else if o.cancelled() {
                TaskStatus::Cancelled
            } else {
                TaskStatus::Failed
            },
            reason: Some(o.plan_reason.clone()),
            elapsed_ms: Some(o.elapsed_ms),
            session: Some(self.session.clone()),
            inputs: o.sources.clone(),
            partition: Some(Box::new(TaskPartition {
                mode: if o.plan_mode == "split" {
                    ExecutionMode::Split
                } else {
                    ExecutionMode::Single
                },
                reason: o.plan_reason.clone(),
                files: o.files.clone(),
                groups: o.groups.clone(),
                symbols: vec![],
            })),
            cost: Some(TaskCost {
                input_tokens: o.input,
                cached_input_tokens: o.cached,
                output_tokens: o.output,
                model_calls: o.calls as u32,
                ..Default::default()
            }),
            ..Default::default()
        }));
    }

    fn change_entry(o: &Outcome, turn: &str) -> Option<Entry> {
        if o.diff.trim().is_empty() {
            return None;
        }
        let title = match o.files.as_slice() {
            [path] => format!("파일 변경 · {path}"),
            files => format!("파일 변경 · {}개 파일", files.len()),
        };
        Some(Entry {
            id: format!("{turn}-change"),
            kind: Kind::Change,
            title,
            body: o.diff.clone(),
            status: "completed".into(),
            expanded: false,
        })
    }

    fn request_approval(&mut self, o: Outcome) {
        self.emit(AgentUpdate::DiffUpdated(o.diff.clone()));
        self.emit(AgentUpdate::ApprovalRequested(Approval {
            id: o.id.clone(),
            title: format!("{} 검증된 변경 승인", self.version.label()),
            summary: o.summary.clone(),
            detail: format!("{}\n기록: {}", o.diff, o.records.display()),
            choices: vec![
                Choice {
                    label: "승인하고 적용".into(),
                    result: "accept".into(),
                },
                Choice {
                    label: "거절하고 보존".into(),
                    result: "reject".into(),
                },
            ],
            answering: false,
            expanded: false,
        }));
        self.pending = Some(o);
        self.emit(AgentUpdate::StatusUpdated {
            status: "변경 승인 대기".into(),
            busy: false,
        });
    }

    fn submit(&mut self, text: String, model: String) -> Result<()> {
        ensure!(
            !self.active && self.pending.is_none(),
            "먼저 실행 중인 작업 또는 변경 승인을 마무리해주세요"
        );
        let (intent, goal) = core::parse(&text);
        ensure!(!goal.is_empty(), "empty request");
        ensure!(
            self.trusted || !matches!(intent, Intent::Edit),
            "편집하려면 워크스페이스를 신뢰해주세요"
        );
        ensure!(
            !self.settings.readonly || !matches!(intent, Intent::Edit),
            "읽기 전용 엔진에서 편집할 수 없습니다"
        );
        let stored = self.get_stored_session(&self.session).unwrap_or_default();
        let compact = stored.compact_summary.clone();
        let ids: Vec<String> = stored.task_ids.iter().skip(stored.compacted).cloned().collect();
        if let Some(handle) = self.thread.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("previous Core worker panicked"))?;
        }

        // Auto-update session title on first submit if it is "새 대화"
        if let Some(mut stored) = self.get_stored_session(&self.session) {
            if stored.title == "새 대화" {
                stored.title = goal.chars().take(45).collect();
                self.save_stored_session(&stored);
                self.refresh_sessions();
            }
        }

        self.sequence += 1;
        self.conversation = Default::default();
        self.v5_projection = Default::default();
        let turn = format!(
            "{}-turn-{}-{}",
            self.session,
            std::process::id(),
            self.sequence
        );
        self.turn = Some(turn.clone());
        self.active = true;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.settings.model = model;
        self.emit(AgentUpdate::TurnSubmitted {
            id: Some(turn.clone()),
            status: "inProgress".into(),
        });
        self.emit(AgentUpdate::TurnStarted {
            id: Some(turn.clone()),
        });
        self.emit(AgentUpdate::EntryUpdated(Entry {
            id: format!("{turn}-user"),
            kind: Kind::User,
            title: String::new(),
            body: text,
            status: "completed".into(),
            expanded: false,
        }));
        self.emit(AgentUpdate::StatusUpdated {
            status: "대화 기록 불러오는 중".into(),
            busy: true,
        });
        self.emit(AgentUpdate::EntryUpdated(Entry {
            id: format!("{turn}-assistant"),
            kind: Kind::Assistant,
            title: crate::agent::PROGRESS_TITLE.into(),
            body: "대화 기록 불러오는 중".into(),
            status: "inProgress".into(),
            expanded: false,
        }));
        let root = self.root.clone();
        let mut settings = self.settings.clone();
        settings.readonly |= !self.trusted;
        let version = self.version;
        let model = self.model.clone();
        let cancel = self.cancel.clone();
        let tx = self.tx.clone();
        let session = self.session.clone();
        self.thread = Some(thread::spawn(move || {
            let progress = tx.clone();
            let observer = Arc::new(move |event| {
                let _ = progress.send(Message::Progress(event));
            });
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                settings.history = loom_core::agent::Core::open(&root)?.context(&ids, 40000)?;
                // Turns folded by /compact arrive as their summary, before the newer turns.
                if let Some(summary) = &compact {
                    settings.history.splice(0..0, compacted_history(summary));
                }
                ensure!(!cancel.load(Ordering::Acquire), "execution cancelled");
                core::execute_with_index(
                    version,
                    &root,
                    &goal,
                    intent,
                    settings,
                    model,
                    cancel,
                    observer,
                    None,
                )
            }))
            .map_err(|_| "Core worker panicked".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            // Report first: a stop must not wait for the checkpoint capture.
            // A change awaiting approval is recorded once applied (the approval path), so
            // a late capture here can never replace the applied state.
            let checkpoint = result
                .as_ref()
                .ok()
                .filter(|outcome| !outcome.cancelled() && !outcome.pending())
                .map(|outcome| outcome.id.clone());
            let _ = tx.send(Message::Done(result));
            if let Some(task) = checkpoint {
                send_checkpoint(&tx, &root, &session, &task);
            }
        }));
        Ok(())
    }

    fn completed(&mut self, o: Outcome) {
        // Record task into current stored session
        if let Some(mut stored) = self.get_stored_session(&self.session) {
            if !stored.task_ids.contains(&o.id) {
                stored.task_ids.push(o.id.clone());
            }
            if let Some(turn) = &self.turn {
                stored.turn_tasks.insert(turn.clone(), o.id.clone());
            }
            if stored.title == "새 대화" || stored.title.starts_with("Loom ") {
                stored.title = o.goal.chars().take(45).collect();
            }
            stored.updated_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            self.save_stored_session(&stored);
            // Cancellation should acknowledge promptly. Rebuilding the session list can
            // involve filesystem work and is not needed to make the interrupted turn
            // durable; the next ordinary refresh will pick up the saved metadata.
            if !o.cancelled() && !self.cancel.load(Ordering::SeqCst) {
                self.refresh_sessions();
            }
        }

        self.project_task(&o);
        let turn = self.turn.clone().unwrap_or_else(|| o.id.clone());
        // A turn that ends while the model is writing (timeout, error, Esc) keeps what
        // was written; the outcome report goes below it instead of replacing it.
        let partial = (!(o.success() || o.pending()))
            .then(|| self.v5_projection.streaming_text().to_owned())
            .filter(|text| !text.is_empty());
        self.v5_projection
            .finish(o.success() || o.pending(), o.cancelled(), &o.summary);
        for entry in self.v5_projection.entries(&turn, true) {
            self.emit(AgentUpdate::EntryUpdated(entry));
        }
        self.input = self.input.zip(o.input).map(|(a, b)| a.saturating_add(b));
        self.output = self.output.zip(o.output).map(|(a, b)| a.saturating_add(b));
        self.emit(AgentUpdate::TokenUsageUpdated(TokenUsage {
            input: self.input,
            output: self.output,
            total: self
                .input
                .zip(self.output)
                .map(|(a, b)| a.saturating_add(b)),
            ..Default::default()
        }));
        let turn = self.turn.clone().unwrap_or_else(|| o.id.clone());
        if let Some(change) = Self::change_entry(&o, &turn) {
            self.emit(AgentUpdate::DiffUpdated(o.diff.clone()));
            self.emit(AgentUpdate::EntryUpdated(change));
        }
        if let Some(partial) = &partial {
            self.emit(AgentUpdate::EntryUpdated(Entry {
                id: format!("{turn}-assistant"),
                kind: Kind::Assistant,
                title: String::new(),
                body: partial.clone(),
                status: "completed".into(),
                expanded: false,
            }));
        }
        self.emit(AgentUpdate::EntryUpdated(Entry {
            id: if partial.is_some() { format!("{turn}-report") } else { format!("{turn}-assistant") },
            kind: Kind::Assistant,
            title: format!("Native {}", self.version.label()),
            body: Self::report(&o),
            status: if o.success() || o.pending() {
                "completed"
            } else {
                "failed"
            }
            .into(),
            expanded: false,
        }));
        if let Some(log) = self.conversation.log(&turn, true) {
            self.emit(AgentUpdate::EntryUpdated(log));
        }
        if let Some(log) = self.conversation.graph_log(&turn) {
            self.emit(AgentUpdate::EntryUpdated(log));
        }
        if o.pending() && !self.cancel.load(Ordering::SeqCst) {
            self.request_approval(o);
            return;
        }
        if o.pending() {
            let _ = core::reject(self.version, &self.root, &o.id, &self.settings);
        }
        self.emit(AgentUpdate::StatusUpdated {
            status: format!("{} · 대기", self.version.label()),
            busy: false,
        });
        let turn_id = self.turn.take();
        self.emit(AgentUpdate::TurnCompleted {
            id: turn_id,
            status: if self.cancel.load(Ordering::SeqCst) || o.cancelled() {
                "interrupted"
            } else if o.success() {
                "completed"
            } else {
                "failed"
            }
            .into(),
            error: o
                .blocker
                .as_ref()
                .map(|_| "작업을 마무리하지 못했습니다.".into()),
        });
    }
}

fn session_key(version: Version) -> &'static str {
    match version {
        Version::Loom => "main",
    }
}

impl AgentBackend for VersionBackend {
    fn command(&mut self, command: AgentCommand) -> Result<()> {
        ensure!(!self.stopped, "backend shut down");
        match command {
            AgentCommand::Inquiry(question) => self.inquire(question)?,
            AgentCommand::Submit {
                text,
                model,
                images,
                skills,
                effort,
            } => {
                ensure!(images.is_empty(), "현재 Loom 실행 경로는 첨부 이미지를 지원하지 않습니다");
                self.settings.skills = skills.into_iter().map(|skill| PathBuf::from(skill.path)).collect();
                self.settings.effort = effort;
                self.submit(text, model)?;
            }
            AgentCommand::Interrupt => {
                if self.active || self.pending.is_some() {
                    self.cancel.store(true, Ordering::SeqCst);
                    self.inquiry_cancel.store(true, Ordering::SeqCst);
                    self.emit(AgentUpdate::StatusUpdated {
                        status: "중단됨".into(),
                        busy: true,
                    });
                    if !self.active {
                        if let Some(o) = self.pending.take() {
                            core::reject(self.version, &self.root, &o.id, &self.settings)?;
                            self.emit(AgentUpdate::ApprovalResolved(o.id));
                            let turn_id = self.turn.take();
                            self.emit(AgentUpdate::TurnCompleted {
                                id: turn_id,
                                status: "interrupted".into(),
                                error: None,
                            });
                        }
                    }
                }
            }
            AgentCommand::Answer { request_id, answers } => {
                let answer = answers.into_iter().filter_map(|a| match a.value { AnswerValue::Text(text) => Some(text), _ => None }).collect::<Vec<_>>().join("\n");
                ensure!(!answer.trim().is_empty(), "답변을 입력하세요");
                self.settings.input_control.answer(request_id.clone(),answer);
                self.emit(AgentUpdate::InputResolved(request_id));
            }
            AgentCommand::CancelInput { request_id } => {
                self.settings.input_control.cancel(request_id.clone());
                self.emit(AgentUpdate::InputResolved(request_id));
            }
            AgentCommand::Reply { request_id, choice } => {
                ensure!(!self.active, "approval already running");
                let o = self.pending.as_ref().context("no pending proposal")?;
                ensure!(o.id == request_id, "approval id mismatch");
                ensure!(
                    choice == "accept" || choice == "reject",
                    "unknown approval choice"
                );
                if choice == "reject" {
                    core::reject(self.version, &self.root, &request_id, &self.settings)?;
                    self.pending = None;
                    self.emit(AgentUpdate::ApprovalResolved(request_id));
                    let turn_id = self.turn.take();
                    self.emit(AgentUpdate::TurnCompleted {
                        id: turn_id,
                        status: "interrupted".into(),
                        error: None,
                    });
                } else {
                    self.active = true;
                    self.conversation = Default::default();
                    if let Some(pending) = &self.pending {
                        for event in &pending.events { self.conversation.observe(event); }
                    }
                    self.cancel = Arc::new(AtomicBool::new(false));
                    self.emit(AgentUpdate::StatusUpdated {
                        status: "승인한 변경 재검증 중".into(),
                        busy: true,
                    });
                    let root = self.root.clone();
                    let settings = self.settings.clone();
                    let version = self.version;
                    let cancel = self.cancel.clone();
                    let tx = self.tx.clone();
                    let session = self.session.clone();
                    self.thread = Some(thread::spawn(move || {
                        let progress = tx.clone();
                        let r = core::approve_with_progress(version, &root, &request_id, &settings, &cancel, Arc::new(move |event| { let _ = progress.send(Message::Progress(event)); }))
                            .map_err(|e| e.to_string());
                        let checkpoint = r.as_ref().ok().map(|outcome| outcome.id.clone());
                        let _ = tx.send(Message::Approved(r));
                        if let Some(task) = checkpoint {
                            send_checkpoint(&tx, &root, &session, &task);
                        }
                    }));
                }
            }
            AgentCommand::Explore(query) => self.submit(
                format!(":inspect search {query}"),
                self.settings.model.clone(),
            )?,
            AgentCommand::RefreshModels => {
                // A local server/model may have appeared since startup. Publish the
                // current list immediately, then rediscover without blocking input.
                self.refresh_models();
                if self.managed_models { self.reload_providers()?; }
            }
            AgentCommand::RefreshSessions | AgentCommand::MoreSessions(_) => {
                self.refresh_sessions()
            }
            AgentCommand::Compact => {
                ensure!(
                    !self.active && !self.inquiry_active && self.pending.is_none(),
                    "실행 중에는 대화를 압축할 수 없습니다"
                );
                let stored = self.get_stored_session(&self.session).unwrap_or_default();
                let ids: Vec<String> = stored.task_ids.iter().skip(stored.compacted).cloned().collect();
                if ids.is_empty() {
                    self.emit(AgentUpdate::Notice("압축할 새 대화가 없습니다.".into()));
                    return Ok(());
                }
                self.emit(AgentUpdate::Notice("대화를 요약하는 중…".into()));
                let (root, model, tx, session) = (self.root.clone(), self.model.clone(), self.tx.clone(), self.session.clone());
                let previous = stored.compact_summary.clone();
                let requested = self.settings.model.clone();
                let through = stored.task_ids.len();
                thread::spawn(move || {
                    let result = (|| -> Result<String> {
                        let mut history = compacted_history(previous.as_deref().unwrap_or_default());
                        if previous.is_none() {
                            history.clear();
                        }
                        history.extend(loom_core::agent::Core::open(&root)?.context(&ids, 120_000)?);
                        history.push(models::Message::user(COMPACT_REQUEST));
                        let request = models::Request {
                            model: if requested == "auto" { String::new() } else { requested },
                            system: Some("You write concise, faithful conversation summaries for continuing work.".into()),
                            messages: history,
                            max_output_tokens: Some(1500),
                            reasoning_effort: Some("none".into()),
                            ..Default::default()
                        };
                        let served = model.complete(models::Need::default(), &request)?;
                        let summary = served.response.text();
                        ensure!(!summary.trim().is_empty(), "모델이 빈 요약을 반환했습니다");
                        Ok(summary)
                    })();
                    let _ = tx.send(Message::Compacted { session, through, result: result.map_err(|error| error.to_string()) });
                });
            }
            AgentCommand::Fork { turn_id, .. } => {
                ensure!(!self.active && self.pending.is_none(), "실행 중에는 대화를 되돌릴 수 없습니다");
                let branch = self.branch_session(&turn_id)?;
                self.load_session(&branch)?;
                self.emit(AgentUpdate::Notice("대화를 되돌렸습니다. 이전 대화는 세션 목록에 남아 있습니다.".into()));
            }
            AgentCommand::PreviewRewind { turn_id } => {
                let (files, added, removed, blocked) = match self.code_rewind(&turn_id) {
                    Ok(patch) => {
                        let text = String::from_utf8_lossy(&patch.diff);
                        let added = text.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
                        let removed = text.lines().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
                        (patch.files.len(), added, removed, None)
                    }
                    Err(reason) => (0, 0, 0, Some(reason.to_string())),
                };
                self.emit(AgentUpdate::RewindPreview { turn_id, files, added, removed, blocked });
            }
            AgentCommand::Rewind { turn_id, .. } => {
                ensure!(!self.active && self.pending.is_none(), "실행 중에는 대화를 되돌릴 수 없습니다");
                ensure!(!self.settings.readonly && self.trusted, "쓰기 권한이 있어야 코드를 되돌릴 수 있습니다");
                let patch = self.code_rewind(&turn_id)?;
                patch.apply(&self.root)?;
                let branch = self.branch_session(&turn_id)?;
                self.load_session(&branch)?;
                self.emit(AgentUpdate::Notice("대화와 코드를 되돌렸습니다.".into()));
            }
            AgentCommand::DeleteSession(id) => {
                ensure!(id != self.session, "열려 있는 대화는 삭제할 수 없습니다");
                ensure!(
                    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
                    "잘못된 대화 id입니다"
                );
                let path = self.sessions_dir().join(format!("{id}.json"));
                std::fs::remove_file(&path)
                    .map_err(|error| anyhow::anyhow!("대화를 삭제할 수 없습니다: {error}"))?;
                self.emit(AgentUpdate::Notice("대화를 삭제했습니다.".into()));
                self.refresh_sessions();
            }
            AgentCommand::OpenSession { id, .. } => {
                ensure!(
                    !self.active && !self.inquiry_active && self.pending.is_none(),
                    "먼저 실행 중인 작업 또는 변경 승인을 마무리해주세요"
                );
                match id {
                    Some(id) => { let _ = self.load_session(&id)?; },
                    None => {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let new_id = format!(
                            "session-{:x}-{}",
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_nanos(),
                            std::process::id()
                        );
                        let s = StoredSession {
                            id: new_id.clone(),
                            title: "새 대화".into(),
                            updated_at: now,
                            task_ids: vec![],
                            ..Default::default()
                        };
                        self.save_stored_session(&s);
                        let _ = self.load_session(&new_id)?;
                    }
                }
            }
            AgentCommand::RefreshSkills => {
                let (skills, errors) = match loom_core::instructions::discover(&self.root) {
                    Ok(found) => (
                        found
                            .into_iter()
                            .map(|s| crate::agent::Skill { name: s.name, path: s.path.display().to_string(), description: s.description })
                            .collect(),
                        vec![],
                    ),
                    Err(error) => (vec![], vec![error.to_string()]),
                };
                self.emit(AgentUpdate::SkillsLoaded { skills, errors });
            }
            AgentCommand::RefreshUsage => self.emit(AgentUpdate::Notice(
                "이 엔진은 공급자 할당량을 보고하지 않습니다. 실행별 토큰은 /cost에서 확인할 수 있습니다."
                    .into(),
            )),
            AgentCommand::TrustWorkspace(value) => {
                self.trusted = value;
                if !value {
                    self.settings.readonly = true;
                    self.settings.policy.sandbox = loom_core::effects::SandboxMode::ReadOnly;
                    self.settings.policy.network = false;
                }
                self.emit(AgentUpdate::TrustResolved(value));
                self.emit(AgentUpdate::PermissionsUpdated {
                    permissions: self.permissions(),
                    defaults_only: false,
                });
            }
            AgentCommand::UpdatePermission { kind, value } => {
                ensure!(
                    !self.active && !self.inquiry_active && self.pending.is_none(),
                    "실행 중 권한을 바꿀 수 없습니다"
                );
                if kind == "sandbox" {
                    ensure!(
                        self.trusted || value == "readOnly",
                        "workspace is untrusted"
                    );
                    let sandbox = match value.as_str() {
                        "readOnly" => loom_core::effects::SandboxMode::ReadOnly,
                        "workspaceWrite" => loom_core::effects::SandboxMode::WorkspaceWrite,
                        "dangerFullAccess" => loom_core::effects::SandboxMode::FullAccess,
                        _ => anyhow::bail!("unknown sandbox"),
                    };
                    self.settings.readonly = sandbox == loom_core::effects::SandboxMode::ReadOnly;
                    self.settings.policy.sandbox = sandbox;
                } else if kind == "network" {
                    let network = match value.as_str() {
                        "allow" | "true" => true,
                        "deny" | "false" => false,
                        _ => anyhow::bail!("unknown network policy"),
                    };
                    ensure!(self.trusted || !network, "workspace is untrusted");
                    self.settings.policy.network = network;
                } else {
                    ensure!(kind == "approval", "unknown permission kind");
                    let policy = loom_core::agent::ApprovalPolicy::parse(&value)?;
                    self.save_approval_policy(policy)?;
                    self.settings.input_control.set_approval_policy(policy);
                }
                self.emit(AgentUpdate::PermissionApplied);
                self.emit(AgentUpdate::PermissionsUpdated {
                    permissions: self.permissions(),
                    defaults_only: false,
                });
            }
            _ => anyhow::bail!(
                "이 기능은 {} 엔진에서 지원하지 않습니다",
                self.version.label()
            ),
        }
        Ok(())
    }

    fn reload_providers(&mut self) -> Result<()> {
        self.pending_model = None;
        if !self.managed_models { self.refresh_models(); return Ok(()); }
        self.reload_generation = self.reload_generation.wrapping_add(1);
        let generation = self.reload_generation;
        let tx = self.tx.clone();
        self.emit(AgentUpdate::Notice("공급자 모델 목록을 갱신하는 중…".into()));
        self.events.push_back(AgentEvent::global(AgentUpdate::ModelCatalogLoading));
        thread::spawn(move || {
            let result = std::panic::catch_unwind(|| -> Result<Arc<dyn ModelPort>> {
                models::registry::Registry::open()?.config()?;
                let (router, sources) = models::discover::discover(models::cli::ledger_path());
                let _ = tx.send(Message::ModelSources { generation, sources });
                Ok(Arc::new(router))
            })
                .map_err(|_| "공급자 갱신 중 오류가 발생했습니다.".to_owned())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = tx.send(Message::ModelsReady { generation, result });
        });
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<AgentEvent>> {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Message::Checkpoint { session, task, revision } => {
                    if let Some(mut stored) = self.get_stored_session(&session) {
                        stored.checkpoints.insert(task, revision);
                        self.save_stored_session(&stored);
                    }
                }
                Message::Compacted { session, through, result } => match result {
                    Ok(summary) => {
                        if let Some(mut stored) = self.get_stored_session(&session) {
                            let before = stored.task_ids.len();
                            stored.compact_summary = Some(summary);
                            stored.compacted = through.min(before);
                            self.save_stored_session(&stored);
                            self.emit(AgentUpdate::Notice(format!(
                                "대화 {through}턴을 요약했습니다. 다음 요청부터 요약과 이후 대화만 보냅니다."
                            )));
                        }
                    }
                    Err(error) => self.emit(AgentUpdate::Notice(format!("대화 압축 실패: {error}"))),
                },
                Message::ModelSources { generation, sources } => {
                    if generation != self.reload_generation { continue; }
                    self.events.push_back(AgentEvent::global(AgentUpdate::ModelCatalogSources(
                        sources.into_iter().map(|source| {
                            let (count, error) = match source.status { Ok(count) => (Some(count), None), Err(error) => (None, Some(error)) };
                            ModelCatalogSource { name: source.name, count, error }
                        }).collect(),
                    )));
                }
                Message::ModelsReady { generation, result } => {
                    if generation != self.reload_generation { continue; }
                    if let Some(loading) = self.loading_model.take() {
                        loading.resolve(result.clone());
                    }
                    match result {
                        Ok(model) => {
                            self.pending_model = Some(model);
                            if self.active || self.inquiry_active { self.emit(AgentUpdate::Notice("연결 갱신 준비 완료 · 진행 중인 작업이 끝나면 적용합니다.".into())); }
                        }
                        Err(error) => {
                            self.events.push_back(AgentEvent::global(AgentUpdate::ModelCatalogSources(vec![ModelCatalogSource { name: "connections".into(), count: None, error: Some(error.clone()) }])));
                            self.emit(AgentUpdate::Error(format!("공급자 갱신 실패: {error}")));
                        }
                    }
                }
                Message::InquiryDone { id, result } => {
                    self.inquiry_active = false;
                    let (body, status) = match result {
                        Ok(task) => {
                            if let Some(mut stored) = self.get_stored_session(&self.session) {
                                stored.task_ids.push(task.id.clone());
                                self.save_stored_session(&stored);
                            }
                            self.input = self
                                .input
                                .zip(task.usage.input)
                                .map(|(a, b)| a.saturating_add(b));
                            self.output = self
                                .output
                                .zip(task.usage.output)
                                .map(|(a, b)| a.saturating_add(b));
                            self.emit(AgentUpdate::TokenUsageUpdated(TokenUsage {
                                input: self.input,
                                output: self.output,
                                total: self
                                    .input
                                    .zip(self.output)
                                    .map(|(a, b)| a.saturating_add(b)),
                                ..Default::default()
                            }));
                            (
                                format!("병렬 질문 답변\n\n{}",task.summary),
                                if matches!(
                                    task.status,
                                    loom_core::agent::Status::Inspected
                                        | loom_core::agent::Status::Answered
                                ) {
                                    "completed"
                                } else {
                                    "failed"
                                },
                            )
                        }
                        Err(reason) => (format!("병렬 질문 답변\n\n{reason}"), "failed"),
                    };
                    self.emit(AgentUpdate::EntryUpdated(Entry {
                        id: format!("{id}-answer"),
                        kind: Kind::Assistant,
                        title: "병렬 질문".into(),
                        body,
                        status: status.into(),
                        expanded: false,
                    }));
                }
                Message::IndexReady(result) => {
                    if !self.active {
                        match result {
                            Ok((files, symbols, reused)) => {
                                self.emit(AgentUpdate::Notice(format!(
                                    "코드 인덱스 준비됨 · {files} 파일 · {symbols} 심볼{}",
                                    if reused { " · 캐시 재사용" } else { "" }
                                )));
                            }
                            Err(_) => {}
                        }
                    }
                }
                Message::Progress(p) => {
                    if self.cancel.load(Ordering::Acquire) {
                        continue;
                    }
                    if p.operation == "loom" {
                        if let Ok(event) =
                            serde_json::from_str::<loom_core::agent::events::Event>(&p.detail)
                        {
                            self.project_v5(event);
                        }
                        continue;
                    }
                    if let Some(event) = &p.event {
                        self.conversation.observe(event);
                        let turn = self
                            .turn
                            .clone()
                            .or_else(|| self.pending.as_ref().map(|o| o.id.clone()))
                            .unwrap_or_else(|| "core".into());
                        self.emit(AgentUpdate::EntryUpdated(
                            self.conversation.assistant(&turn),
                        ));
                        if let Some(log) = self.conversation.log(&turn, false) {
                            self.emit(AgentUpdate::EntryUpdated(log));
                        }
                        if let Some(log) = self.conversation.graph_log(&turn) {
                            self.emit(AgentUpdate::EntryUpdated(log));
                        }
                        self.emit(AgentUpdate::WorkPhaseUpdated(self.conversation.phase()));
                        continue;
                    }
                    if p.operation == "model" {
                        if let Some(id) = p.summary.split(" · ").next() {
                            if let Some((provider, model)) = id.split_once('/') {
                                self.emit(AgentUpdate::ModelResolved {
                                    requested: self.settings.model.clone(),
                                    provider: provider.into(),
                                    model: model.into(),
                                });
                            }
                        }
                    }
                    let phase = if self.conversation.running() {
                        self.conversation.phase()
                    } else if p.operation == "model_request" || p.operation == "model" {
                        WorkPhase::Thinking
                    } else {
                        WorkPhase::Tool
                    };
                    self.emit(AgentUpdate::WorkPhaseUpdated(phase));
                    let display_summary = display_progress_summary(&p.operation, &p.summary);
                    self.emit(AgentUpdate::StatusUpdated {
                        status: display_summary.chars().take(140).collect(),
                        busy: true,
                    });
                    if !matches!(
                        p.operation.as_str(),
                        "model_response"
                            | "model_request"
                            | "result"
                            | "progress"
                            | "model"
                            | "finish"
                    ) {
                        self.sequence += 1;
                        let operation_id = progress_operation_id(&p.detail);
                        let started = progress_is_started(&p.detail);
                        let body = progress_entry_body(&p.operation, &display_summary, &p.detail);
                        self.emit(AgentUpdate::EntryUpdated(Entry {
                            id: operation_id
                                .map(|id| {
                                    format!(
                                        "{}-operation-{id}",
                                        self.turn.as_deref().unwrap_or("core")
                                    )
                                })
                                .unwrap_or_else(|| {
                                    format!(
                                        "{}-operation-{}",
                                        self.turn.as_deref().unwrap_or("core"),
                                        self.sequence
                                    )
                                }),
                            kind: Kind::Tool,
                            title: format!(
                                "{} · {}",
                                p.operation,
                                display_summary.chars().take(120).collect::<String>()
                            ),
                            body,
                            status: if started { "inProgress" } else { "completed" }.into(),
                            expanded: false,
                        }));
                    }
                }
                Message::Done(result) => {
                    self.active = false;
                    match result {
                        Ok(o) => self.completed(o),
                        Err(reason) => {
                            self.emit(AgentUpdate::StatusUpdated {
                                status: "실행 중단".into(),
                                busy: false,
                            });
                            self.emit(AgentUpdate::EntryUpdated(Entry {
                                id: format!("{}-error", self.turn.as_deref().unwrap_or("core")),
                                kind: Kind::Assistant,
                                title: format!("{} 실행 결과", self.version.label()),
                                body: if self.cancel.load(Ordering::SeqCst) {"작업을 중지했습니다."} else {"작업을 시작하지 못했습니다."}.into(),
                                status: "failed".into(),
                                expanded: false,
                            }));
                            self.emit(AgentUpdate::EntryUpdated(Entry {
                                id: format!(
                                    "{}-startup-error",
                                    self.turn.as_deref().unwrap_or("core")
                                ),
                                kind: Kind::Tool,
                                title: "작업 시작 실패".into(),
                                body: reason,
                                status: "failed".into(),
                                expanded: false,
                            }));
                            let turn_id = self.turn.take();
                            self.emit(AgentUpdate::TurnCompleted {
                                id: turn_id,
                                status: if self.cancel.load(Ordering::SeqCst) {
                                    "interrupted"
                                } else {
                                    "failed"
                                }
                                .into(),
                                error: Some("작업을 시작하지 못했습니다.".into()),
                            });
                        }
                    }
                }
                Message::Approved(result) => {
                    self.active = false;
                    let turn = self
                        .turn
                        .clone()
                        .or_else(|| self.pending.as_ref().map(|o| o.id.clone()))
                        .unwrap_or_else(|| "core".into());
                    if let Some(log) = self.conversation.log(&turn, true) {
                        self.emit(AgentUpdate::EntryUpdated(log));
                    }
                    if let Some(log) = self.conversation.graph_log(&turn) {
                        self.emit(AgentUpdate::EntryUpdated(log));
                    }
                    match result {
                        Ok(o) => {
                            self.pending = None;
                            self.emit(AgentUpdate::ApprovalResolved(o.id.clone()));
                            self.project_task(&o);
                            self.emit(AgentUpdate::EntryUpdated(Entry {
                                id: format!("{turn}-assistant"),
                                kind: Kind::Assistant,
                                title: format!("{} 적용 완료", self.version.label()),
                                body: format!(
                                    "검증된 변경을 적용했습니다.\n\n{}",
                                    Self::report(&o)
                                ),
                                status: "completed".into(),
                                expanded: false,
                            }));
                            self.emit(AgentUpdate::Notice("검증된 변경을 적용했습니다.".into()));
                            let turn_id = self.turn.take();
                            self.emit(AgentUpdate::TurnCompleted {
                                id: turn_id,
                                status: "completed".into(),
                                error: None,
                            });
                        }
                        Err(reason) => {
                            self.emit(AgentUpdate::EntryUpdated(Entry { id:format!("{turn}-assistant"), kind:Kind::Assistant, title:"재검증 중단".into(), body:"변경 적용을 완료하지 못했습니다. 아래 작업 기록에 원인이 남아 있습니다.".into(), status:"failed".into(), expanded:false }));
                            self.emit(AgentUpdate::EntryUpdated(Entry {
                                id: format!("{turn}-approval-error"),
                                kind: Kind::Tool,
                                title: "변경 적용 실패".into(),
                                body: reason,
                                status: "failed".into(),
                                expanded: false,
                            }));
                            let id = self
                                .pending
                                .as_ref()
                                .map(|o| o.id.clone())
                                .unwrap_or_default();
                            if self.cancel.load(Ordering::SeqCst) {
                                core::reject(self.version, &self.root, &id, &self.settings)?;
                                self.pending = None;
                                self.emit(AgentUpdate::ApprovalResolved(id));
                                let completed_turn = self.turn.take();
                                self.emit(AgentUpdate::TurnCompleted {
                                    id: completed_turn,
                                    status: "interrupted".into(),
                                    error: None,
                                });
                            } else {
                                self.emit(AgentUpdate::ApprovalReplyFailed { id, reason:"변경 적용을 완료하지 못했습니다. 작업 기록에서 원인을 확인하세요.".into() });
                            }
                        }
                    }
                    self.emit(AgentUpdate::StatusUpdated {
                        status: "대기".into(),
                        busy: false,
                    });
                }
            }
        }
        if !self.active && !self.inquiry_active {
            if let Some(model) = self.pending_model.take() {
                self.model = model;
                self.refresh_models();
                self.emit(AgentUpdate::Notice("공급자 연결과 모델 목록을 갱신했습니다.".into()));
            }
        }
        Ok(self.events.pop_front())
    }

    fn update_model_policy(&mut self, call_seconds: u64, attempts: usize) {
        self.settings.call_seconds = call_seconds.max(1);
        self.settings.attempts = attempts.clamp(1, 5);
    }
    fn update_execution_budget(&mut self, steps: usize, seconds: u64) {
        self.settings.steps = steps.max(1);
        self.settings.seconds = seconds.max(1);
    }

    fn shutdown(&mut self) -> Result<()> {
        self.cancel.store(true, Ordering::SeqCst);
        self.inquiry_cancel.store(true, Ordering::SeqCst);
        self.stopped = true;
        Ok(())
    }
}

impl VersionBackend {
    fn project_v5(&mut self, event: loom_core::agent::events::Event) {
        use loom_core::agent::events::Stage;
        let turn = self.turn.clone().unwrap_or_else(|| "core".into());
        if event.target == "model" {
            self.v5_projection.observe(&event);
            for entry in self.v5_projection.entries(&turn, false) {
                self.emit(AgentUpdate::EntryUpdated(entry));
            }
            self.emit(AgentUpdate::WorkPhaseUpdated(WorkPhase::Thinking));
            return;
        }
        if event.target == "model_result" {
            self.v5_projection.observe(&event);
            return;
        }
        if event.target == "speed" {
            if let Some(speed) = event.detail["tokens_per_second"].as_f64() {
                self.emit(AgentUpdate::ModelSpeed { tokens_per_second: speed as f32 });
            }
            return;
        }
        if event.target == "stream" {
            self.v5_projection.observe(&event);
            // Only the live progress entry (always last) changes while text streams.
            if let Some(entry) = self.v5_projection.entries(&turn, false).pop() {
                self.emit(AgentUpdate::EntryUpdated(entry));
            }
            self.emit(AgentUpdate::WorkPhaseUpdated(WorkPhase::Writing));
            return;
        }
        if event.target == "commentary" {
            self.v5_projection.observe(&event);
            for entry in self.v5_projection.entries(&turn, false) { self.emit(AgentUpdate::EntryUpdated(entry)); }
            self.emit(AgentUpdate::WorkPhaseUpdated(WorkPhase::Writing));
            return;
        }
        self.v5_projection.observe(&event);
        if let Some(input) = event.detail.get("input_request") {
            if let (Some(id), Some(question)) = (input["id"].as_str(), input["question"].as_str()) {
                self.emit(AgentUpdate::InputRequested(InputRequest {
                    id: id.into(),
                    blocking: true,
                    questions: vec![Question {
                        id: "answer".into(),
                        header: "질문".into(),
                        prompt: question.into(),
                        options: vec![],
                        allow_custom: true,
                        secret: false,
                    }],
                }));
            }
        }
        if event.stage == Stage::Orchestrating {
            if let Some(units) = event.detail["units"].as_array() {
                for unit in units {
                    if let (Some(id), Some(goal)) = (unit["id"].as_str(), unit["goal"].as_str()) {
                        self.emit(AgentUpdate::TaskUpdated(ActivityTask {
                            id: format!("{turn}-{id}"),
                            parent: Some(turn.clone()),
                            title: goal.into(),
                            status: TaskStatus::Pending,
                            dependencies: unit["depends_on"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|d| d.as_str().map(|s| format!("{turn}-{s}")))
                                .collect(),
                            ..Default::default()
                        }));
                    }
                }
            }
        }
        if matches!(
            event.stage,
            Stage::AgentStarted | Stage::AgentFinished | Stage::Merge
        ) {
            let id = format!("{turn}-{}", event.worker);
            self.emit(AgentUpdate::TaskUpdated(ActivityTask {
                id,
                parent: Some(turn.clone()),
                title: event.target.clone(),
                status: if event.failed {
                    if self.cancel.load(Ordering::Acquire) {
                        TaskStatus::Cancelled
                    } else {
                        TaskStatus::Failed
                    }
                } else if event.stage == Stage::AgentFinished {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Running
                },
                dependencies: self
                    .v5_projection
                    .dependencies
                    .get(&event.worker)
                    .into_iter()
                    .flatten()
                    .map(|d| format!("{turn}-{d}"))
                    .collect(),
                reason: Some("에이전트 작업 · 세부 기록을 펼쳐 확인".into()),
                ..Default::default()
            }));
        }
        self.emit(AgentUpdate::StatusUpdated {
            status: if event.failed {
                "확인 필요".into()
            } else {
                self.v5_projection
                    .summary()
                    .lines()
                    .next()
                    .unwrap_or("진행 중")
                    .into()
            },
            busy: true,
        });
        let phase =
            if matches!(event.stage, Stage::Change) {
                WorkPhase::Writing
            } else if event.detail["started"] == true
                && (event.detail["tool"] == "run" || event.stage == Stage::Check) {
                WorkPhase::Command
            } else {
                WorkPhase::Tool
            };
        for entry in self.v5_projection.entries(&turn, false) {
            self.emit(AgentUpdate::EntryUpdated(entry));
        }
        self.emit(AgentUpdate::WorkPhaseUpdated(phase));
    }
}

impl VersionBackend {
    fn v5_logs(outcome: &Outcome) -> Vec<Entry> {
        let path = outcome
            .records
            .with_file_name(format!("{}.events.jsonl", outcome.id));
        let Ok(contents) = std::fs::read_to_string(path) else { return vec![]; };
        let mut projection = super::v5_projection::Projection::default();
        for line in contents.lines() {
            if let Ok(event) = serde_json::from_str(line) {
                projection.observe(&event);
            }
        }
        projection.finish(outcome.success() || outcome.pending(), outcome.cancelled(), &outcome.summary);
        projection
            .entries(&outcome.id, true)
            .into_iter()
            .filter(|e| e.kind == Kind::Tool || e.id.contains("-commentary-"))
            .collect()
    }
    fn inquire(&mut self, question: String) -> Result<()> {
        ensure!(self.active, "/ask는 작업 실행 중에 사용할 수 있습니다");
        ensure!(!self.inquiry_active, "이전 병렬 질문의 답변을 기다려주세요");
        ensure!(!question.trim().is_empty(), "질문을 입력해주세요");
        let live = self
            .settings
            .input_control
            .snapshot()
            .ok_or_else(|| anyhow::anyhow!("World 준비가 끝나면 질문할 수 있습니다"))?;
        if let Some(handle) = self.inquiry_thread.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("inquiry worker panicked"))?;
        }
        let root = self.root.clone();
        let model = self.model.clone();
        let tx = self.tx.clone();
        let mut ids = self
            .get_stored_session(&self.session)
            .map(|s| s.task_ids)
            .unwrap_or_default();
        if !live.task_id.is_empty() && !ids.contains(&live.task_id) {ids.push(live.task_id.clone());}
        let mut history = loom_core::agent::Core::open(&root)?.context(&ids, 30000)?;
        self.sequence += 1;
        let id = format!(
            "{}-inquiry-{}",
            self.turn.as_deref().unwrap_or("core"),
            self.sequence
        );
        self.emit(AgentUpdate::EntryUpdated(Entry {
            id: format!("{id}-user"),
            kind: Kind::User,
            title: "병렬 질문".into(),
            body: format!("병렬 질문 · {question}"),
            status: "completed".into(),
            expanded: false,
        }));
        self.emit(AgentUpdate::EntryUpdated(Entry {
            id: format!("{id}-answer"),
            kind: Kind::Assistant,
            title: "병렬 질문".into(),
            body: "병렬 질문 · 현재 작업 상태를 확인 중…".into(),
            status: "inProgress".into(),
            expanded: false,
        }));
        let context=format!("Core inquiry snapshot: main objective = {}; observed stage = {}. Main execution continues independently. This question is read-only and cannot declare the main task complete.",live.goal,live.stage);
        if let Some(last)=history.last_mut() {last.parts.push(models::Part::Text(context));}
        else {history.push(models::Message::user(context));}
        let settings = self.settings.clone();
        self.inquiry_active = true;
        self.inquiry_cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.inquiry_cancel.clone();
        self.inquiry_thread = Some(thread::spawn(move || {
            let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut core=loom_core::agent::Core::open(&root)?;
                core.execute(&question,loom_core::agent::Intent::Inspect,loom_core::agent::Options{
                    history,pinned:Some(live.revision),inquiry:true,read_only:true,steps:8,seconds:60,tokens:40000,call_seconds:settings.call_seconds.min(30),validation_reserve:0,
                    model:(settings.model!="auto").then(||settings.model.clone()),reasoning_effort:settings.effort,policy:settings.policy,..Default::default()
                },model.as_ref(),cancel,&|_|{})
            })).map_err(|_|"parallel question worker panicked".into()).and_then(|r:Result<loom_core::agent::Task>|r.map_err(|e|e.to_string()));
            let _ = tx.send(Message::InquiryDone { id, result });
        }));
        Ok(())
    }
}

impl Drop for VersionBackend {
    fn drop(&mut self) {
        self.cancel.store(true,Ordering::SeqCst);
        self.inquiry_cancel.store(true,Ordering::SeqCst);
    }
}

#[cfg(test)]
mod provider_reload_tests {
    use super::*;
    use std::time::Duration;
    fn model(id: &str) -> Arc<dyn ModelPort> {
        Arc::new(models::ScriptedModel::with_candidate(models::router::Candidate {
            endpoint: "fixture".into(), model: id.into(), tier: models::Tier::Small,
            cost: models::CostClass::Local, price: Some((0.0, 0.0)), tools: true, context: None,
        }, std::iter::empty()))
    }
    #[test]
    fn initial_model_waits_for_discovery_and_can_be_cancelled() {
        let loading = Arc::new(LoadingModel::default());
        let request = models::Request::default();
        let completed = loading.clone();
        let worker = thread::spawn(move || completed.complete(models::Need::default(), &request));
        thread::sleep(Duration::from_millis(20));
        assert!(!worker.is_finished(), "the model must wait for initial discovery");
        loading.resolve(Ok(model("ready")));
        assert!(worker.join().unwrap().is_err(), "fixture has no scripted response, but should unblock");

        let loading = LoadingModel::default();
        let cancelled = Arc::new(AtomicBool::new(true));
        let request = models::Request { cancel: Some(cancelled), ..Default::default() };
        let error = loading.complete(models::Need::default(), &request).unwrap_err();
        assert_eq!(error.kind, models::ErrorKind::Cancelled);
    }
    #[test]
    fn requesting_another_reload_invalidates_a_waiting_old_model() -> Result<()> {
        let root = tempfile::tempdir()?;
        let before = model("before");
        let mut backend = VersionBackend::with_model(Version::Loom, root.path(), before.clone(), Settings::default())?;
        backend.pending_model = Some(model("disabled-old-provider"));
        backend.reload_providers()?;
        backend.poll()?;
        assert!(backend.pending_model.is_none());
        assert!(Arc::ptr_eq(&backend.model, &before));
        Ok(())
    }
    #[test]
    fn connection_reload_waits_for_work_and_ignores_stale_discovery() -> Result<()> {
        let root = tempfile::tempdir()?;
        let before = model("before");
        let after = model("after");
        let mut backend = VersionBackend::with_model(Version::Loom, root.path(), before.clone(), Settings::default())?;
        while backend.poll()?.is_some() {}
        backend.active = true;
        backend.reload_generation = 2;
        backend.tx.send(Message::ModelsReady { generation: 1, result: Ok(model("stale")) })?;
        backend.tx.send(Message::ModelsReady { generation: 2, result: Ok(after.clone()) })?;
        while backend.poll()?.is_some() {}
        assert!(Arc::ptr_eq(&backend.model, &before));
        assert!(backend.pending_model.is_some());
        backend.active = false;
        backend.inquiry_active = true;
        backend.poll()?;
        assert!(Arc::ptr_eq(&backend.model, &before));
        backend.inquiry_active = false;
        backend.poll()?;
        assert!(Arc::ptr_eq(&backend.model, &after));
        assert!(backend.pending_model.is_none());
        Ok(())
    }
}
