//! L6: durable task ownership, independent validation and guarded acceptance.
use crate::{
    base::{ArtifactRef, ArtifactStore, Cost, EventLog, Phase, Revision, TaskId},
    effects::{Overlay, Patch, RunStatus, Runner, TestResult},
    models::{cost_ceiling, ModelPort, Need, Tier},
    worker::{self, Budget, ToolSet, WorkerResult, WorkerStatus, WorkerTask},
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Planned,
    Running,
    Validating,
    AwaitingApproval,
    Accepted,
    Unresolved,
    ValidationFailed,
    ScopeConflict,
    Cancelled,
}
/// The surface knows the requested effect; Core enforces it independently of prose.
/// Auto retains compatibility with existing chat callers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskIntent {
    #[default]
    Auto,
    Answer,
    Inspect,
    Edit,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    Single,
    Split,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SplitDecision {
    pub observed_fixed_ms: Option<u64>,
    pub estimated_work: usize,
    pub reason: String,
    pub mode: ExecutionMode,
    #[serde(default)]
    pub groups: Vec<crate::world::split::Group>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub tier: Tier,
    pub result: ArtifactRef,
    pub status: WorkerStatus,
    pub validation: Vec<TestResult>,
    pub summary: String,
    pub cost_by_phase: BTreeMap<Phase, Cost>,
    pub input_tokens: u64,
    #[serde(default)]
    pub remote_input_tokens: Option<u64>,
    #[serde(default)]
    pub remote_calls: u32,
    #[serde(default)]
    pub model_wait_ms: u64,
    #[serde(default)]
    pub gateway_overhead_ms: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub steps: usize,
    pub rereads: u32,
    pub phase_bytes: BTreeMap<String, u64>,
    pub models: BTreeMap<String, u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub goal: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default = "legacy_policy")]
    pub policy: crate::effects::ExecutionPolicy,
    #[serde(default)]
    pub guidance: String,
    #[serde(default)]
    pub evidence: Vec<ArtifactRef>,
    #[serde(default)]
    pub images: Vec<crate::models::Part>,
    #[serde(default)]
    pub image_artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    pub skills: Vec<PathBuf>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    pub status: TaskStatus,
    #[serde(default)]
    pub active_tier: Option<Tier>,
    pub base: Revision,
    pub attempts: Vec<Attempt>,
    pub patch: Option<ArtifactRef>,
    #[serde(default)]
    pub context: Option<ArtifactRef>,
    #[serde(default)]
    pub contract: Option<crate::context::EffectContract>,
    #[serde(default)]
    pub scope: Option<crate::context::ScopeReport>,
    #[serde(default)]
    pub slice_bytes: usize,
    pub checks: Vec<String>,
    pub summary: String,
    pub decision: SplitDecision,
    pub pending_apply: Option<Revision>,
    pub readonly: bool,
    #[serde(default)]
    pub intent: TaskIntent,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CanonicalState {
    pub revision: Option<Revision>,
    pub tasks: BTreeMap<TaskId, Task>,
    pub findings: Vec<String>,
    pub failures: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CoreEvent {
    TaskSaved(Box<Task>),
    RevisionUpdated(Revision),
    WorkerEvent { task: TaskId, event: Value },
}

/// Effect of rewinding code to an earlier reply (`Core::rewind_preview`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindPreview {
    pub files: Vec<String>,
    pub added: usize,
    pub removed: usize,
    /// Why code cannot be rewound; the conversation alone still can.
    pub blocked: Option<String>,
}
impl RewindPreview {
    fn blocked(reason: String) -> Self {
        Self {
            blocked: Some(reason),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExecutionOptions {
    pub need: Need,
    pub budget: Budget,
    pub checks: Option<Vec<String>>,
    pub max_attempts: usize,
    pub approve: bool,
    pub readonly: bool,
    pub intent: TaskIntent,
    pub context: Option<String>,
    pub use_context: bool,
    pub session: Option<String>,
    pub skills: Vec<PathBuf>,
    pub images: Vec<crate::models::Part>,
    pub reasoning_effort: Option<String>,
    pub model: Option<String>,
    pub context_limit: usize,
    pub control: crate::worker::WorkerControl,
    pub policy: crate::effects::ExecutionPolicy,
}
impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            need: Need {
                max_cost: crate::models::CostClass::Free,
                ..Need::default()
            },
            budget: Budget::default(),
            checks: None,
            max_attempts: 8,
            approve: true,
            readonly: false,
            intent: TaskIntent::Auto,
            context: None,
            use_context: false,
            session: None,
            skills: vec![],
            images: vec![],
            reasoning_effort: None,
            model: None,
            context_limit: 96_000,
            control: crate::worker::WorkerControl::default(),
            policy: legacy_policy(),
        }
    }
}

pub struct Core {
    pub state: CanonicalState,
    root: PathBuf,
    log: EventLog,
    store: crate::base::FileArtifactStore,
}
impl Core {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.canonicalize()?;
        let log = EventLog::open(&root)?;
        let store = crate::base::FileArtifactStore::open(&root)?;
        let state = log.fold(CanonicalState::default(), |state, event| match event {
            CoreEvent::TaskSaved(task) => {
                state.tasks.insert(task.id.clone(), *task);
            }
            CoreEvent::RevisionUpdated(revision) => state.revision = Some(revision),
            CoreEvent::WorkerEvent { .. } => {}
        })?;
        let mut core = Self {
            state,
            root,
            log,
            store,
        };
        let pending: Vec<_> = core
            .state
            .tasks
            .values()
            .filter(|t| t.pending_apply.is_some())
            .cloned()
            .collect();
        if !pending.is_empty() {
            let current = Revision::capture(&core.root)?;
            for mut task in pending {
                if task.pending_apply.as_ref() == Some(&current) {
                    task.status = TaskStatus::Accepted;
                    task.pending_apply = None;
                    core.state.revision = Some(current.clone());
                    core.log
                        .append(&CoreEvent::RevisionUpdated(current.clone()))?;
                    core.save(task, &mut |_| {})?;
                }
            }
        }
        refresh_reports(&mut core.state);
        Ok(core)
    }
    pub fn artifacts(&self) -> &crate::base::FileArtifactStore {
        &self.store
    }
    fn save(&mut self, mut task: Task, observe: &mut impl FnMut(CoreEvent)) -> Result<Task> {
        if matches!(
            task.status,
            TaskStatus::Unresolved
                | TaskStatus::ValidationFailed
                | TaskStatus::Cancelled
                | TaskStatus::ScopeConflict
        ) && !task.summary.contains("\n\nCore 실행 보고\n")
        {
            self.add_stop_report(&mut task);
        }
        if task.status != TaskStatus::Running {
            task.active_tier = None;
        }
        let event = CoreEvent::TaskSaved(Box::new(task.clone()));
        self.log.append(&event)?;
        self.state.tasks.insert(task.id.clone(), task.clone());
        refresh_reports(&mut self.state);
        if matches!(
            task.status,
            TaskStatus::Accepted
                | TaskStatus::Unresolved
                | TaskStatus::ValidationFailed
                | TaskStatus::Cancelled
                | TaskStatus::ScopeConflict
        ) && let Some(session) = &task.session
        {
            let after = if task.status == TaskStatus::Accepted {
                Some(Revision::capture(&self.root)?)
            } else {
                None
            };
            crate::sessions::Sessions::open(&self.root)?.record(session, &task, after)?;
        }
        observe(event);
        Ok(task)
    }

    /// This runs locally even if the model is blocked or no budget remains.
    fn add_stop_report(&self, task: &mut Task) {
        let last = task.attempts.last();
        let result = last
            .and_then(|attempt| self.store.get(&attempt.result, None).ok())
            .and_then(|bytes| serde_json::from_slice::<WorkerResult>(&bytes).ok());
        let activity = result
            .as_ref()
            .map(|result| result.activity_report.as_str())
            .filter(|report| !report.is_empty())
            .map(display_activity_report)
            .unwrap_or_else(|| "진행 결과: 실행 완료된 도구 기록 없음".into());
        let checks: Vec<_> = task
            .attempts
            .iter()
            .flat_map(|attempt| &attempt.validation)
            .collect();
        let validation = if !checks.is_empty() {
            let failed = checks
                .iter()
                .filter(|check| check.status != RunStatus::Passed)
                .count();
            let detail = checks
                .iter()
                .rev()
                .filter_map(|check| check.first_error.as_ref())
                .next()
                .map(|error| {
                    format!(
                        " · {}",
                        error.to_string().chars().take(200).collect::<String>()
                    )
                })
                .unwrap_or_default();
            format!("누적 {}개 실행 · 실패 {failed}개{detail}", checks.len())
        } else {
            "아직 수행하지 않음".into()
        };
        let remaining = result
            .as_ref()
            .map(|result| {
                result
                    .plan
                    .iter()
                    .filter(|step| step.status != "completed")
                    .take(4)
                    .map(|step| step.step.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .filter(|plan| !plan.is_empty())
            .unwrap_or_else(|| task.goal.chars().take(300).collect());
        let evidence = result
            .as_ref()
            .map(|result| format!("\n실행 근거: .loom/artifacts/{}", result.trace.id))
            .unwrap_or_default();
        let reason = task.summary.chars().take(600).collect::<String>();
        let (calls, tokens, wait_ms) =
            task.attempts
                .iter()
                .fold((0usize, 0u64, 0u64), |(calls, tokens, wait), attempt| {
                    (
                        calls.saturating_add(attempt.steps),
                        tokens
                            .saturating_add(attempt.input_tokens)
                            .saturating_add(attempt.output_tokens),
                        wait.saturating_add(attempt.model_wait_ms),
                    )
                });
        let errors = task
            .attempts
            .iter()
            .rev()
            .take(3)
            .rev()
            .filter_map(|attempt| {
                self.store
                    .get(&attempt.result, None)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<WorkerResult>(&bytes).ok())
                    .and_then(|result| result.last_model_error)
                    .map(|error| error.chars().take(200).collect::<String>())
            })
            .collect::<Vec<_>>();
        let errors = if errors.is_empty() {
            String::new()
        } else {
            format!("\n최근 모델 실패: {}", errors.join("; "))
        };
        task.summary = format!(
            "{}\n\nCore 실행 보고\n{activity}\n누적 실행: 시도 {}회 · 모델 요청 {calls}회 · 토큰 {tokens} · 모델 대기 {}초{errors}\nCore 검증: {validation}\n원본 반영: 없음 · 미수락 제안은 적용되지 않았습니다.\n남은 작업: {remaining}\n중단 사유: {reason}{evidence}",
            if task.status == TaskStatus::Cancelled {
                "요청에 따라 작업을 중단했습니다."
            } else {
                "작업을 완료하지 못했습니다."
            },
            task.attempts.len(),
            wait_ms / 1000
        );
    }
    /// Whether some live process is executing this task now. A task saved as Planned,
    /// Running or Validating by a process that has since ended (window closed, crash,
    /// kill) is not: its lease is held for the whole run and released by the OS on exit.
    /// The stored status is left as is so `resume` can continue from it.
    pub fn is_executing(&self, id: &TaskId) -> bool {
        self.try_execution_lease(id, false).is_err()
    }
    fn execution_lease(&self, id: &TaskId) -> Result<fs::File> {
        self.try_execution_lease(id, true)
    }
    fn try_execution_lease(&self, id: &TaskId, wait: bool) -> Result<fs::File> {
        use sha2::{Digest, Sha256};
        let directory = self.root.join(".loom/tasks");
        fs::create_dir_all(&directory)?;
        let key = format!("{:x}", Sha256::digest(id.0.as_bytes()));
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(format!("{key}.lock")))?;
        // A child process forked elsewhere (e.g. a background overlay removal) holds a
        // copy of every descriptor until its exec closes them; wait out that moment.
        for _ in 0..if wait { 40 } else { 0 } {
            match lock.try_lock_exclusive() {
                Ok(()) => return Ok(lock),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(e) => return Err(e.into()),
            }
        }
        lock.try_lock_exclusive()
            .context("task is already running in another Core instance")?;
        Ok(lock)
    }
    pub fn execute(
        &mut self,
        goal: &str,
        mut options: ExecutionOptions,
        model: &dyn ModelPort,
        cancel: &AtomicBool,
        mut observe: impl FnMut(CoreEvent),
    ) -> Result<Task> {
        if options.need.max_cost > cost_ceiling() {
            bail!("paid models are blocked in Core");
        }
        let mut guidance = crate::instructions::initial(&self.root, goal, &options.skills)?;
        let mut evidence = vec![];
        if let Some(session) = &options.session {
            let sessions = crate::sessions::Sessions::open(&self.root)?;
            if let Some(goal) = sessions.ensure(session)?.goal {
                if goal.status == "budget_limited" {
                    bail!("persistent goal token budget exhausted");
                }
                if goal.status == "active"
                    && let Some(limit) = goal.token_budget
                {
                    options.budget.tokens = options
                        .budget
                        .tokens
                        .min(limit.saturating_sub(goal.tokens_used));
                }
            }
            let (mut history, mut refs) = sessions.context(session)?;
            if history.len() > options.context_limit / 2 {
                sessions.compact(session, 2)?;
                (history, refs) = sessions.context(session)?;
            }
            guidance.push_str(&history);
            evidence = refs;
        }
        let world = crate::world::World::open(&self.root)?;
        let base = world.snapshot.revision.clone();
        self.state.revision = Some(base.clone());
        let revision_event = CoreEvent::RevisionUpdated(base.clone());
        self.log.append(&revision_event)?;
        observe(revision_event);
        let contract = crate::context::derive(goal, &world)?;
        let mut ids: Vec<_> = contract
            .anchor
            .iter()
            .chain(&contract.write_hints)
            .map(|id| id.0.clone())
            .collect();
        if ids.is_empty() {
            for path in &contract.write_files {
                ids.extend(
                    world
                        .graph
                        .in_file(&path.to_string_lossy())
                        .filter(|symbol| !symbol.is_pseudo())
                        .map(|symbol| symbol.id.clone()),
                );
            }
        }
        ids.sort();
        ids.dedup();
        let groups = if options.readonly
            || matches!(options.intent, TaskIntent::Answer | TaskIntent::Inspect)
        {
            vec![]
        } else {
            crate::world::split::candidates(&world.graph, &ids).unwrap_or_default()
        };
        let split =
            groups.len() >= 2 && groups.len() <= 4 && options.budget.steps >= groups.len() * 3;
        let execution_reason = if split {
            format!(
                "AST 그래프에서 쓰기·의존 범위가 분리된 {}개 작업을 병렬 배분합니다. 병합 후 Core가 검증합니다.",
                groups.len()
            )
        } else if ids.is_empty() {
            "그래프에서 작업 대상을 아직 특정하지 못해 한 작업으로 시작합니다.".into()
        } else {
            "연결된 쓰기·의존 범위 또는 현재 호출 한도를 고려해 한 작업으로 실행합니다.".into()
        };
        let mut slice_bytes = 0;
        let mut supplied = vec![];
        // Efficiency adoption remains opt-in until the measured input-token gate passes.
        if options.use_context {
            let slice = crate::context::compile(&contract, &world, 16_000)?;
            slice_bytes = slice.bytes;
            options.context = Some(slice.text);
            supplied = slice.supplied;
        }
        let context = options
            .context
            .as_ref()
            .map(|text| {
                self.store
                    .put(&serde_json::to_vec(&worker::PreparedContext {
                        text: text.clone(),
                        supplied,
                    })?)
            })
            .transpose()?;
        let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let id = TaskId(format!("task-{millis:x}-{}", std::process::id()));
        let fixed = self
            .state
            .tasks
            .values()
            .flat_map(|t| t.attempts.iter())
            .map(|a| {
                a.validation.iter().map(|v| v.ms).sum::<u64>()
                    + a.cost_by_phase.get(&Phase::Orient).map_or(0, |c| c.ms)
            })
            .filter(|n| *n > 0)
            .collect::<Vec<_>>();
        let mut fixed = fixed;
        fixed.sort_unstable();
        let estimated_work =
            contract.write_files.len() + contract.anchor.len() + contract.write_hints.len();
        let task = Task {
            id,
            goal: goal.into(),
            session: options.session.clone(),
            policy: options.policy,
            guidance,
            evidence,
            images: vec![],
            image_artifacts: options
                .images
                .iter()
                .map(|part| self.store.put(&serde_json::to_vec(part)?))
                .collect::<Result<Vec<_>>>()?,
            skills: options.skills.clone(),
            reasoning_effort: options.reasoning_effort.clone(),
            model: options.model.clone(),
            status: TaskStatus::Planned,
            active_tier: None,
            base,
            attempts: vec![],
            patch: None,
            context,
            contract: Some(contract),
            scope: None,
            slice_bytes,
            checks: options
                .checks
                .clone()
                .unwrap_or_else(|| worker::default_checks(&self.root)),
            summary: String::new(),
            decision: SplitDecision {
                observed_fixed_ms: fixed.get(fixed.len() / 2).copied(),
                estimated_work,
                reason: execution_reason,
                mode: if split {
                    ExecutionMode::Split
                } else {
                    ExecutionMode::Single
                },
                groups: if split { groups } else { vec![] },
            },
            pending_apply: None,
            readonly: options.readonly
                || matches!(options.intent, TaskIntent::Answer | TaskIntent::Inspect),
            intent: options.intent,
        };
        let _lease = self.execution_lease(&task.id)?;
        let task = self.save(task, &mut observe)?;
        self.solve(task, options, model, cancel, &mut observe)
    }
    pub fn resume(
        &mut self,
        id: &TaskId,
        options: ExecutionOptions,
        model: &dyn ModelPort,
        cancel: &AtomicBool,
        mut observe: impl FnMut(CoreEvent),
    ) -> Result<Task> {
        let _lease = self.execution_lease(id)?;
        self.state = Self::open(&self.root)?.state;
        let mut task = self.state.tasks.get(id).context("unknown task")?.clone();
        if options.readonly && !task.readonly {
            bail!("read-only execution cannot resume an edit task");
        }
        if options.need.max_cost > cost_ceiling() {
            bail!("paid models are blocked in Core");
        }
        if task.status == TaskStatus::ScopeConflict {
            let world = crate::world::World::open(&self.root)?;
            task.base = world.snapshot.revision.clone();
            task.contract = Some(crate::context::derive(&task.goal, &world)?);
            task.scope = None;
            task.pending_apply = None;
            if options.use_context || task.slice_bytes > 0 {
                let slice =
                    crate::context::compile(task.contract.as_ref().unwrap(), &world, 16_000)?;
                task.slice_bytes = slice.bytes;
                task.context = Some(self.store.put(&serde_json::to_vec(
                    &worker::PreparedContext {
                        text: slice.text,
                        supplied: slice.supplied,
                    },
                )?)?);
            }
            self.state.revision = Some(task.base.clone());
            let event = CoreEvent::RevisionUpdated(task.base.clone());
            self.log.append(&event)?;
            observe(event);
            task.status = TaskStatus::Planned;
            self.save(task.clone(), &mut observe)?;
        }
        match task.status {
            TaskStatus::Accepted | TaskStatus::Cancelled => Ok(task),
            TaskStatus::AwaitingApproval if options.approve => self.approve(id, cancel, observe),
            TaskStatus::AwaitingApproval => Ok(task),
            TaskStatus::Validating if task.patch.is_some() => {
                let patch: Patch =
                    serde_json::from_slice(&self.store.get(task.patch.as_ref().unwrap(), None)?)?;
                let overlay = Overlay::create(&self.root, &patch.base)?;
                patch.apply(overlay.dir())?;
                let mut task = task;
                let validation =
                    self.validate(&overlay, &task.checks, cancel, task.policy, None)?;
                record_validation(&mut task, &validation);
                task.status = if validation.iter().all(|r| r.status == RunStatus::Passed)
                    && !validation.is_empty()
                {
                    TaskStatus::AwaitingApproval
                } else {
                    TaskStatus::ValidationFailed
                };
                if task.status == TaskStatus::AwaitingApproval {
                    self.seal_patch(&mut task, &overlay)?;
                }
                let task = self.save(task, &mut observe)?;
                if task.status == TaskStatus::AwaitingApproval && options.approve {
                    self.approve(id, cancel, observe)
                } else {
                    Ok(task)
                }
            }
            _ => self.solve(task, options, model, cancel, &mut observe),
        }
    }
    fn solve(
        &mut self,
        mut task: Task,
        options: ExecutionOptions,
        model: &dyn ModelPort,
        cancel: &AtomicBool,
        observe: &mut impl FnMut(CoreEvent),
    ) -> Result<Task> {
        let tiers = [Tier::Local, Tier::Small, Tier::Medium, Tier::Flagship];
        let mut prepared = task
            .context
            .as_ref()
            .map(|reference| {
                serde_json::from_slice::<worker::PreparedContext>(&self.store.get(reference, None)?)
                    .map_err(anyhow::Error::from)
            })
            .transpose()?;
        let last = task
            .attempts
            .last()
            .map_or(options.need.min_tier, |a| a.tier.max(options.need.min_tier))
            .max(task.active_tier.unwrap_or(options.need.min_tier));
        let first = tiers.iter().position(|t| *t == last).unwrap();
        let retry_offset = task
            .attempts
            .iter()
            .rev()
            .take_while(|a| a.tier == last)
            .count()
            .min(2);
        let mut tier_steps = retry_offset;
        // The command budget covers the whole solve, including validation and
        // tier retries. Re-granting it to every attempt can multiply runtime and
        // token usage by max_attempts even when the user set a hard limit.
        let started = Instant::now();
        let deadline = started.checked_add(Duration::from_secs(options.budget.seconds));
        let validation_reserve = if task.readonly {
            0
        } else {
            (options.budget.seconds / 10).min(30)
        };
        let mut remaining_tokens = options.budget.tokens;
        for _index in 0..options.max_attempts {
            if cancel.load(Ordering::SeqCst) {
                task.status = TaskStatus::Cancelled;
                break;
            }
            if started.elapsed() >= Duration::from_secs(options.budget.seconds)
                || remaining_tokens == 0
            {
                task.status = TaskStatus::Unresolved;
                let reason = if remaining_tokens == 0 {
                    "total model token budget exhausted"
                } else {
                    "total execution time budget exhausted"
                };
                let last = task.attempts.last();
                let detail = last.map(|a| format!(
                    "{} successful remote calls, {}ms model wait, {}ms gateway overhead (reported), {} steps",
                    a.remote_calls, a.model_wait_ms, a.gateway_overhead_ms, a.steps
                )).unwrap_or_else(|| "no completed worker attempt".into());
                task.summary = if task.summary.is_empty() {
                    format!("{reason}; {detail}")
                } else {
                    format!("{reason}; {detail}; last attempt: {}", task.summary)
                };
                break;
            }
            let mut attempt_budget = options.budget;
            attempt_budget.seconds = options
                .budget
                .seconds
                .saturating_sub(started.elapsed().as_secs());
            // Keep time for Core's independent validation. The same overall
            // deadline still covers work, retries and verification.
            if attempt_budget.seconds <= validation_reserve {
                task.status = TaskStatus::Unresolved;
                task.summary = format!(
                    "worker execution window ended; {}s reserved for Core validation; last attempt: {}",
                    attempt_budget.seconds, task.summary
                );
                break;
            }
            attempt_budget.seconds = attempt_budget.seconds.saturating_sub(validation_reserve);
            attempt_budget.tokens = remaining_tokens;
            let tier = tiers[(first + tier_steps / 2).min(3)];
            tier_steps += 1;
            task.active_tier = Some(tier);
            task.status = TaskStatus::Running;
            self.save(task.clone(), observe)?;
            let overlay = Overlay::create(&self.root, &task.base)?;
            let mut artifacts = task.evidence.clone();
            artifacts.extend(task.image_artifacts.iter().cloned());
            let mut images = task.images.clone();
            for reference in &task.image_artifacts {
                images.push(serde_json::from_slice(&self.store.get(reference, None)?)?);
            }
            let mut failed_patch_diff = None;
            let mut repair_plan = vec![];
            let handoff = if let Some(previous) = task.attempts.last() {
                let result: WorkerResult =
                    serde_json::from_slice(&self.store.get(&previous.result, None)?)?;
                repair_plan = result.plan.clone();
                artifacts.extend([previous.result.clone(), result.trace.clone()]);
                artifacts.extend(result.artifacts.iter().cloned());
                artifacts.extend(previous.validation.iter().map(|v| v.artifact.clone()));
                let mut diff = None;
                let mut proposal_restored = false;
                if let Some(reference) = &task.patch {
                    let patch: Patch = serde_json::from_slice(&self.store.get(reference, None)?)?;
                    if previous
                        .validation
                        .iter()
                        .any(|check| check.status != RunStatus::Passed)
                    {
                        failed_patch_diff = Some(patch.diff.clone());
                    }
                    artifacts.push(reference.clone());
                    diff = Some(self.store.put(&patch.diff)?);
                    artifacts.extend(diff.clone());
                    if !task.readonly && patch.base == task.base {
                        patch.apply(overlay.dir())?;
                        proposal_restored = true;
                        if task.slice_bytes > 0 && !patch.files.is_empty() {
                            let world = crate::world::World::open(overlay.dir())?;
                            let contract = crate::context::derive(&task.goal, &world)?;
                            let slice = crate::context::compile(&contract, &world, 16_000)?;
                            task.slice_bytes = slice.bytes;
                            prepared = Some(worker::PreparedContext {
                                text: slice.text,
                                supplied: slice.supplied,
                            });
                            task.context = Some(
                                self.store
                                    .put(&serde_json::to_vec(&prepared.as_ref().unwrap())?)?,
                            );
                        }
                    }
                }
                // A failed proposal can reveal files the original lexical contract
                // missed. Forward observed effects and the revised contract so the
                // next worker can orient around actual work instead of stale guesses.
                // The structured index is kept in the worker result; only its
                // projection belongs in the remote prompt, not all 64 entries.
                Some(json!({"summary":previous.summary,"validation":previous.validation,"previous_result":previous.result,"trace":result.trace,"worker_artifacts":result.artifacts,"work_memory":result.work_memory,"prior_observations":result.work_memory.render(3000),"observation_validity":"previous attempt; revalidate source evidence against the restored proposal before editing","patch":task.patch,"diff":diff,"proposal_restored":proposal_restored,"contract":task.contract,"scope_feedback":task.scope,"observed_write_files":result.touched}).to_string())
            } else {
                None
            };
            let mut worker = WorkerTask {
                allowed_writes: None,
                goal: task.goal.clone(),
                guidance: if task.intent == TaskIntent::Edit {
                    format!(
                        "Required outcome: modify the requested files and propose a real patch. A prose-only reply cannot complete this task. If you cannot edit, finish unresolved with the blocker.\n{}",
                        task.guidance
                    )
                } else {
                    task.guidance.clone()
                },
                images,
                skills: task.skills.clone(),
                reasoning_effort: task
                    .reasoning_effort
                    .clone()
                    .or(options.reasoning_effort.clone()),
                context_limit: options.context_limit,
                model: task.model.clone().or(options.model.clone()),
                plan: repair_plan,
                depth: 0,
                defer_validation_to_core: false,
                control: options.control.clone(),
                policy: task.policy,
                overlay,
                context: prepared
                    .as_ref()
                    .map(|c| c.text.clone())
                    .or_else(|| options.context.clone()),
                supplied: prepared
                    .as_ref()
                    .map(|c| c.supplied.clone())
                    .unwrap_or_default(),
                handoff,
                artifacts,
                tools: if task.readonly {
                    ToolSet::ReadOnly
                } else {
                    ToolSet::Edit
                },
                tool_profile: Default::default(),
                budget: attempt_budget,
                need: Need {
                    min_tier: tier,
                    ..options.need
                },
                checks: task.checks.clone(),
            };
            let task_id = task.id.clone();
            let mut report = |event| {
                observe(CoreEvent::WorkerEvent {
                    task: task_id.clone(),
                    event,
                })
            };
            let result = if task.decision.mode == ExecutionMode::Split && task.attempts.is_empty() {
                worker::run_split(
                    &mut worker,
                    &task.decision.groups,
                    model,
                    cancel,
                    &mut report,
                )?
            } else {
                worker::run(&mut worker, model, cancel, &mut report)?
            };
            remaining_tokens = remaining_tokens
                .saturating_sub(result.input_tokens.saturating_add(result.output_tokens));
            if let Some(session) = &task.session
                && !result.plan.is_empty()
            {
                crate::sessions::Sessions::open(&self.root)?.update(session, |s| {
                    s.plan = result.plan.clone();
                    Ok(())
                })?;
            }
            let record = self.store.put(&serde_json::to_vec(&result)?)?;
            task.patch = Some(self.store.put(&serde_json::to_vec(&result.patch)?)?);
            task.summary = result.summary.clone();
            task.attempts.push(attempt(tier, record, &result));
            if let Some(contract) = &task.contract {
                task.scope = Some(crate::context::monitor(contract, &result.touched));
            }
            if cancel.load(Ordering::SeqCst) {
                task.status = TaskStatus::Cancelled;
                break;
            }
            if result.status != WorkerStatus::Proposed {
                task.status = TaskStatus::Unresolved;
                if matches!(result.status, WorkerStatus::Failed(_)) {
                    break;
                }
                self.save(task.clone(), observe)?;
                if result.finished {
                    // An explicit unresolved finish ends the turn. Investigation
                    // can continue through Core::resume on user request.
                    break;
                }
                if result.summary == "model-call budget reached" {
                    // Spend no second SMALL attempt on another long exploration.
                    // Pinned models cannot honor tier escalation; stop cleanly.
                    if tier == Tier::Flagship || task.model.is_some() {
                        break;
                    }
                    tier_steps = tier_steps.div_ceil(2) * 2;
                }
                continue;
            }
            if task.readonly && !result.patch.files.is_empty() {
                task.status = TaskStatus::Unresolved;
                task.summary = "read-only worker attempted filesystem changes".into();
                return self.save(task, observe);
            }
            if failed_patch_diff
                .as_ref()
                .is_some_and(|diff| *diff == result.patch.diff)
            {
                // The failed proposal was restored for repair. A fresh finish
                // without a changed patch supplies no reason to rebuild or retry.
                task.status = TaskStatus::ValidationFailed;
                task.summary = format!(
                    "proposal unchanged since failed Core validation; {}",
                    result.summary
                );
                break;
            }
            if result.patch.files.is_empty() {
                if task.intent != TaskIntent::Edit
                    && (result.answered || (task.readonly && task.intent != TaskIntent::Inspect))
                {
                    // A reply (greeting, question, explanation) is complete as is:
                    // nothing to validate or apply, and no reason to retry or escalate.
                    task.status = TaskStatus::Accepted;
                    return self.save(task, observe);
                }
                task.status = TaskStatus::Unresolved;
                task.summary = format!(
                    "No file changes proposed for an edit task. {}",
                    result.summary
                );
                self.save(task.clone(), observe)?;
                if result.finished {
                    break;
                }
                continue;
            }
            task.status = TaskStatus::Validating;
            self.save(task.clone(), observe)?;
            if task.checks.is_empty()
                && result
                    .touched
                    .iter()
                    .all(|p| matches!(p.extension().and_then(|s| s.to_str()), Some("md" | "txt")))
            {
                task.checks = vec!["git diff --check".into()];
            }
            task.checks = worker::effective_checks(&self.root, &task.checks, &result.touched);
            let validation =
                self.validate(&worker.overlay, &task.checks, cancel, task.policy, deadline)?;
            record_validation(&mut task, &validation);
            if cancel.load(Ordering::SeqCst) {
                task.status = TaskStatus::Cancelled;
                break;
            }
            if validation.is_empty() || validation.iter().any(|r| r.status != RunStatus::Passed) {
                task.status = TaskStatus::ValidationFailed;
                if validation.is_empty() {
                    task.summary =
                        format!("no Core validation command configured; {}", task.summary);
                }
                if validation.iter().any(|r| r.status == RunStatus::TimedOut)
                    && deadline.is_some_and(|end| Instant::now() >= end)
                {
                    task.summary = format!(
                        "validation exceeded the shared execution deadline; last proposal: {}",
                        task.summary
                    );
                }
                // Socket tests fail under the network-denied sandbox whatever the change
                // is: say so instead of presenting it as the model's failure.
                let denied = !task.policy.network
                    && validation
                        .iter()
                        .filter(|r| r.status != RunStatus::Passed)
                        .any(|r| {
                            self.store.get(&r.artifact, None).is_ok_and(|bytes| {
                                crate::effects::denied_by_sandbox(&String::from_utf8_lossy(&bytes))
                            })
                        });
                if denied {
                    task.summary = format!(
                        "Validation failed where the sandbox denied a socket or network call, which the change \
                         cannot fix. If these tests need localhost, allow it with LOOM_SANDBOX_LOOPBACK=1 \
                         (localhost only) or network access, then resume.\n\n{}",
                        task.summary
                    );
                }
                self.save(task.clone(), observe)?;
                if validation.is_empty()
                    || denied
                    || validation.iter().any(|r| r.status == RunStatus::TimedOut)
                {
                    // Environment and exhausted time are not repairable code failures.
                    break;
                }
                // finish ends the worker proposal, not the Core's task. With
                // attempts remaining, restore the patch and repair from the
                // independent validation failure instead of asking for resume.
                continue;
            }
            task.status = TaskStatus::AwaitingApproval;
            self.seal_patch(&mut task, &worker.overlay)?;
            let task = self.save(task, observe)?;
            drop(worker);
            return if options.approve {
                self.approve(&task.id, cancel, observe)
            } else {
                Ok(task)
            };
        }
        if task.status == TaskStatus::Unresolved
            && started.elapsed() >= Duration::from_secs(options.budget.seconds)
            && task.attempts.last().is_some_and(|a| {
                a.status != WorkerStatus::Proposed
                    && (a.summary == "budget exhausted"
                        || a.summary == "time budget exhausted"
                        || a.summary == "cancelled or timed out")
            })
        {
            let attempt = task.attempts.last().unwrap();
            task.summary = format!(
                "total execution time budget exhausted; {}ms model wait, {}ms reported gateway overhead, {} steps; last attempt: {}",
                attempt.model_wait_ms, attempt.gateway_overhead_ms, attempt.steps, task.summary
            );
        }
        self.save(task, observe)
    }
    /// What rewinding the code to `turn` would change, or why it cannot: shown before the
    /// user chooses between rewinding the conversation only or the conversation and code.
    pub fn rewind_preview(&self, session: &str, turn: &str) -> RewindPreview {
        let sessions = match crate::sessions::Sessions::open(&self.root) {
            Ok(sessions) => sessions,
            Err(error) => return RewindPreview::blocked(error.to_string()),
        };
        let (target, expected) = match sessions.rewind_target(session, turn) {
            Ok(pair) => pair,
            Err(_) => {
                return RewindPreview::blocked("no saved code checkpoint for this reply".into());
            }
        };
        match Revision::capture(&self.root) {
            Ok(current) if current == expected => {}
            Ok(_) => {
                return RewindPreview::blocked(
                    "files changed after the last Loom change; rewinding code would overwrite them"
                        .into(),
                );
            }
            Err(error) => return RewindPreview::blocked(error.to_string()),
        }
        match Patch::between(&self.root, &expected, &target) {
            Ok(patch) => {
                let text = String::from_utf8_lossy(&patch.diff);
                let lines = text.lines();
                let (mut added, mut removed) = (0, 0);
                for line in lines {
                    if line.starts_with('+') && !line.starts_with("+++") {
                        added += 1;
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        removed += 1;
                    }
                }
                RewindPreview {
                    files: patch
                        .files
                        .iter()
                        .map(|f| f.path.display().to_string())
                        .collect(),
                    added,
                    removed,
                    blocked: None,
                }
            }
            Err(error) => RewindPreview::blocked(error.to_string()),
        }
    }
    /// Rewind only the conversation: a new session branch ending at `turn`; files stay.
    pub fn rewind_conversation(
        &self,
        session: &str,
        turn: &str,
    ) -> Result<crate::sessions::Session> {
        crate::sessions::Sessions::open(&self.root)?.fork(session, turn)
    }
    pub fn rewind(&mut self, session: &str, turn: &str) -> Result<crate::sessions::Session> {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".loom/apply.lock"))?;
        lock.lock_exclusive()?;
        let sessions = crate::sessions::Sessions::open(&self.root)?;
        let (target, expected) = sessions.rewind_target(session, turn)?;
        if Revision::capture(&self.root)? != expected {
            bail!("workspace changed since the session checkpoint; preserve user edits");
        }
        let patch = Patch::between(&self.root, &expected, &target)?;
        patch.apply(&self.root)?;
        self.state.revision = Some(target.clone());
        self.log.append(&CoreEvent::RevisionUpdated(target))?;
        sessions.fork(session, turn)
    }
    fn validate(
        &self,
        overlay: &Overlay,
        checks: &[String],
        cancel: &AtomicBool,
        policy: crate::effects::ExecutionPolicy,
        deadline: Option<Instant>,
    ) -> Result<Vec<TestResult>> {
        let runner = Runner::new(self.store.clone());
        let mut results = vec![];
        for command in checks {
            let remaining = deadline.map_or(Duration::from_secs(300), |end| {
                end.saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(300))
            });
            let result = runner.run_with_policy(
                overlay.dir(),
                command,
                remaining,
                cancel,
                Some(&overlay.target_dir()),
                policy,
            )?;
            let passed = result.status == RunStatus::Passed;
            results.push(result);
            if !passed {
                break;
            }
        }
        Ok(results)
    }
    fn seal_patch(&self, task: &mut Task, overlay: &Overlay) -> Result<Patch> {
        let patch = overlay.patch()?;
        task.patch = Some(self.store.put(&serde_json::to_vec(&patch)?)?);
        if let Some(contract) = &task.contract {
            task.scope = Some(crate::context::monitor(
                contract,
                &patch.files.iter().map(|f| f.path.clone()).collect(),
            ));
        }
        Ok(patch)
    }
    pub fn reject(&mut self, id: &TaskId, mut observe: impl FnMut(CoreEvent)) -> Result<Task> {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".loom/apply.lock"))?;
        lock.lock_exclusive()?;
        self.state = Self::open(&self.root)?.state;
        let mut task = self.state.tasks.get(id).context("unknown task")?.clone();
        if task.status != TaskStatus::AwaitingApproval {
            bail!("task is not awaiting approval");
        }
        task.status = TaskStatus::Cancelled;
        self.save(task, &mut observe)
    }
    pub fn approve(
        &mut self,
        id: &TaskId,
        cancel: &AtomicBool,
        mut observe: impl FnMut(CoreEvent),
    ) -> Result<Task> {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join(".loom/apply.lock"))?;
        lock.lock_exclusive()?;
        // Another UI can have answered the same task: reload authoritative state.
        self.state = Self::open(&self.root)?.state;
        let mut task = self.state.tasks.get(id).context("unknown task")?.clone();
        if task.status == TaskStatus::Accepted {
            return Ok(task);
        }
        if task.status != TaskStatus::AwaitingApproval {
            bail!("task is not awaiting approval");
        }
        if cancel.load(Ordering::SeqCst) {
            task.status = TaskStatus::Cancelled;
            return self.save(task, &mut observe);
        }
        let proposal: Patch = serde_json::from_slice(
            &self
                .store
                .get(task.patch.as_ref().context("proposal missing")?, None)?,
        )?;
        let current = Revision::capture(&self.root)?;
        let overlay = Overlay::create(&self.root, &current)?;
        let applied = if current == proposal.base {
            proposal.apply(overlay.dir())
        } else {
            proposal.merge_into(&overlay)
        };
        if let Err(error) = applied {
            task.status = TaskStatus::ScopeConflict;
            task.summary = format!("{}\n{error}", task.summary);
            return self.save(task, &mut observe);
        }
        if current != proposal.base {
            let validation = self.validate(&overlay, &task.checks, cancel, task.policy, None)?;
            let passed =
                !validation.is_empty() && validation.iter().all(|r| r.status == RunStatus::Passed);
            record_validation(&mut task, &validation);
            if !passed {
                task.status = TaskStatus::ValidationFailed;
                return self.save(task, &mut observe);
            }
        }
        if cancel.load(Ordering::SeqCst) {
            task.status = TaskStatus::Cancelled;
            return self.save(task, &mut observe);
        }
        let patch = self.seal_patch(&mut task, &overlay)?;
        let expected = Revision::capture(overlay.dir())?;
        task.pending_apply = Some(expected.clone());
        self.save(task.clone(), &mut observe)?;
        if let Err(error) = patch.apply(&self.root) {
            task.pending_apply = None;
            task.status = TaskStatus::ScopeConflict;
            task.summary = format!("{}\n{error}", task.summary);
            return self.save(task, &mut observe);
        }
        task.pending_apply = None;
        task.status = TaskStatus::Accepted;
        self.state.revision = Some(expected.clone());
        let event = CoreEvent::RevisionUpdated(expected);
        self.log.append(&event)?;
        observe(event);
        self.save(task, &mut observe)
    }
}
fn refresh_reports(state: &mut CanonicalState) {
    state.findings = state
        .tasks
        .values()
        .filter(|t| t.readonly && t.status == TaskStatus::Accepted)
        .map(|t| t.summary.clone())
        .collect();
    state.failures = state
        .tasks
        .values()
        .filter(|t| {
            matches!(
                t.status,
                TaskStatus::Unresolved | TaskStatus::ValidationFailed | TaskStatus::ScopeConflict
            )
        })
        .map(|t| format!("{}: {:?}: {}", t.id, t.status, t.summary))
        .collect();
}

fn display_activity_report(report: &str) -> String {
    report
        .lines()
        .map(|line| {
            if line == "Progress:" {
                "진행 결과:".to_owned()
            } else if line == "no completed tool activity" {
                "실행 완료된 도구 기록 없음".to_owned()
            } else if let Some(rest) = line.strip_prefix("Isolated-copy change proposal: ") {
                format!(
                    "격리 사본 수정 제안: {}",
                    if rest == "none" { "없음" } else { rest }
                )
            } else if let Some(rest) = line.strip_prefix("Attempt: ") {
                format!(
                    "시도: {}",
                    rest.replace("model requests", "모델 요청")
                        .replace("tokens", "토큰")
                        .replace("elapsed", "경과")
                        .replace("model wait", "모델 대기")
                )
            } else if let Some(rest) = line.strip_prefix("Last model error: ") {
                format!("최근 모델 실패: {rest}")
            } else {
                line.replace(" · Result:", " · 결과:")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn record_validation(task: &mut Task, validation: &[TestResult]) {
    if let Some(attempt) = task.attempts.last_mut() {
        let cost = Cost {
            ms: validation.iter().map(|v| v.ms).sum(),
            usd: Some(0.0),
            ..Default::default()
        };
        let ledger = attempt
            .cost_by_phase
            .entry(Phase::Validate)
            .or_insert_with(|| Cost {
                usd: Some(0.0),
                ..Default::default()
            });
        ledger.accumulate(&cost);
        *attempt.phase_bytes.entry("validate".into()).or_default() +=
            validation.iter().map(|v| v.artifact.bytes).sum::<u64>();
        attempt.validation.extend_from_slice(validation);
    }
}
fn attempt(tier: Tier, record: ArtifactRef, result: &WorkerResult) -> Attempt {
    Attempt {
        tier,
        result: record,
        status: result.status.clone(),
        validation: vec![],
        summary: result.summary.clone(),
        cost_by_phase: result.cost_by_phase.clone(),
        input_tokens: result.input_tokens,
        remote_input_tokens: result.remote_input_tokens,
        remote_calls: result.remote_calls,
        model_wait_ms: result.model_wait_ms,
        gateway_overhead_ms: result.gateway_overhead_ms,
        cached_input_tokens: result.cached_input_tokens,
        output_tokens: result.output_tokens,
        steps: result.steps,
        rereads: result.rereads,
        phase_bytes: result.phase_bytes.clone(),
        models: result.models.clone(),
    }
}

fn legacy_policy() -> crate::effects::ExecutionPolicy {
    crate::effects::ExecutionPolicy {
        sandbox: crate::effects::SandboxMode::FullAccess,
        network: true,
    }
}
