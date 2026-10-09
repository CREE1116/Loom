//! Native surface: asynchronous Core execution and authoritative UI projections.
use crate::agent::*;
use super::versions::display_progress_summary;
use anyhow::{Context, Result, bail};
use loom_core::{
    base::{ArtifactStore, TaskId},
    control::{Core, CoreEvent, ExecutionOptions, Task, TaskIntent, TaskStatus as Status},
    effects::Patch,
    models::{ModelPort, Tier},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

enum Message {
    Event(CoreEvent),
    Done(Result<Task, String>),
    ChatDone { id: String, result: Result<ChatAnswer, String> },
}

struct ChatAnswer {
    text: String,
    model: String,
    endpoint: String,
    usage: loom_core::models::Usage,
}

/// Only an explicit command selects the optional tool-free conversation path.
/// Never infer execution mode from greeting or question keywords.
fn chat_input(text: &str, has_images: bool) -> Option<String> {
    if has_images { return None; }
    text.strip_prefix(":chat ")
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_owned())
}

fn task_input(text: &str, default_intent: TaskIntent) -> (String, TaskIntent) {
    if let Some(goal) = text.strip_prefix(":inspect ").filter(|goal| !goal.trim().is_empty()) {
        return (goal.trim().to_owned(), TaskIntent::Inspect);
    }
    if let Some(goal) = text.strip_prefix(":edit ").filter(|goal| !goal.trim().is_empty()) {
        return (goal.trim().to_owned(), TaskIntent::Edit);
    }
    (text.to_owned(), default_intent)
}

pub(crate) const STARTUP_NOTICE: &str = "Native: 기본 입력=자동 · :edit 편집 · :inspect 조회 · :chat 대화";
#[derive(Default)]
struct ChildActivity {
    status: String,
    entries: Vec<Entry>,
}
pub struct NativeBackend {
    root: PathBuf,
    model: Arc<dyn ModelPort>,
    options: ExecutionOptions,
    default_intent: TaskIntent,
    session: String,
    events: VecDeque<AgentEvent>,
    tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Message>,
    cancel: Arc<AtomicBool>,
    active: bool,
    chat_active: bool,
    chat_interrupt_notified: bool,
    stopped: bool,
    pending: Option<TaskId>,
    /// In-flight, one-task web access approvals (separate from patch approval).
    pending_web: BTreeSet<String>,
    /// Temporary UI entry for a submitted edit while Core builds its task.
    pending_echo_id: Option<String>,
    /// Last assistant text shown for the running task; the final summary repeats it
    /// when the model answered in prose, and must not be shown twice.
    last_text: Option<String>,
    child_activities: BTreeMap<String, ChildActivity>,
    current: Option<TaskId>,
    thread: Option<thread::JoinHandle<()>>,
    input: u64,
    output: u64,
    cached: u64,
    approval: String,
    trusted: bool,
}
impl NativeBackend {
    pub fn new(root: &Path, script: Option<&Path>, options: ExecutionOptions) -> Result<Self> {
        Self::with_model(root, loom_core::surfaces::model(script)?, options)
    }
    pub fn with_model(
        root: &Path,
        model: Arc<dyn ModelPort>,
        mut options: ExecutionOptions,
    ) -> Result<Self> {
        options.approve = false;
        // Preserve Core's Auto contract: questions may finish with an answer,
        // while actual patches still require independent validation and approval.
        // Explicit :edit or a configured Edit intent retains the patch requirement.
        let default_intent = options.intent;
        let root = root.canonicalize()?;
        let sessions = loom_core::sessions::Sessions::open(&root)?;
        let session = options
            .session
            .clone()
            .or_else(|| std::fs::read_to_string(root.join(".loom/native-last-session")).ok())
            .unwrap_or_else(|| "native".into());
        sessions.ensure(&session)?;
        options.session = Some(session.clone());
        let (tx, rx) = mpsc::channel();
        let settings: serde_json::Value = std::fs::read(root.join(".loom/native-settings.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let approval = settings["approval"]
            .as_str()
            .unwrap_or("on-request")
            .to_owned();
        let trusted = settings["trusted"].as_bool().unwrap_or(true);
        if let Some(policy) = settings.get("policy") {
            options.policy = serde_json::from_value(policy.clone())?;
        }
        if options.readonly || !trusted {
            options.policy.sandbox = loom_core::effects::SandboxMode::ReadOnly;
        }
        let mut backend = Self {
            root,
            model,
            options,
            default_intent,
            session: session.clone(),
            events: VecDeque::new(),
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            active: false,
            chat_active: false,
            chat_interrupt_notified: false,
            stopped: false,
            pending: None,
            pending_web: BTreeSet::new(),
            pending_echo_id: None,
            last_text: None,
            child_activities: BTreeMap::new(),
            current: None,
            thread: None,
            input: 0,
            output: 0,
            cached: 0,
            approval,
            trusted,
        };
        backend.load_session(&session)?;
        backend.refresh_models();
        backend.emit(AgentUpdate::Notice(STARTUP_NOTICE.into()));
        Ok(backend)
    }
    fn permissions(&self) -> Permissions {
        Permissions {
            approval: Some(self.approval.clone()),
            sandbox: Some(
                match self.options.policy.sandbox {
                    loom_core::effects::SandboxMode::ReadOnly => "readOnly",
                    loom_core::effects::SandboxMode::WorkspaceWrite => "workspaceWrite",
                    loom_core::effects::SandboxMode::FullAccess => "dangerFullAccess",
                }
                .into(),
            ),
            network: Some(if self.options.policy.network { "allow" } else { "deny" }.into()),
        }
    }
    fn save_settings(&self) -> Result<()> {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(self.root.join(".loom"))?;
        file.write_all(serde_json::to_string(&serde_json::json!({"approval":self.approval,"trusted":self.trusted,"policy":self.options.policy}))?.as_bytes())?;
        file.as_file().sync_all()?;
        file.persist(self.root.join(".loom/native-settings.json"))?;
        Ok(())
    }
    fn load_session(&mut self, id: &str) -> Result<()> {
        if self.active {
            bail!("finish or interrupt the active task before switching sessions");
        }
        let session = loom_core::sessions::Sessions::open(&self.root)?.ensure(id)?;
        self.session = id.into();
        self.options.session = Some(id.into());
        self.pending = None;
        self.current = None;
        std::fs::write(self.root.join(".loom/native-last-session"), id)?;
        let core = Core::open(&self.root)?;
        let tasks = core
            .state
            .tasks
            .values()
            .filter(|t| t.session.as_deref() == Some(id) || (id == "native" && t.session.is_none()))
            .cloned()
            .collect::<Vec<_>>();
        let mut turns = session
            .turns
            .iter()
            .map(|turn| {
                turn_snapshot(
                    &turn.task.0,
                    &turn.goal,
                    &turn.summary,
                    turn.status,
                )
            })
            .collect::<Vec<_>>();
        for task in &tasks {
            if !turns
                .iter()
                .any(|turn| turn.id.as_deref() == Some(&task.id.0))
            {
                turns.push(turn_snapshot(
                    &task.id.0,
                    &task.goal,
                    &task.summary,
                    task.status,
                ));
            }
        }
        self.events
            .push_back(AgentEvent::global(AgentUpdate::SessionLoaded {
                snapshot: SessionSnapshot {
                    id: id.into(),
                    title: Some(session.title),
                    status: Some("idle".into()),
                    turns: Some(turns),
                },
                switching: true,
                model: Some(self.options.model.clone().unwrap_or_else(|| "auto".into())),
                permissions: self.permissions(),
            }));
        self.emit_goal()?;
        for task in tasks {
            self.project(&task)?;
        }
        Ok(())
    }
    fn emit_goal(&mut self) -> Result<()> {
        let goal = loom_core::sessions::Sessions::open(&self.root)?
            .get(&self.session)?
            .goal
            .map(|g| GoalState {
                objective: g.objective,
                status: g.status,
                tokens_used: Some(g.tokens_used),
                token_budget: g.token_budget,
            });
        self.emit(AgentUpdate::GoalUpdated(goal));
        Ok(())
    }
    fn refresh_models(&mut self) {
        let profiles = self.model.model_profiles();
        let mut names = vec!["auto".into()];
        names.extend(
            profiles
                .iter()
                .filter(|p| p.candidate.cost <= loom_core::models::cost_ceiling())
                .map(|p| format!("{}/{}", p.candidate.endpoint, p.candidate.model)),
        );
        self.events
            .push_back(AgentEvent::global(AgentUpdate::ModelsLoaded(names.clone())));
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
        let ui_profiles = names
            .into_iter()
            .map(|id| {
                let actual = profiles
                    .iter()
                    .find(|p| format!("{}/{}", p.candidate.endpoint, p.candidate.model) == id);
                ModelProfile {
                    id,
                    efforts: actual
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
                    default_effort: actual.and_then(|p| p.default_effort.clone()),
                }
            })
            .collect();
        self.events
            .push_back(AgentEvent::global(AgentUpdate::ModelProfilesLoaded(
                ui_profiles,
            )));
    }

    fn emit(&mut self, update: AgentUpdate) {
        if let AgentUpdate::EntryUpdated(entry) = &update {
            for (id, activity) in &mut self.child_activities {
                if entry.id.starts_with(&format!("{id}-")) {
                    if let Some(old) = activity.entries.iter_mut().find(|old|old.id==entry.id) { *old=entry.clone(); }
                    else {activity.entries.push(entry.clone());}
                    if activity.entries.len()>80 {activity.entries.remove(0);}
                }
            }
        }
        self.events
            .push_back(AgentEvent::scoped(self.session.clone(), update));
    }
    fn restore_child_activity(&mut self, id: &str) -> Result<()> {
        let Some((root,_))=id.split_once("-child-") else {return Ok(());};
        let core=Core::open(&self.root)?;
        let Some(task)=core.state.tasks.get(&TaskId(root.into())) else {return Ok(());};
        let store=loom_core::base::FileArtifactStore::open(&self.root)?;
        let mut activity=ChildActivity {status:"unknown".into(),entries:vec![]};
        let mut found=false;
        for attempt in &task.attempts {
            let result:loom_core::worker::WorkerResult=serde_json::from_slice(&store.get(&attempt.result,None)?)?;
            let trace=store.get(&result.trace,None)?;
            for line in trace.split(|byte|*byte==b'\n').filter(|line|!line.is_empty()) {
                let mut event:serde_json::Value=serde_json::from_slice(line)?;
                let mut event_id=root.to_owned();
                while let Some(child)=event["child"].as_str() {
                    event_id.push('-');event_id.push_str(child);
                    event=event["event"].clone();
                }
                if event_id!=id {continue;}
                found=true;
                if let Some(state)=event["agent_state"].as_str() {
                    activity.status=match state {"merged"=>"completed","proposed"=>"needs_review",other=>other}.into();
                }
                let entry=if let Some(tool)=event["tool"].as_str() {
                    let description = display_progress_summary(tool, event["description"].as_str().unwrap_or(tool));
                    let result_summary = display_progress_summary(tool, event["result_summary"].as_str().unwrap_or("Result"));
                    Some(Entry {id:format!("{id}-tool-{}",event["operation_id"].as_str().unwrap_or(tool)),kind:Kind::Tool,
                        title:format!("{description} · {result_summary}"),
                        body:event["result"].as_str().unwrap_or("").into(),status:if event["error"]==true {"failed"} else {"completed"}.into(),expanded:false})
                } else {event["text"].as_str().filter(|text|!text.is_empty()).map(|text|Entry {id:format!("{id}-response-{}",event["step"]),kind:Kind::Assistant,title:"모델 응답".into(),body:text.into(),status:"completed".into(),expanded:false})};
                if let Some(entry)=entry {activity.entries.push(entry);if activity.entries.len()>80 {activity.entries.remove(0);}}
            }
        }
        if found {self.child_activities.insert(id.into(),activity);}
        Ok(())
    }
    fn project(&mut self, task: &Task) -> Result<()> {
        let worker = task
            .active_tier
            .or_else(|| task.attempts.last().map(|a| a.tier))
            .map(|tier| match tier {
                Tier::Local => WorkerKind::Local,
                Tier::Small => WorkerKind::Small,
                Tier::Medium => WorkerKind::Medium,
                Tier::Flagship => WorkerKind::Flagship,
            });
        // A task restored as Planned/Running/Validating whose process ended (window
        // closed, crash) is not running: show it as interrupted, not as working forever.
        let stale = matches!(task.status, Status::Planned | Status::Running | Status::Validating)
            && !(self.active && self.current.as_ref() == Some(&task.id))
            && !Core::open(&self.root).is_ok_and(|core| core.is_executing(&task.id));
        let status = match task.status {
            _ if stale => TaskStatus::Blocked,
            Status::Planned => TaskStatus::Ready,
            Status::Running | Status::Validating => TaskStatus::Running,
            Status::AwaitingApproval => TaskStatus::NeedsReview,
            Status::Accepted => TaskStatus::Completed,
            Status::Cancelled => TaskStatus::Cancelled,
            Status::Unresolved => TaskStatus::Blocked,
            Status::ValidationFailed | Status::ScopeConflict => TaskStatus::Failed,
        };
        let result = loom_core::surfaces::outcome(task);
        let files = task
            .scope
            .as_ref()
            .map(|s| {
                s.revised
                    .write_files
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect()
            })
            .unwrap_or_default();
        self.emit(AgentUpdate::TaskUpdated(ActivityTask {
            id: task.id.to_string(),
            title: task.goal.clone(),
            worker,
            status,
            reason: Some(if stale {
                format!("중단됨: 실행하던 Loom이 종료됨 · resume {} 로 이어서 실행", task.id)
            } else {
                format!("{:?} · {}", task.status, task.decision.reason)
            }),
            critical: true,
            session: Some(self.session.clone()),
            partition: Some(Box::new(TaskPartition {
                mode: match task.decision.mode {
                    loom_core::control::ExecutionMode::Single => ExecutionMode::Single,
                    loom_core::control::ExecutionMode::Split => ExecutionMode::Split,
                },
                reason: task.decision.reason.clone(),
                files,
                symbols: task
                    .contract
                    .as_ref()
                    .map(|c| c.anchor.iter().map(ToString::to_string).collect())
                    .unwrap_or_default(),
                groups: task.decision.groups.iter().map(|group|group.anchors.clone()).collect(),
            })),
            cost: Some(TaskCost {
                orient_bytes: result["phase_bytes"]["orient"].as_u64().unwrap_or(0),
                validate_bytes: result["phase_bytes"]["validate"].as_u64().unwrap_or(0),
                edit_bytes: result["phase_bytes"]["edit"].as_u64().unwrap_or(0),
                message_bytes: result["phase_bytes"]["message"].as_u64().unwrap_or(0),
                input_tokens: Some(result["input_tokens"].as_u64().unwrap_or(0)),
                cached_input_tokens: Some(result["cached_input_tokens"].as_u64().unwrap_or(0)),
                output_tokens: Some(result["output_tokens"].as_u64().unwrap_or(0)),
                model_calls: task
                    .attempts
                    .iter()
                    .fold(0u32, |sum, attempt|sum.saturating_add(u32::try_from(attempt.steps).unwrap_or(u32::MAX))),
            }),
            ..Default::default()
        }));
        if task.status == Status::AwaitingApproval && !self.active && self.pending.is_none() {
            self.pending = Some(task.id.clone());
            self.current = Some(task.id.clone());
            let core = Core::open(&self.root)?;
            let patch: Patch = serde_json::from_slice(
                &core
                    .artifacts()
                    .get(task.patch.as_ref().context("missing patch")?, None)?,
            )?;
            self.emit(AgentUpdate::DiffUpdated(
                String::from_utf8_lossy(&patch.diff).into_owned(),
            ));
            self.emit(AgentUpdate::ApprovalRequested(Approval {
                id: task.id.to_string(),
                title: "검증된 Native 변경 승인".into(),
                summary: task.summary.clone(),
                detail: format!(
                    "{}\n파일: {}\nScope: {:?}",
                    String::from_utf8_lossy(&patch.diff),
                    patch.files.len(),
                    task.scope.as_ref().map(|s| &s.events)
                ),
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
        }
        Ok(())
    }
    fn launch(&mut self, goal: Option<String>, approval: Option<(TaskId, bool)>) -> Result<()> {
        if self.active {
            bail!("Native task is already running");
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(text) = goal.as_ref() {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?.as_nanos();
            let echo_id = format!("native-echo-{nonce:x}-{}", std::process::id());
            self.pending_echo_id = Some(echo_id.clone());
            self.emit(AgentUpdate::TurnSubmitted { id: None, status: "inProgress".into() });
            self.emit(AgentUpdate::EntryUpdated(Entry {
                id: echo_id, kind: Kind::User, title: String::new(),
                body: text.clone(), status: "completed".into(), expanded: false,
            }));
        }
        self.active = true;
        self.chat_active = false;
        self.cancel = Arc::new(AtomicBool::new(false));
        let root = self.root.clone();
        let model = Arc::clone(&self.model);
        let options = self.options.clone();
        let cancel = Arc::clone(&self.cancel);
        let tx = self.tx.clone();
        // Closing the window cancels the task, so Core stops the worker and kills
        // validation process groups before the process exits.
        let watched = Arc::clone(&self.cancel);
        let finished = Arc::new(AtomicBool::new(false));
        let done = Arc::clone(&finished);
        thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                if crate::runtime::SHUTDOWN.load(Ordering::SeqCst) {
                    watched.store(true, Ordering::SeqCst);
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        self.thread = Some(thread::spawn(move || {
            let _finished = scopeguard(finished);
            let result = (|| -> Result<Task> {
                let mut core = Core::open(&root)?;
                let observe = |event| {
                    let _ = tx.send(Message::Event(event));
                };
                if let Some((id, accept)) = approval {
                    if accept {
                        core.approve(&id, &cancel, observe)
                    } else {
                        core.reject(&id, observe)
                    }
                } else {
                    core.execute(
                        &goal.context("goal missing")?,
                        options,
                        model.as_ref(),
                        &cancel,
                        observe,
                    )
                }
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::Done(result));
        }));
        Ok(())
    }
    fn launch_chat(&mut self, text: String) -> Result<()> {
        if self.active { bail!("Native task is already running"); }
        if let Some(thread) = self.thread.take() { thread.join().map_err(|_| anyhow::anyhow!("previous Native task panicked"))?; }
        let id = format!("chat-{:x}-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?.as_nanos(), std::process::id());
        let root = self.root.clone();
        let session = self.session.clone();
        let model = self.model.clone();
        let options = self.options.clone();
        let tx = self.tx.clone();
        self.active = true;
        self.chat_active = true;
        self.chat_interrupt_notified = false;
        self.current = Some(TaskId(id.clone()));
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        self.emit(AgentUpdate::TurnSubmitted { id: Some(id.clone()), status: "inProgress".into() });
        self.emit(AgentUpdate::TurnStarted { id: Some(id.clone()) });
        self.emit(AgentUpdate::EntryUpdated(Entry {
            id: format!("{id}-user"), kind: Kind::User, title: String::new(),
            body: text.clone(), status: "completed".into(), expanded: false,
        }));
        self.thread = Some(thread::spawn(move || {
            let answer = (|| -> Result<ChatAnswer> {
                use loom_core::models::{Message as ModelMessage, Request, cost_ceiling};
                let sessions = loom_core::sessions::Sessions::open(&root)?;
                let recent = sessions.get(&session)?.turns.into_iter().rev().take(5)
                    .collect::<Vec<_>>();
                let mut messages = Vec::new();
                for turn in recent.into_iter().rev() {
                    messages.push(ModelMessage::user(format!("User: {}\nAssistant: {}", turn.goal, turn.summary)));
                }
                messages.push(ModelMessage::user(text.clone()));
                let request = Request {
                    model: options.model.clone().unwrap_or_default(),
                    system: Some("You are Loom's conversational assistant. Answer the user's message directly, without coding tools.".into()),
                    messages, tools: vec![], max_output_tokens: Some(2048),
                    reasoning_effort: options.reasoning_effort.clone(),
                    cancel: Some(cancel.clone()), timeout: Some(std::time::Duration::from_secs(30)),
                    ..Request::default()
                };
                let mut need = options.need;
                need.tools = false;
                need.max_cost = need.max_cost.min(cost_ceiling());
                let served = model.complete_stream(need, &request, &mut |_| {})?;
                if cancel.load(Ordering::SeqCst) {
                    bail!("cancelled");
                }
                anyhow::ensure!(served.candidate.cost <= need.max_cost && served.candidate.cost <= cost_ceiling(),
                    "chat model exceeded cost ceiling");
                anyhow::ensure!(served.response.tool_calls().next().is_none(), "chat model unexpectedly called a tool");
                let answer = served.response.text();
                anyhow::ensure!(!answer.trim().is_empty(), "chat model produced no response");
                let revision = loom_core::base::Revision::capture(&root)?;
                let tokens = served.response.usage.input.unwrap_or(0) + served.response.usage.output.unwrap_or(0);
                sessions.update(&session, |s| {
                    if s.turns.is_empty() { s.title = text.chars().take(70).collect(); }
                    s.turns.push(loom_core::sessions::Turn {
                        task: TaskId(id.clone()), goal: text.clone(), summary: answer.clone(),
                        status: Status::Accepted, before: revision.clone(), after: Some(revision),
                        evidence: vec![], tokens,
                    });
                    if let Some(goal) = s.goal.as_mut() { goal.tokens_used = goal.tokens_used.saturating_add(tokens); }
                    Ok(())
                })?;
                Ok(ChatAnswer {
                    text: answer, model: served.response.model,
                    endpoint: served.candidate.endpoint, usage: served.response.usage,
                })
            })().map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::ChatDone { id, result: answer });
        }));
        Ok(())
    }
    fn receive(&mut self, message: Message) -> Result<()> {
        match message {
            Message::ChatDone { id, result } => {
                self.active = false;
                self.chat_active = false;
                self.current = None;
                if std::mem::take(&mut self.chat_interrupt_notified) {
                    // The UI already received its terminal event at interrupt.
                    // Never publish a late provider result or a second terminal.
                    return Ok(());
                }
                match result {
                    Ok(answer) => {
                        self.emit(AgentUpdate::ModelResolved { requested: self.options.model.clone().unwrap_or_else(|| "auto".into()), provider: answer.endpoint.clone(), model: answer.model.clone() });
                        self.input += answer.usage.input.unwrap_or(0);
                        self.cached += answer.usage.cached_input.unwrap_or(0);
                        self.output += answer.usage.output.unwrap_or(0);
                        self.emit(AgentUpdate::TokenUsageUpdated(TokenUsage {
                            input: answer.usage.input.map(|_| self.input),
                            cached_input: answer.usage.cached_input.map(|_| self.cached),
                            output: answer.usage.output.map(|_| self.output),
                            total: answer.usage.input.zip(answer.usage.output).map(|_| self.input + self.output),
                            ..Default::default()
                        }));
                        self.emit(AgentUpdate::EntryUpdated(Entry {
                            id: format!("{id}-assistant"), kind: Kind::Assistant,
                            title: format!("모델 · {}/{}", answer.endpoint, answer.model),
                            body: answer.text, status: "completed".into(), expanded: false,
                        }));
                        self.emit_goal()?;
                        self.emit(AgentUpdate::TurnCompleted { id: Some(id), status: "completed".into(), error: None });
                    }
                    Err(reason) => {
                        let cancelled = self.cancel.load(Ordering::SeqCst);
                        self.emit(AgentUpdate::TurnCompleted {
                            id: Some(id), status: if cancelled { "interrupted" } else { "failed" }.into(),
                            error: (!cancelled).then_some(reason),
                        });
                    }
                }
            }

            Message::Event(CoreEvent::TaskSaved(task)) => {
                if task.status == Status::Planned {
                    self.current = Some(task.id.clone());
                    self.emit(AgentUpdate::TurnStarted {
                        id: Some(task.id.to_string()),
                    });
                    if self.pending_echo_id.take().is_none() {
                        self.emit(AgentUpdate::EntryUpdated(Entry {
                            id: format!("{}-user", task.id),
                            kind: Kind::User, title: String::new(), body: task.goal.clone(),
                            status: "completed".into(), expanded: false,
                        }));
                    }
                }
                self.project(&task)?;
            }
            Message::Event(CoreEvent::WorkerEvent { task, event }) => {
                if event.get("model_started").is_some() {
                    self.emit(AgentUpdate::WorkPhaseUpdated(WorkPhase::Thinking));
                }
                if let Some(message) = event["progress"]["message"].as_str() {
                    self.emit(AgentUpdate::EntryUpdated(Entry {
                        id: format!("{}-{}", task, event["progress"]["id"].as_str().unwrap_or("progress")),
                        kind: Kind::Assistant, title: "Core 진행".into(), body: message.into(),
                        status: "completed".into(), expanded: false,
                    }));
                }
                if let Some(start) = event.get("tool_started") {
                    self.emit(AgentUpdate::EntryUpdated(Entry {
                        id: format!("{}-tool-{}", task, start["operation_id"].as_str().unwrap_or("pending")),
                        kind: Kind::Tool,
                        title: start["description"].as_str().unwrap_or("도구 실행").into(),
                        body: "실행 중 · 결과를 기다리고 있습니다.".into(),
                        status: "inProgress".into(), expanded: false,
                    }));
                    self.emit(AgentUpdate::WorkPhaseUpdated(if matches!(start["tool"].as_str(), Some("exec_command" | "write_stdin")) { WorkPhase::Shell } else { WorkPhase::Tool }));
                }
                if let Some(request) = event.get("network_approval_request") {
                    let id = request["id"].as_str().context("invalid web approval id")?.to_owned();
                    if self.pending_web.insert(id.clone()) {
                        let tool = request["tool"].as_str().unwrap_or("web tool");
                        let target = request["target"].as_str().unwrap_or("unknown");
                        let target: String = target.chars().filter(|c| !c.is_control()).take(240).collect();
                        self.emit(AgentUpdate::ApprovalRequested(Approval {
                            id,
                            title: "Native 웹 접근 권한 요청".into(),
                            summary: format!("{tool}: {target}"),
                            detail: "이 작업이 진행되는 동안 웹 검색·페이지 가져오기를 허용할까요? 터미널 네트워크 권한이나 다음 작업의 설정은 변경되지 않습니다.".into(),
                            choices: vec![
                                Choice { label: "이 작업에서 웹 접근 허용".into(), result: "accept".into() },
                                Choice { label: "거부".into(), result: "reject".into() },
                            ],
                            answering: false,
                            expanded: false,
                        }));
                    }
                }
                if let Some(id) = event["network_approval_resolved"]["id"].as_str()
                    && self.pending_web.remove(id) {
                        self.emit(AgentUpdate::ApprovalResolved(id.to_owned()));
                }
                if let Some(model) = event["model"].as_str() {
                    if event.get("child").is_none() && self.current.as_ref() == Some(&task) {
                        self.emit(AgentUpdate::ModelResolved {
                            requested: self.options.model.clone().unwrap_or_else(|| "auto".into()),
                            provider: event["route"].as_str().or(event["endpoint"].as_str()).unwrap_or("—").into(),
                            model: model.into(),
                        });
                    }
                }
                if let Some(request) = event.get("input_request") {
                    let questions = request["questions"]
                        .as_array()
                        .context("invalid input request")?
                        .iter()
                        .enumerate()
                        .map(|(index, q)| Question {
                            id: q["id"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("q-{index}")),
                            header: q["header"].as_str().unwrap_or("질문").into(),
                            prompt: q["question"].as_str().unwrap_or("").into(),
                            options: q["options"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|option| QuestionOption {
                                    label: option.as_str().unwrap_or("").into(),
                                    description: String::new(),
                                })
                                .collect(),
                            allow_custom: true,
                            secret: false,
                        })
                        .collect();
                    self.emit(AgentUpdate::InputRequested(InputRequest {
                        id: request["id"].as_str().unwrap_or("").into(),
                        questions,
                        blocking: true,
                    }));
                }
                if let Some(id) = event["input_resolved"].as_str() {
                    self.emit(AgentUpdate::InputResolved(id.into()));
                }
                if let Some(delta) = event["text_delta"].as_str() {
                    self.emit(AgentUpdate::WorkPhaseUpdated(WorkPhase::Writing));
                    self.emit(AgentUpdate::EntryDelta {
                        id: format!("{}-stream-{}", task, event["step"]),
                        kind: Kind::Assistant,
                        text: delta.into(),
                    });
                } else if let Some(text) = event["text"].as_str().filter(|s| !s.is_empty()) {
                    if self.current.as_ref()==Some(&task) {self.last_text = Some(text.trim().to_owned());}
                    self.emit(AgentUpdate::EntryUpdated(Entry {
                        id: format!(
                            "{}-{}-{}",
                            task,
                            if event["streamed"] == true {
                                "stream"
                            } else {
                                "response"
                            },
                            event["step"]
                        ),
                        kind: Kind::Assistant,
                        title: event["model"].as_str().map(|model| format!("모델 · {}/{}", event["endpoint"].as_str().unwrap_or("—"), model)).unwrap_or_default(),
                        body: text.into(),
                        status: "completed".into(),
                        expanded: false,
                    }));
                }
                if let Some(id) = event["child"].as_str() {
                    let child_id=format!("{task}-{id}");
                    let activity=self.child_activities.entry(child_id.clone()).or_default();
                    if let Some(state)=event["event"]["agent_state"].as_str() {
                        activity.status=match state {"merged"=>"completed","proposed"=>"needs_review",other=>other}.into();
                    } else if activity.status.is_empty() {activity.status="running".into();}
                    let status=activity.status.clone();
                    let detail=event["event"]["goal"].as_str().or(event["event"]["summary"].as_str()).unwrap_or("격리 사본에서 작업 중").to_owned();
                    self.emit(AgentUpdate::ActivityUpdated(Agent {
                        id: child_id.clone(),
                        name: id.into(),
                        status: status.clone(),
                        detail: detail.clone(),
                    }));
                    self.emit(AgentUpdate::TaskUpdated(ActivityTask {
                        id:child_id.clone(),parent:Some(task.to_string()),title:detail,
                        status:match status.as_str() {"completed"=>TaskStatus::Completed,"cancelled"=>TaskStatus::Cancelled,"failed"=>TaskStatus::Failed,"needs_review"=>TaskStatus::NeedsReview,_=>TaskStatus::Running},
                        session:Some(self.session.clone()),..Default::default()
                    }));
                    return self.receive(Message::Event(CoreEvent::WorkerEvent {
                        task: TaskId(child_id),
                        event: event["event"].clone(),
                    }));
                }
                if event.get("usage").is_some() {
                    let input = event["usage"]["input"].as_u64();
                    let output = event["usage"]["output"].as_u64();
                    let cached = event["usage"]["cached_input"].as_u64();
                    self.input += input.unwrap_or(0);
                    self.output += output.unwrap_or(0);
                    self.cached += cached.unwrap_or(0);
                    self.emit(AgentUpdate::TokenUsageUpdated(TokenUsage {
                        input: input.map(|_| self.input),
                        output: output.map(|_| self.output),
                        cached_input: cached.map(|_| self.cached),
                        total: input.zip(output).map(|_| self.input + self.output),
                        ..Default::default()
                    }));
                }
                if let Some(tool) = event["tool"].as_str() {
                    let description = event["description"]
                        .as_str()
                        .map(|description| display_progress_summary(tool, description));
                    let result_summary = display_progress_summary(
                        tool,
                        event["result_summary"].as_str().unwrap_or("Result"),
                    );
                    self.emit(AgentUpdate::WorkPhaseUpdated(WorkPhase::Tool));
                    self.emit(AgentUpdate::EntryUpdated(Entry {
                        id: event["operation_id"].as_str().map(|id|format!("{task}-tool-{id}"))
                            .unwrap_or_else(||format!("{}-{}-{}", task, event["step"], tool)),
                        kind: Kind::Tool,
                        title: description
                            .map(|description| format!("{description} · {result_summary}"))
                            .unwrap_or_else(||tool.into()),
                        body: event["result"].as_str().unwrap_or("").into(),
                        status: if event["error"] == true {
                            "failed"
                        } else {
                            "completed"
                        }
                        .into(),
                        expanded: false,
                    }));
                }
            }
            Message::Event(CoreEvent::RevisionUpdated(_)) => {}
            Message::Done(result) => {
                self.active = false;
                self.pending_echo_id = None;
                for id in std::mem::take(&mut self.pending_web) {
                    self.emit(AgentUpdate::ApprovalResolved(id));
                }
                match result {
                    Ok(task) => {
                        if task.status == Status::AwaitingApproval {
                            if self.cancel.load(Ordering::SeqCst) {
                                return self.launch(None, Some((task.id.clone(), false)));
                            }
                            self.project(&task)?;
                            return Ok(());
                        }
                        if let Some(pending) = self.pending.take() {
                            self.emit(AgentUpdate::ApprovalResolved(pending.to_string()));
                        }
                        // A stop report is useful after cancellation, too: preserve
                        // what happened and what remains instead of a silent end.
                        if self.last_text.take().as_deref() != Some(task.summary.trim())
                        {
                            self.emit(AgentUpdate::EntryUpdated(Entry {
                                id: format!("{}-assistant", task.id),
                                kind: Kind::Assistant,
                                title: "Native Core".into(),
                                body: task.summary.clone(),
                                status: "completed".into(),
                                expanded: false,
                            }));
                        }
                        self.emit_goal()?;
                        self.emit(AgentUpdate::TurnCompleted {
                            id: Some(task.id.to_string()),
                            status: if task.status == Status::Accepted {
                                "completed"
                            } else if task.status == Status::Cancelled {
                                "interrupted"
                            } else {
                                "failed"
                            }
                            .into(),
                            error: if matches!(task.status, Status::Accepted | Status::Cancelled) {
                                None
                            } else {
                                Some(format!("{:?}: {}", task.status, task.summary))
                            },
                        });
                    }
                    Err(reason) => {
                        if let Some(pending) = &self.pending {
                            self.emit(AgentUpdate::ApprovalReplyFailed {
                                id: pending.to_string(),
                                reason,
                            });
                        } else {
                            self.emit(AgentUpdate::TurnCompleted {
                                id: self.current.as_ref().map(ToString::to_string),
                                status: "failed".into(),
                                error: Some(reason),
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
impl AgentBackend for NativeBackend {
    fn command(&mut self, command: AgentCommand) -> Result<()> {
        if self.stopped {
            bail!("Native backend is shut down");
        }
        match command {
            AgentCommand::Inquiry(_) => anyhow::bail!("병렬 질문은 --backend v5에서 지원합니다"),
            AgentCommand::Submit {
                text,
                model,
                images,
                skills,
                effort,
            } => {
                if self.pending.is_some() {
                    bail!("먼저 변경 승인에 답해주세요.");
                }
                if !self.trusted && !self.options.readonly {
                    bail!("workspace trust is required before editing");
                }
                if model == "auto" || model.is_empty() {
                    self.options.need.min_tier = Tier::Local;
                    self.options.model = None;
                } else if let Some(tier) = loom_core::models::cli::parse_tier(&model) {
                    self.options.need.min_tier = tier;
                    self.options.model = None;
                } else {
                    self.options.need.min_tier = Tier::Local;
                    self.options.model = Some(model.clone());
                }
                self.options.reasoning_effort = effort;
                self.options.skills = skills
                    .iter()
                    .map(|skill| PathBuf::from(&skill.path))
                    .collect();
                self.options.images = images
                    .iter()
                    .map(|image| loom_core::surfaces::image_part(image.path()))
                    .collect::<Result<Vec<_>>>()?;
                self.options.approve = self.approval == "never";
                if let Some(chat) = chat_input(&text, !images.is_empty()) {
                    self.launch_chat(chat)?;
                } else {
                    let (goal, intent) = task_input(&text, self.default_intent);
                    self.options.intent = intent;
                    let started = self.launch(Some(goal), None);
                    self.options.intent = self.default_intent;
                    started?;
                }
            }
            AgentCommand::Reply { request_id, choice } => {
                if self.active && self.pending_web.remove(&request_id) {
                    if !matches!(choice.as_str(), "accept" | "reject") {
                        self.pending_web.insert(request_id);
                        bail!("invalid web permission choice");
                    }
                    // Never turn an untrusted workspace into a network-capable one.
                    let granted = choice == "accept" && self.trusted;
                    self.options.control.answer(request_id.clone(), serde_json::json!(granted));
                    self.emit(AgentUpdate::ApprovalResolved(request_id));
                    return Ok(());
                }
                let id = self.pending.clone().context("no pending approval")?;
                if request_id != id.to_string() || !matches!(choice.as_str(), "accept" | "reject") {
                    bail!("invalid Native approval token");
                }
                if choice == "accept" && self.options.readonly {
                    bail!("read-only Native backend cannot approve edits");
                }
                self.launch(None, Some((id, choice == "accept")))?;
            }
            AgentCommand::Interrupt => {
                self.cancel.store(true, Ordering::SeqCst);
                if self.active && self.chat_active && !self.chat_interrupt_notified {
                    self.chat_interrupt_notified = true;
                    self.emit(AgentUpdate::TurnCompleted {
                        id: self.current.as_ref().map(ToString::to_string),
                        status: "interrupted".into(), error: None,
                    });
                    // Retain active until the provider thread returns. It must
                    // not accept another turn or publish any late result.
                }
                if !self.active {
                    if let Some(id) = self.pending.clone() {
                        self.launch(None, Some((id, false)))?;
                    }
                }
            }
            AgentCommand::Answer {
                request_id,
                answers,
            } => {
                let value = serde_json::Value::Object(
                    answers
                        .into_iter()
                        .map(|answer| {
                            (
                                answer.question_id,
                                match answer.value {
                                    AnswerValue::Option(index) => {
                                        serde_json::json!({"option":index})
                                    }
                                    AnswerValue::Text(text) => serde_json::json!({"text":text}),
                                },
                            )
                        })
                        .collect(),
                );
                self.options.control.answer(request_id, value);
            }
            AgentCommand::CancelInput { request_id } => {
                self.options.control.answer(request_id.clone(), serde_json::json!({"cancelled":true}));
                self.emit(AgentUpdate::InputResolved(request_id));
            }
            AgentCommand::DirectAgent { id, text } => {
                if !self.active {
                    bail!("no active Native worker");
                }
                self.options.control.direct_agent(id, text);
            }
            AgentCommand::RefreshModels => self.refresh_models(),
            AgentCommand::RefreshSkills => {
                let skills = loom_core::instructions::discover(&self.root)?
                    .into_iter()
                    .map(|s| Skill {
                        name: s.name,
                        path: s.path.display().to_string(),
                        description: s.description,
                    })
                    .collect();
                self.events
                    .push_back(AgentEvent::global(AgentUpdate::SkillsLoaded {
                        skills,
                        errors: vec![],
                    }));
            }
            AgentCommand::DeleteSession(id) => {
                loom_core::sessions::Sessions::open(&self.root)?.delete(&id)?;
                self.emit(AgentUpdate::Notice("대화를 삭제했습니다.".into()));
                return self.command(AgentCommand::RefreshSessions);
            }
            AgentCommand::RefreshSessions | AgentCommand::MoreSessions(_) => {
                let sessions = loom_core::sessions::Sessions::open(&self.root)?
                    .list()?
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
                        sessions,
                        cursor: None,
                        append: false,
                    }));
            }
            AgentCommand::Goal(action) => {
                let sessions = loom_core::sessions::Sessions::open(&self.root)?;
                sessions.update(&self.session, |s| {
                    match action {
                        GoalAction::Show => {}
                        GoalAction::Set(objective) => {
                            s.goal = Some(loom_core::sessions::Goal {
                                objective,
                                status: "active".into(),
                                tokens_used: 0,
                                token_budget: None,
                            })
                        }
                        GoalAction::Clear => s.goal = None,
                        GoalAction::Pause | GoalAction::Resume => {
                            let goal = s.goal.as_mut().context("no current goal")?;
                            goal.status = if action == GoalAction::Pause {
                                "paused"
                            } else {
                                "active"
                            }
                            .into();
                        }
                    }
                    Ok(())
                })?;
                self.emit_goal()?;
            }
            AgentCommand::Compact => {
                if self.active {
                    bail!("finish the active task before manual compaction");
                }
                self.emit(AgentUpdate::CompressionState {
                    running: true,
                    error: None,
                });
                let result =
                    loom_core::sessions::Sessions::open(&self.root)?.compact(&self.session, 2);
                self.emit(AgentUpdate::CompressionState {
                    running: false,
                    error: result.err().map(|e| e.to_string()),
                });
            }
            AgentCommand::UpdatePermission { kind, value } => {
                if self.active {
                    bail!("cannot change permissions while a task is running");
                }
                match kind.as_str() {
                    "sandbox" => {
                        self.options.policy.sandbox = match value.as_str() {
                            "readOnly" => loom_core::effects::SandboxMode::ReadOnly,
                            "workspaceWrite" => loom_core::effects::SandboxMode::WorkspaceWrite,
                            "dangerFullAccess" => loom_core::effects::SandboxMode::FullAccess,
                            _ => bail!("invalid sandbox mode"),
                        };
                        self.options.readonly = self.options.policy.sandbox
                            == loom_core::effects::SandboxMode::ReadOnly;
                    }
                    "approval" => {
                        if !matches!(value.as_str(), "never" | "on-request" | "untrusted") {
                            bail!("invalid approval policy");
                        }
                        self.approval = value;
                    }
                    "network" => {
                        self.options.policy.network = match value.as_str() {
                            "true" | "allow" => true,
                            "false" | "deny" => false,
                            _ => bail!("invalid network policy"),
                        }
                    }
                    _ => bail!("invalid permission kind"),
                }
                self.save_settings()?;
                self.emit(AgentUpdate::PermissionApplied);
                self.emit(AgentUpdate::PermissionsUpdated {
                    permissions: self.permissions(),
                    defaults_only: false,
                });
            }
            AgentCommand::TrustWorkspace(trusted) => {
                self.trusted = trusted;
                if !trusted {
                    self.options.readonly = true;
                    self.options.policy.sandbox = loom_core::effects::SandboxMode::ReadOnly;
                    self.options.policy.network = false;
                }
                self.save_settings()?;
                self.emit(AgentUpdate::TrustResolved(trusted));
            }
            AgentCommand::RefreshUsage => {
                self.emit(AgentUpdate::UsageUpdated(format!(
                    "Native Core · input {} · output {} · cached {}\n{}",
                    self.input,
                    self.output,
                    self.cached,
                    serde_json::to_string_pretty(&self.model.limits())?
                )));
            }
            AgentCommand::Explore(query) => {
                let world = loom_core::world::World::open(&self.root)?;
                self.emit(AgentUpdate::Notice(world.find(&query, 4096)?.compact()));
            }
            AgentCommand::OpenSession { id, .. } => {
                let id = match id {
                    Some(id) => id,
                    None => {
                        loom_core::sessions::Sessions::open(&self.root)?
                            .create()?
                            .id
                    }
                };
                self.load_session(&id)?;
            }
            AgentCommand::Fork { turn_id, .. } => {
                if self.active {
                    bail!("cannot fork a running task");
                }
                let branch = loom_core::sessions::Sessions::open(&self.root)?
                    .fork(&self.session, &turn_id)?;
                self.load_session(&branch.id)?;
            }
            AgentCommand::PreviewRewind { turn_id } => {
                let mut preview = Core::open(&self.root)?.rewind_preview(&self.session, &turn_id);
                if self.active || self.pending.is_some() || self.options.readonly || !self.trusted
                    || self.options.policy.sandbox == loom_core::effects::SandboxMode::ReadOnly {
                    preview.blocked = Some("현재 작업과 승인을 마치고 쓰기 권한을 허용한 뒤 코드 복원이 가능합니다.".into());
                }
                self.emit(AgentUpdate::RewindPreview { turn_id, files: preview.files.len(), added: preview.added, removed: preview.removed, blocked: preview.blocked });
            }
            AgentCommand::Rewind { turn_id, .. } => {
                if self.active || self.pending.is_some() || self.options.readonly || !self.trusted
                    || self.options.policy.sandbox == loom_core::effects::SandboxMode::ReadOnly {
                    bail!("finish editing and enable workspace writes before rewind");
                }
                let branch = Core::open(&self.root)?.rewind(&self.session, &turn_id)?;
                self.load_session(&branch.id)?;
                self.emit(AgentUpdate::Notice("대화와 코드 복원 완료".into()));
            }
            AgentCommand::ReadActivity(id) => {
                if !self.child_activities.contains_key(&id) {self.restore_child_activity(&id)?;}
                if let Some(activity)=self.child_activities.get(&id) {
                    let history=AgentUpdate::ActivityHistory {id,entries:activity.entries.clone(),status:activity.status.clone()};
                    self.emit(history);
                    return Ok(());
                }
                let core = Core::open(&self.root)?;
                let task = core
                    .state
                    .tasks
                    .get(&TaskId(id.clone()))
                    .context("unknown Native activity")?;
                self.emit(AgentUpdate::ActivityHistory {
                    id,
                    entries: vec![Entry {
                        id: task.id.0.clone(),
                        kind: Kind::Assistant,
                        title: "Native task".into(),
                        body: task.summary.clone(),
                        status: format!("{:?}", task.status),
                        expanded: true,
                    }],
                    status: format!("{:?}", task.status),
                });
            }
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<Option<AgentEvent>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        if let Ok(message) = self.rx.try_recv() {
            self.receive(message)?;
        }
        Ok(self.events.pop_front())
    }
    fn shutdown(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.cancel.store(true, Ordering::SeqCst);
        // Let Runner observe cancellation and reap its child group before the
        // terminal process exits. HTTP model calls must never hold exit open.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while self.thread.as_ref().is_some_and(|t| !t.is_finished())
            && std::time::Instant::now() < deadline
        {
            thread::sleep(std::time::Duration::from_millis(10));
        }
        if self.thread.as_ref().is_some_and(|t| t.is_finished()) {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
        Ok(())
    }
}
impl Drop for NativeBackend {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn turn_snapshot(id: &str, goal: &str, summary: &str, status: Status) -> TurnSnapshot {
    // A restored turn must agree with the terminal event originally sent to the
    // UI. Otherwise an unresolved or validation-failed task looks user-cancelled.
    let status = match status {
        Status::Accepted => "completed",
        Status::Unresolved | Status::ValidationFailed | Status::ScopeConflict => "failed",
        Status::Cancelled | Status::Planned | Status::Running | Status::Validating
        | Status::AwaitingApproval => "interrupted",
    };
    TurnSnapshot {
        id: Some(id.into()),
        status: status.into(),
        entries: vec![
            Entry {
                id: format!("{id}-user"),
                kind: Kind::User,
                title: String::new(),
                body: goal.into(),
                status: "completed".into(),
                expanded: false,
            },
            Entry {
                id: format!("{id}-assistant"),
                kind: Kind::Assistant,
                title: "Native Core".into(),
                body: summary.into(),
                status: "completed".into(),
                expanded: false,
            },
        ],
        activities: vec![],
    }
}

/// Marks a flag when dropped, also on early return or panic.
fn scopeguard(flag: Arc<AtomicBool>) -> impl Drop {
    struct Guard(Arc<AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    Guard(flag)
}
