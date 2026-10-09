use crate::agent::{
    Options,
    events::{Event, Observer, Stage},
    worker::{self, Shared},
    world::WorldHub,
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use crate::{
    base::Revision,
    effects::{Overlay, Patch, RunStatus, Runner, TestResult},
    models::{ModelPort, Usage},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Intent {
    Auto,
    Inspect,
    Edit,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Running,
    AwaitingApproval,
    Applying,
    Applied,
    Rejected,
    Answered,
    Inspected,
    Unresolved,
    Cancelled,
}
impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::AwaitingApproval => "AwaitingApproval",
            Self::Applying => "Applying",
            Self::Applied => "Applied",
            Self::Rejected => "Rejected",
            Self::Answered => "Answered",
            Self::Inspected => "Inspected",
            Self::Unresolved => "Unresolved",
            Self::Cancelled => "Cancelled",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub goal: String,
    pub status: Status,
    pub summary: String,
    pub patch: Patch,
    pub validation: Vec<TestResult>,
    pub calls: usize,
    pub usage: Usage,
    pub elapsed_ms: u64,
    pub delegated: bool,
    pub records: PathBuf,
    pub checks: Vec<String>,
    pub policy: crate::effects::ExecutionPolicy,
    #[serde(default)]
    pub approval_policy: crate::agent::ApprovalPolicy,
    pub expected: Option<Revision>,
}
impl Task {
    pub fn pending(&self) -> bool {
        self.status == Status::AwaitingApproval
    }
}
pub struct Core {
    root: PathBuf,
    tasks: PathBuf,
    hub: WorldHub,
}

fn migrate_legacy_tasks(legacy: &Path, current: &Path) -> Result<()> {
    if !legacy.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(legacy)? {
        let entry = entry?;
        let source = entry.path();
        let target = current.join(entry.file_name());
        if target.exists() {
            continue;
        }
        fs::rename(source, target)?;
    }
    Ok(())
}

impl Core {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.canonicalize()?;
        let tasks = root.join(".loom/tasks");
        fs::create_dir_all(&tasks)?;
        migrate_legacy_tasks(&root.join(".loom/v5/tasks"), &tasks)?;
        // Front ends open a Core per turn. The world hub (parsed symbols, the last
        // snapshot's files) is per repository and lives for the process, so the next turn
        // starts from it instead of re-reading and re-parsing the repository.
        static HUBS: std::sync::Mutex<Vec<(PathBuf, WorldHub)>> = std::sync::Mutex::new(Vec::new());
        let hub = {
            let mut hubs = HUBS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match hubs.iter().find(|(r, _)| *r == root) {
                Some((_, hub)) => hub.clone(),
                None => {
                    let hub = WorldHub::default();
                    hubs.push((root.clone(), hub.clone()));
                    hub
                }
            }
        };
        Ok(Self { root, tasks, hub })
    }
    pub fn context(
        &self,
        ids: &[String],
        budget: usize,
    ) -> Result<Vec<crate::models::Message>> {
        let ids = &ids[ids.len().saturating_sub(16)..];
        let tasks = ids
            .iter()
            .map(|id| self.load(id))
            .collect::<Result<Vec<_>>>()?;
        crate::agent::conversation::messages(&tasks, budget)
    }
    pub fn execute(
        &mut self,
        goal: &str,
        intent: Intent,
        mut options: Options,
        model: &dyn ModelPort,
        cancel: Arc<AtomicBool>,
        observe: &Observer<'_>,
    ) -> Result<Task> {
        ensure!(!cancel.load(Ordering::Acquire), "execution cancelled");
        ensure!(!goal.trim().is_empty(), "goal required");
        ensure!(
            options.steps > 0 && options.seconds > 0 && options.tokens > 0,
            "positive execution budget required"
        );
        ensure!(
            !options.read_only || intent != Intent::Edit,
            "read-only mode cannot edit"
        );
        if intent == Intent::Inspect {
            options.read_only = true;
        }
        if options.policy.sandbox == crate::effects::SandboxMode::ReadOnly {
            options.read_only = true;
        }
        ensure!(
            !options.read_only || intent != Intent::Edit,
            "read-only mode cannot edit"
        );
        if options.inquiry {
            options.read_only = true;
            options.plan = false;
        }
        ensure!(
            options.pinned.is_none() || options.read_only,
            "pinned inquiry is read-only"
        );
        let inferred_checks = options.checks.is_empty();
        if options.checks.is_empty() {
            options.checks = crate::worker::default_checks(&self.root);
        }
        let base = options
            .pinned
            .clone()
            .map(Ok)
            .unwrap_or_else(|| {
                observe(Event::new("main", Stage::Direct, "preparation",
                    json!({"phase":"revision","started":true})));
                Revision::capture_cancellable(&self.root, &cancel)
            })?;
        ensure!(!cancel.load(Ordering::Acquire), "execution cancelled");
        observe(Event::new("main", Stage::Direct, "preparation",
            json!({"phase":"overlay","started":true})));
        let mut overlay = Overlay::create_cancellable(&self.root, &base, &cancel)?;
        if cancel.load(Ordering::Acquire) {
            overlay.defer_cleanup();
            anyhow::bail!("execution cancelled");
        }
        let id = format!(
            "task-{:x}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            std::process::id()
        );
        let lease = self.lock(&format!("{id}.lock"))?;
        let mut task = Task {
            id: id.clone(),
            goal: goal.into(),
            status: Status::Running,
            summary: String::new(),
            patch: Patch { base: base.clone(), files: vec![], diff: vec![] },
            validation: vec![],
            calls: 0,
            usage: Usage::default(),
            elapsed_ms: 0,
            delegated: false,
            records: self.tasks.join(format!("{id}.json")),
            checks: options.checks.clone(),
            policy: options.policy,
            approval_policy: if options.auto_apply {
                crate::agent::ApprovalPolicy::Never
            } else {
                options.control.approval_policy()
            },
            expected: None,
        };
        self.save(&task)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.tasks.join(format!("{id}.events.jsonl")))?;
        if !options.inquiry {
            options.control.begin(base.clone(), goal, &id);
        }
        let may_have_edits = AtomicBool::new(false);
        let worker_observer = |event: Event| {
            if event.detail["started"] == true
                && matches!(event.detail["tool"].as_str(), Some("change" | "run" | "agent"))
            {
                may_have_edits.store(true, Ordering::Release);
            }
            observe(event);
        };
        let shared = Shared::new(
            &self.root,
            model,
            &options,
            self.hub.clone(),
            cancel.clone(),
            &worker_observer,
            log,
        );
        shared.emit(Event::new(
            "main",
            if options.plan {
                Stage::Planning
            } else {
                Stage::Direct
            },
            goal,
            json!({}),
        ));
        let result = worker::run(
            &shared,
            "main",
            goal,
            &mut overlay,
            options.inquiry.then(Default::default),
            options.plan,
        );
        let complete = match result {
            Ok(proposal) => {
                task.summary = proposal.summary;
                task.patch = proposal.patch;
                task.delegated = proposal.delegated;
                proposal.complete
            }
            Err(e) => {
                task.summary = e.to_string();
                // Before any potentially mutating tool, the initial empty
                // patch is exact. Avoid a full scan just to acknowledge stop.
                if !cancel.load(Ordering::Acquire) || may_have_edits.load(Ordering::Acquire) {
                    task.patch = overlay.patch()?;
                }
                false
            }
        };
        if !cancel.load(Ordering::Acquire) && inferred_checks && !task.patch.files.is_empty() {
            let touched = task.patch.files.iter().map(|f| f.path.clone()).collect();
            task.checks = crate::worker::effective_checks(
                overlay.dir(),
                &crate::worker::default_checks(overlay.dir()),
                &touched,
            );
            if task.checks.is_empty()
                && task.patch.files.iter().all(|f| {
                    matches!(
                        f.path.extension().and_then(|e| e.to_str()),
                        Some("md" | "txt")
                    )
                })
            {
                task.checks = vec!["git diff --check".into()];
            }
        }
        let metrics = shared.metrics();
        task.calls = metrics.calls;
        task.usage = metrics.usage;
        if cancel.load(Ordering::Acquire) {
            task.status = Status::Cancelled;
        } else if !complete || (intent == Intent::Edit && task.patch.files.is_empty()) {
            task.status = Status::Unresolved;
        } else if task.patch.files.is_empty() {
            task.status = if intent == Intent::Inspect {
                Status::Inspected
            } else {
                Status::Answered
            };
        } else {
            match validate(
                &overlay,
                &task.checks,
                task.policy,
                &cancel,
                shared.remaining(),
                &|event| shared.emit(event),
            ) {
                Ok(validation) => {
                    task.validation = validation;
                    task.status = if !task.validation.is_empty()
                        && task.validation.len() == task.checks.len()
                        && task
                            .validation
                            .iter()
                            .all(|r| r.status == RunStatus::Passed)
                    {
                        Status::AwaitingApproval
                    } else {
                        Status::Unresolved
                    };
                    if task.status == Status::Unresolved {
                        task.summary
                            .push_str("\nValidation did not pass; proposal remains isolated.");
                    }
                }
                Err(e) => {
                    task.status = Status::Unresolved;
                    task.summary.push_str(&format!("\nValidation error: {e}"));
                }
            }
        }
        if cancel.load(Ordering::Acquire) {
            task.status = Status::Cancelled;
            overlay.defer_cleanup();
        }
        if let Err(error) = shared.flush() {
            task.status = Status::Unresolved;
            task.summary
                .push_str(&format!("\nExecution log could not be saved: {error}"));
        }
        task.elapsed_ms = shared
            .started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.save(&task)?;
        if task.approval_policy == crate::agent::ApprovalPolicy::Never && task.pending() {
            task = match self.approve_locked(task.clone(), &cancel, observe) {
                Ok(applied) => applied,
                Err(error) => {
                    // Never must not turn a blocked automatic apply into a
                    // permission prompt, including after a UI restart.
                    let mut blocked = self.load(&task.id)?;
                    if blocked.pending() {
                        blocked.status = if cancel.load(Ordering::Acquire) {
                            Status::Cancelled
                        } else {
                            Status::Unresolved
                        };
                        blocked.summary.push_str(&format!("\nAutomatic apply blocked: {error}"));
                        self.save(&blocked)?;
                    }
                    return Err(error);
                }
            };
        }
        drop(lease);
        Ok(task)
    }
    pub fn approve(
        &mut self,
        id: &str,
        cancel: &AtomicBool,
        observe: &Observer<'_>,
    ) -> Result<Task> {
        valid_id(id)?;
        let _lease = self.lock(&format!("{id}.lock"))?;
        let task = self.load(id)?;
        self.approve_locked(task, cancel, observe)
    }
    /// The caller holds the task lease continuously, including automatic apply.
    fn approve_locked(
        &mut self,
        mut task: Task,
        cancel: &AtomicBool,
        observe: &Observer<'_>,
    ) -> Result<Task> {
        let id = task.id.clone();
        let _apply = self.lock("apply.lock")?;
        ensure!(task.pending(), "task is not awaiting approval");
        ensure!(!cancel.load(Ordering::Acquire), "approval cancelled");
        ensure!(
            Revision::capture(&self.root)? == task.patch.base,
            "workspace changed; preserve user changes and rerun"
        );
        let overlay = Overlay::create(&self.root, &task.patch.base)?;
        task.patch.apply(overlay.dir())?;
        match validate(
            &overlay,
            &task.checks,
            task.policy,
            cancel,
            Duration::from_secs(300),
            observe,
        ) {
            Ok(validation) => task.validation = validation,
            Err(e) => {
                task.summary
                    .push_str(&format!("\nApproval validation error: {e}"));
                self.save(&task)?;
                return Err(e);
            }
        }
        self.save(&task)?;
        ensure!(
            !task.validation.is_empty()
                && task.validation.len() == task.checks.len()
                && task
                    .validation
                    .iter()
                    .all(|r| r.status == RunStatus::Passed),
            "approval revalidation failed; proposal remains pending"
        );
        ensure!(!cancel.load(Ordering::Acquire), "approval cancelled");
        task.expected = Some(Revision::capture(overlay.dir())?);
        task.status = Status::Applying;
        self.save(&task)?;
        observe(Event::new(
            "main",
            Stage::Apply,
            &id,
            json!({"files":task.patch.files}),
        ));
        if let Err(e) = task.patch.apply(&self.root) {
            task.status = Status::AwaitingApproval;
            self.save(&task)?;
            return Err(e);
        }
        task.status = Status::Applied;
        self.save(&task)?;
        Ok(task)
    }
    pub fn reject(&mut self, id: &str) -> Result<()> {
        valid_id(id)?;
        let _lease = self.lock(&format!("{id}.lock"))?;
        let mut task = self.load(id)?;
        ensure!(task.pending(), "task is not awaiting approval");
        task.status = Status::Rejected;
        self.save(&task)
    }
    pub fn history(&self) -> Result<Vec<Task>> {
        let mut tasks = Vec::new();
        for entry in fs::read_dir(&self.tasks)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let mut task: Task = serde_json::from_slice(&fs::read(path)?)?;
            if (matches!(task.status, Status::Running | Status::Applying)
                || (task.pending() && task.approval_policy == crate::agent::ApprovalPolicy::Never))
                && let Ok(_lease) = self.lock(&format!("{}.lock", task.id))
            {
                // Execution may have finished after the initial read. Never
                // recover or overwrite a stale snapshot after taking its lease.
                task = self.load(&task.id)?;
                if matches!(task.status, Status::Running | Status::Applying)
                    || (task.pending() && task.approval_policy == crate::agent::ApprovalPolicy::Never)
                {
                    if task.status == Status::Applying
                        && task.expected.as_ref() == Some(&Revision::capture(&self.root)?)
                    {
                        task.status = Status::Applied;
                    } else {
                        task.status = Status::Unresolved;
                        task.summary =
                            "execution interrupted; rerun from the preserved workspace".into();
                    }
                    self.save(&task)?;
                }
            } else if task.pending() && task.approval_policy == crate::agent::ApprovalPolicy::Never {
                // Its lease is held by a live automatic apply. Surfaces must
                // not expose it as an actionable permission request.
                task.status = Status::Applying;
            }
            tasks.push(task);
        }
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(tasks)
    }
    fn load(&self, id: &str) -> Result<Task> {
        valid_id(id)?;
        serde_json::from_slice(&fs::read(self.tasks.join(format!("{id}.json")))?)
            .map_err(Into::into)
    }
    fn save(&self, task: &Task) -> Result<()> {
        valid_id(&task.id)?;
        let temporary = self
            .tasks
            .join(format!("{}.{}.tmp", task.id, std::process::id()));
        let mut file = File::create(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(task)?)?;
        file.sync_all()?;
        fs::rename(temporary, self.tasks.join(format!("{}.json", task.id)))?;
        File::open(&self.tasks)?.sync_all()?;
        Ok(())
    }
    fn lock(&self, name: &str) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.tasks.join(name))?;
        // A child process forked elsewhere (e.g. a background overlay removal) holds a
        // copy of every descriptor until its exec closes them; wait out that moment.
        for _ in 0..40 {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(file),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(e) => return Err(e.into()),
            }
        }
        file.try_lock_exclusive()
            .context("task or apply already running")?;
        Ok(file)
    }
}
fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid task id"
    );
    Ok(())
}
fn validate(
    overlay: &Overlay,
    checks: &[String],
    policy: crate::effects::ExecutionPolicy,
    cancel: &AtomicBool,
    time: Duration,
    observe: &Observer<'_>,
) -> Result<Vec<TestResult>> {
    let started = std::time::Instant::now();
    let mut results = Vec::new();
    let runner = Runner::new(overlay.artifacts().clone());
    let before = Revision::capture(overlay.dir())?;
    let listing = std::process::Command::new("git")
        .current_dir(overlay.dir())
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()?;
    ensure!(listing.status.success(), "cannot identify validation files");
    let mut absent_locks = std::collections::BTreeSet::new();
    for name in listing.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let path = Path::new(std::str::from_utf8(name)?);
        if path.file_name().is_some_and(|n| n == "Cargo.toml") {
            let lock = path.with_file_name("Cargo.lock");
            if !overlay.resolve(&lock)?.exists() {
                absent_locks.insert(lock);
            }
        }
    }
    for command in checks {
        if cancel.load(Ordering::Acquire) || started.elapsed() >= time {
            break;
        }
        observe(Event::new(
            "main",
            Stage::Check,
            command,
            json!({"started":true}),
        ));
        let result = runner.run_with_policy(
            overlay.dir(),
            command,
            time.saturating_sub(started.elapsed()),
            cancel,
            Some(&overlay.target_dir()),
            policy,
        )?;
        let ok = result.status == RunStatus::Passed;
        let mut event = Event::new(
            "main",
            Stage::Check,
            command,
            serde_json::to_value(&result)?,
        );
        event.failed = !ok;
        observe(event);
        results.push(result);
        if !ok {
            break;
        }
    }
    for lock in absent_locks {
        let path = overlay.resolve(&lock)?;
        if path.is_file() {
            let bytes = fs::read(&path)?;
            ensure!(
                bytes.starts_with(b"# This file is automatically @generated by Cargo."),
                "validation created an unexpected lockfile"
            );
            use crate::base::ArtifactStore;
            let artifact = overlay.artifacts().put(&bytes)?;
            observe(Event::new(
                "main",
                Stage::Check,
                lock.display().to_string(),
                json!({"generated_artifact":artifact}),
            ));
            fs::remove_file(path)?;
        }
    }
    ensure!(
        Revision::capture(overlay.dir())? == before,
        "validation modified source; proposal cannot be accepted"
    );
    Ok(results)
}
