//! Child workers share only the model port; every edit lives in its own overlay.
use super::{Budget, ToolSet, WorkerResult, WorkerTask};
use crate::{
    base::{ArtifactRef, ArtifactStore, Revision},
    effects::{ExecutionPolicy, Overlay},
    models::{ModelPort, Need},
    sessions::PlanStep,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{Scope, ScopedJoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct FileScope {
    writes: BTreeSet<PathBuf>,
    reads: BTreeSet<PathBuf>,
}

fn scoped_paths(args: &Value, key: &str) -> Result<BTreeSet<PathBuf>> {
    let Some(paths) = args.as_array() else {
        bail!("{key} must be an array of relative file paths");
    };
    let mut out = BTreeSet::new();
    for path in paths {
        let path = path.as_str().context("scope entries must be strings")?;
        let candidate = Path::new(path);
        if candidate.as_os_str().is_empty()
            || candidate.is_absolute()
            || candidate
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("invalid {key} path `{path}`: expected a relative file path");
        }
        out.insert(candidate.to_owned());
    }
    Ok(out)
}

fn requested_scope(args: &Value, readonly: bool) -> Result<Option<FileScope>> {
    if args.get("write_files").is_none() && args.get("read_files").is_none() {
        return Ok(None);
    }
    let writes = match args.get("write_files") {
        Some(value) => scoped_paths(value, "write_files")?,
        None => BTreeSet::new(),
    };
    let reads = match args.get("read_files") {
        Some(value) => scoped_paths(value, "read_files")?,
        None => BTreeSet::new(),
    };
    if readonly && !writes.is_empty() {
        bail!("read-only child cannot request write_files");
    }
    if !readonly && writes.is_empty() {
        bail!("scoped editing child must declare at least one write file");
    }
    Ok(Some(FileScope { writes, reads }))
}

fn overlaps(wanted: &FileScope, existing: &FileScope) -> bool {
    !wanted.writes.is_disjoint(&existing.writes)
        || !wanted.writes.is_disjoint(&existing.reads)
        || !wanted.reads.is_disjoint(&existing.writes)
}

struct Child<'scope> {
    goal: String,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Value>,
    handle: Option<ScopedJoinHandle<'scope, Result<WorkerResult>>>,
    result: Option<WorkerResult>,
    artifact: Option<ArtifactRef>,
    merged: bool,
    readonly: bool,
    scope: Option<FileScope>,
}
pub struct Agents<'scope, 'env> {
    scope: &'scope Scope<'scope, 'env>,
    model: &'env dyn ModelPort,
    root: PathBuf,
    need: Need,
    budget: Budget,
    cancel: &'env AtomicBool,
    deadline: Option<Instant>,
    checks: Vec<String>,
    policy: ExecutionPolicy,
    readonly: bool,
    parent_scope: Option<BTreeSet<PathBuf>>,
    depth: usize,
    control: super::WorkerControl,
    skills: Vec<PathBuf>,
    parent_model: Option<String>,
    reasoning_effort: Option<String>,
    context_limit: usize,
    automatic: bool,
    lifecycle: Vec<Value>,
    children: BTreeMap<String, Child<'scope>>,
    serial: usize,
}
impl<'scope, 'env> Agents<'scope, 'env> {
    /// A reservation persists until a proposed patch has been merged; finishing
    /// a worker alone must never release its write scope.
    fn live(&self) -> impl Iterator<Item = &Child<'scope>> {
        self.children.values().filter(|child| {
            !child.merged
                && (child.handle.is_some()
                    || child
                        .result
                        .as_ref()
                        .is_some_and(|r| r.status == super::WorkerStatus::Proposed))
        })
    }

    fn check_scope(&self, wanted: Option<&FileScope>, readonly: bool) -> Result<()> {
        if let Some(allowed) = &self.parent_scope {
            if !readonly && wanted.is_none() {
                bail!("WRITE_SCOPE_CONFLICT: scoped parent cannot spawn an unscoped editing child");
            }
            if let Some(wanted) = wanted
                && !wanted.writes.is_subset(allowed)
            {
                bail!("WRITE_SCOPE_CONFLICT: child's write_files exceed parent's scope");
            }
        }
        for child in self.live() {
            match (wanted, child.scope.as_ref()) {
                (Some(wanted), Some(existing)) => {
                    if overlaps(wanted, existing) {
                        bail!(
                            "WRITE_SCOPE_CONFLICT: requested read/write files overlap an outstanding child lease"
                        );
                    }
                }
                (Some(_), None) if !child.readonly => {
                    bail!("WRITE_SCOPE_CONFLICT: an unscoped editing child owns unknown files");
                }
                (None, Some(existing)) if !readonly && !existing.writes.is_empty() => {
                    bail!(
                        "WRITE_SCOPE_CONFLICT: an editing child must declare scope while scoped leases are outstanding"
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn parent_conflict(&self, name: &str, args: &Value) -> Option<String> {
        let leases: Vec<_> = self
            .live()
            .filter_map(|c| c.scope.as_ref())
            .filter(|s| !s.writes.is_empty())
            .collect();
        if leases.is_empty() {
            return None;
        }
        if matches!(
            name,
            "edit" | "write" | "replace_symbol" | "insert_after_symbol"
        ) {
            let path = if name.ends_with("symbol") {
                args["id"].as_str()
            } else {
                args["path"].as_str()
            }?;
            let file = path.split("::").next().unwrap_or_default();
            let file = Path::new(file.strip_prefix("./").unwrap_or(file));
            if file.is_absolute()
                || file
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
                || leases
                    .iter()
                    .any(|s| s.writes.contains(file) || s.reads.contains(file))
            {
                return Some(format!(
                    "WRITE_SCOPE_CONFLICT: `{}` is reserved by a child",
                    file.display()
                ));
            }
        } else if matches!(
            name,
            "rename_symbol" | "exec_command" | "write_stdin" | "check"
        ) {
            return Some("WRITE_SCOPE_CONFLICT: finish or merge scoped children before unbounded parent mutation".into());
        } else if !matches!(
            name,
            "map"
                | "plan_split"
                | "find"
                | "ast_search"
                | "outline"
                | "read"
                | "read_artifact"
                | "refs"
                | "finish"
                | "use_skill"
                | "read_skill_resource"
        ) {
            // Dynamic extension tools may write to arbitrary paths. Block them
            // while child file leases are outstanding, irrespective of name.
            return Some("WRITE_SCOPE_CONFLICT: external tool is unavailable while child write leases are outstanding".into());
        }
        None
    }

    pub fn new(
        scope: &'scope Scope<'scope, 'env>,
        model: &'env dyn ModelPort,
        task: &WorkerTask,
        cancel: &'env AtomicBool,
        started: Instant,
    ) -> Self {
        Self {
            scope,
            model,
            root: task.overlay.workspace().to_owned(),
            need: task.need,
            budget: task.budget,
            cancel,
            deadline: started.checked_add(Duration::from_secs(task.budget.seconds)),
            checks: task.checks.clone(),
            policy: task.policy,
            readonly: task.tools == ToolSet::ReadOnly,
            parent_scope: task.allowed_writes.clone(),
            depth: task.depth,
            control: task.control.clone(),
            skills: task.skills.clone(),
            parent_model: task.model.clone(),
            reasoning_effort: task.reasoning_effort.clone(),
            context_limit: task.context_limit,
            automatic: false,
            lifecycle: vec![],
            children: BTreeMap::new(),
            serial: 0,
        }
    }
    pub fn call(&mut self, name: &str, args: &Value, parent: &mut Overlay) -> Result<String> {
        self.collect_finished();
        match name {
            "spawn_agent" => {
                if self
                    .children
                    .values()
                    .filter(|c| c.handle.is_some())
                    .count()
                    >= 4
                {
                    bail!("at most four live children");
                }
                if self.depth >= 3 {
                    bail!("maximum child nesting depth reached");
                }
                let goal = args["goal"].as_str().context("goal required")?.to_owned();
                let readonly = self.readonly || args["read_only"].as_bool().unwrap_or(false);
                let scope = requested_scope(args, readonly)?;
                self.check_scope(scope.as_ref(), readonly)?;
                let base = Revision::capture(parent.dir())?;
                let context = if let Some(anchors) = args.get("anchors") {
                    let selected = scope
                        .as_ref()
                        .context("anchors require an explicit child scope")?;
                    let anchors = anchors
                        .as_array()
                        .context("anchors must be an array of symbol IDs")?
                        .iter()
                        .map(|a| a.as_str().map(ToOwned::to_owned))
                        .collect::<Option<Vec<_>>>()
                        .context("anchors must contain only strings")?;
                    let world = crate::world::World::open(parent.dir())?;
                    anyhow::ensure!(
                        world.snapshot.revision == base,
                        "repository moved while preparing child graph slice"
                    );
                    Some(
                        crate::world::split::scoped_slice(
                            &world,
                            &anchors,
                            &selected.writes,
                            &selected.reads,
                            8000,
                        )
                        .map_err(anyhow::Error::msg)?,
                    )
                } else {
                    None
                };
                let id = self.spawn(goal, base, readonly, scope, context)?;
                Ok(json!({"id":id,"status":"running"}).to_string())
            }
            "wait_agent" => {
                let id = args["id"].as_str().context("id required")?;
                let requested_deadline = std::time::Instant::now()
                    + std::time::Duration::from_millis(
                        args["timeout_ms"].as_u64().unwrap_or(30_000).min(30_000),
                    );
                let deadline = self
                    .deadline
                    .map_or(requested_deadline, |parent| parent.min(requested_deadline));
                loop {
                    if self.cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
                        break;
                    }
                    self.collect(id)?;
                    let child = self.children.get(id).context("unknown child")?;
                    if child.result.is_some() || std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(
                        Duration::from_millis(10)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                self.report(id)
            }
            "interrupt_agent" => {
                let id = args["id"].as_str().context("id required")?;
                self.children
                    .get(id)
                    .context("unknown child")?
                    .cancel
                    .store(true, Ordering::SeqCst);
                Ok(json!({"id":id,"status":"cancelling"}).to_string())
            }
            "send_agent" => {
                let id = args["id"].as_str().context("id required")?.to_owned();
                self.collect(&id)?;
                let child = self.children.get(&id).context("unknown child")?;
                if child.handle.is_some() {
                    bail!("interrupt or wait for a running child before sending follow-up");
                }
                if child.scope.is_some()
                    && !child.merged
                    && child
                        .result
                        .as_ref()
                        .is_some_and(|r| r.status == super::WorkerStatus::Proposed)
                {
                    bail!("merge previous scoped proposal before requesting a follow-up");
                }
                let previous_scope = child.scope.clone();
                let goal = format!(
                    "Previous child goal: {}\nPrevious result: {}\nFollow-up: {}",
                    child.goal,
                    child.result.as_ref().map_or("", |r| r.summary.as_str()),
                    args["text"].as_str().context("text required")?
                );
                let readonly = self.readonly || child.readonly;
                self.check_scope(previous_scope.as_ref(), readonly)?;
                let new_id = self.spawn(
                    goal,
                    Revision::capture(parent.dir())?,
                    readonly,
                    previous_scope,
                    None,
                )?;
                Ok(json!({"id":new_id,"status":"running","previous":id}).to_string())
            }
            "merge_agent" => {
                let id = args["id"].as_str().context("id required")?;
                if self.readonly {
                    bail!("readonly parent cannot merge edits");
                }
                self.collect(id)?;
                let child = self.children.get_mut(id).context("unknown child")?;
                let result = child.result.as_ref().context("child still running")?;
                if result.status != super::WorkerStatus::Proposed {
                    bail!("child did not produce a proposal");
                }
                if !child.merged {
                    if let Some(scope) = &child.scope {
                        let actual: BTreeSet<PathBuf> = result
                            .patch
                            .files
                            .iter()
                            .map(|file| file.path.clone())
                            .collect();
                        if actual != result.touched {
                            bail!(
                                "WRITE_SCOPE_CONFLICT: inconsistent child patch and touched-file list"
                            );
                        }
                        let illegal: Vec<_> = actual.difference(&scope.writes).collect();
                        if !illegal.is_empty() {
                            bail!(
                                "WRITE_SCOPE_CONFLICT: child changed files outside write_files: {}",
                                illegal
                                    .iter()
                                    .map(|path| path.display().to_string())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                        }
                    }
                    result.patch.merge_into(parent)?;
                    child.merged = true;
                    self.lifecycle.push(json!({"child":id,"event":{"agent_state":"merged","goal":child.goal,"summary":result.summary}}));
                }
                Ok(json!({"id":id,"merged":true,"files":result.patch.files}).to_string())
            }
            _ => bail!("unknown agent command"),
        }
    }
    pub fn directives(
        &mut self,
        control: &super::WorkerControl,
        parent: &mut Overlay,
    ) -> Result<()> {
        for directive in control.directives() {
            let id = directive
                .id
                .rsplit("-child-")
                .next()
                .map(|n| format!("child-{n}"))
                .unwrap_or(directive.id);
            if self.children.contains_key(&id) {
                if let Some(text) = directive.text {
                    let child = self.children.get_mut(&id).unwrap();
                    child.cancel.store(true, Ordering::SeqCst);
                    if let Some(handle) = child.handle.take() {
                        let result = handle
                            .join()
                            .map_err(|_| anyhow::anyhow!("child panicked"))??;
                        child.result = Some(result);
                    }
                    let _ = self.call("send_agent", &json!({"id":id,"text":text}), parent)?;
                } else {
                    self.children
                        .get(&id)
                        .unwrap()
                        .cancel
                        .store(true, Ordering::SeqCst);
                }
            }
        }
        Ok(())
    }
    fn spawn(
        &mut self,
        goal: String,
        base: Revision,
        readonly: bool,
        scope: Option<FileScope>,
        context: Option<String>,
    ) -> Result<String> {
        let seconds = self.deadline.map_or(self.budget.seconds, |deadline| {
            deadline.saturating_duration_since(Instant::now()).as_secs()
        });
        anyhow::ensure!(
            seconds > 0 && !self.cancel.load(Ordering::SeqCst),
            "parent execution window ended"
        );
        self.serial += 1;
        let id = format!("child-{}", self.serial);
        let overlay = Overlay::create(&self.root, &base)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let model = self.model;
        let (tx, rx) = mpsc::channel();
        let mut task = WorkerTask {
            goal: goal.clone(),
            guidance: crate::instructions::initial(&self.root, &goal, &[])?,
            images: vec![],
            skills: self.skills.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            model: self.parent_model.clone(),
            context_limit: self.context_limit,
            plan: Vec::<PlanStep>::new(),
            policy: if scope.is_some() {
                // Scoped workers' test commands can write to the runner's
                // explicit temp/build directories, never arbitrary workspace
                // or host paths. Core edit tools enforce allowed_writes.
                ExecutionPolicy {
                    sandbox: crate::effects::SandboxMode::ReadOnly,
                    network: self.policy.network,
                }
            } else {
                self.policy
            },
            depth: if self.automatic { 3 } else { self.depth + 1 },
            defer_validation_to_core: self.automatic,
            control: self.control.child(),
            overlay,
            context,
            supplied: vec![],
            handoff: None,
            artifacts: vec![],
            tools: if readonly {
                ToolSet::ReadOnly
            } else {
                ToolSet::Edit
            },
            tool_profile: super::ToolProfile::Progressive,
            allowed_writes: scope.as_ref().map(|s| s.writes.clone()),
            budget: Budget {
                seconds: seconds.min(self.budget.seconds),
                ..self.budget
            },
            need: self.need,
            checks: if self.automatic {
                vec![]
            } else {
                self.checks.clone()
            },
        };
        let handle = self.scope.spawn(move || {
            super::run(&mut task, model, &flag, |event| {
                let _ = tx.send(event);
            })
        });
        self.children.insert(
            id.clone(),
            Child {
                goal,
                cancel,
                rx,
                handle: Some(handle),
                result: None,
                artifact: None,
                merged: false,
                readonly,
                scope,
            },
        );
        self.lifecycle.push(
            json!({"child":id,"event":{"agent_state":"running","goal":self.children[&id].goal}}),
        );
        Ok(id)
    }
    fn collect(&mut self, id: &str) -> Result<()> {
        let child = self.children.get_mut(id).context("unknown child")?;
        if child.handle.as_ref().is_some_and(|h| h.is_finished()) {
            let result = child
                .handle
                .take()
                .unwrap()
                .join()
                .map_err(|_| anyhow::anyhow!("child panicked"))??;
            child.artifact = Some(
                crate::base::FileArtifactStore::open(&self.root)?
                    .put(&serde_json::to_vec(&result)?)?,
            );
            child.result = Some(result);
            let result = child.result.as_ref().unwrap();
            self.lifecycle.push(json!({"child":id,"event":{
                "agent_state":if result.summary == "cancelled" { "cancelled" }
                    else if result.status == super::WorkerStatus::Proposed { "proposed" } else { "failed" },
                "goal":child.goal,"summary":result.activity_report,"artifact":child.artifact
            }}));
        }
        Ok(())
    }
    fn report(&self, id: &str) -> Result<String> {
        let child = self.children.get(id).context("unknown child")?;
        Ok(json!({"id":id,"status":child.result.as_ref().map(|r|&r.status),"running":child.handle.is_some(),"summary":child.result.as_ref().map(|r|&r.summary),"artifact":child.artifact,"merged":child.merged,"requires_merge":child.result.as_ref().is_some_and(|r|!r.patch.files.is_empty()) && !child.merged,"files":child.result.as_ref().map(|r|&r.touched)}).to_string())
    }
    pub fn events(&mut self) -> Vec<Value> {
        self.collect_finished();
        let mut events = vec![];
        for (id, child) in &self.children {
            while let Ok(event) = child.rx.try_recv() {
                events.push(json!({"child":id,"event":event}));
            }
        }
        events.append(&mut self.lifecycle);
        events
    }
    fn collect_finished(&mut self) {
        let ids: Vec<_> = self
            .children
            .iter()
            .filter(|(_, child)| {
                child
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.is_finished())
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Err(error) = self.collect(&id) {
                self.lifecycle.push(json!({"child":id,"event":{"agent_state":"failed","summary":error.to_string()}}));
            }
        }
    }
    pub(super) fn parent_model(&mut self, model: Option<String>) {
        self.parent_model = model;
    }
    pub(super) fn automatic(&mut self, budget: Budget) {
        self.automatic = true;
        self.budget = budget;
    }
    pub(super) fn spawn_group(
        &mut self,
        group: &crate::world::split::Group,
        world: &crate::world::World,
        goal: &str,
    ) -> Result<String> {
        let scope = FileScope {
            writes: group.write_files.iter().map(PathBuf::from).collect(),
            reads: group.read_files.iter().map(PathBuf::from).collect(),
        };
        self.check_scope(Some(&scope), false)?;
        let context = crate::world::split::scoped_slice(
            world,
            &group.anchors,
            &scope.writes,
            &scope.reads,
            8000,
        )
        .map_err(anyhow::Error::msg)?;
        self.spawn(format!("Your task is ONLY the requested changes to these anchors: {}. Owned write files: {}\nOther workers own every other part. Do not search for or perform their changes. Read dependencies as needed. Finish with your scoped proposal; Core will merge and independently validate all parts. Do not run the parent task's full checks or delegate further.\n\nThe full parent request is reference material for desired behavior, not your task scope:\n{goal}", group.anchors.join(", "), group.write_files.join(", ")),
            world.snapshot.revision.clone(), false, Some(scope), Some(context))
    }
    pub(super) fn results(&self) -> Vec<&WorkerResult> {
        self.children
            .values()
            .filter_map(|child| child.result.as_ref())
            .collect()
    }
    pub(super) fn finish(&mut self) -> Result<()> {
        self.cancel_all();
        let ids: Vec<_> = self.children.keys().cloned().collect();
        let mut failure = None;
        for id in ids {
            while self.children[&id]
                .handle
                .as_ref()
                .is_some_and(|handle| !handle.is_finished())
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            if let Err(error) = self.collect(&id) {
                self.lifecycle.push(json!({"child":id,"event":{"agent_state":"failed","summary":error.to_string()}}));
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
    pub fn artifacts(&self) -> Vec<ArtifactRef> {
        self.children
            .values()
            .flat_map(|c| {
                c.artifact
                    .iter()
                    .cloned()
                    .chain(c.result.iter().flat_map(|r| {
                        r.artifacts
                            .iter()
                            .cloned()
                            .chain(std::iter::once(r.trace.clone()))
                    }))
            })
            .collect()
    }
    pub fn cancel_all(&self) {
        for child in self.children.values() {
            child.cancel.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for Agents<'_, '_> {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    #[test]
    fn explicit_scopes_require_relative_files_and_respect_read_write_conflicts() {
        for path in ["../outside.rs", "/tmp/outside.rs", "", "./"] {
            assert!(
                requested_scope(&json!({"write_files":[path]}), false).is_err(),
                "{path}"
            );
        }
        assert!(requested_scope(&json!({"read_files":["src/lib.rs"]}), false).is_err());
        assert!(requested_scope(&json!({"write_files":["src/lib.rs"]}), true).is_err());
        let make = |writes: &[&str], reads: &[&str]| FileScope {
            writes: writes.iter().map(PathBuf::from).collect(),
            reads: reads.iter().map(PathBuf::from).collect(),
        };
        let first = make(&["a.rs"], &["shared.rs"]);
        assert!(overlaps(&first, &make(&["shared.rs"], &[])));
        assert!(overlaps(&first, &make(&[], &["a.rs"])));
        assert!(overlaps(&first, &make(&["a.rs"], &[])));
        assert!(!overlaps(&first, &make(&["b.rs"], &["shared.rs"])));
    }

    #[test]
    fn final_merge_refuses_a_child_patch_containing_undeclared_files() {
        use crate::base::FileArtifactStore;
        use crate::models::ScriptedModel;
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(dir.path().join("lib.rs"), "pub fn stable() {}\n").unwrap();
        let base = Revision::capture(dir.path()).unwrap();
        let mut parent = Overlay::create(dir.path(), &base).unwrap();
        let child = Overlay::create(dir.path(), &base).unwrap();
        // Simulate an arbitrary side effect that bypassed Core's edit tool.
        std::fs::write(child.dir().join("undeclared.rs"), "fn unexpected() {}\n").unwrap();
        let patch = child.patch().unwrap();
        let touched = child.touched().unwrap();
        assert!(touched.contains(&PathBuf::from("undeclared.rs")));
        let trace = FileArtifactStore::open(dir.path())
            .unwrap()
            .put(b"trace")
            .unwrap();
        let result = WorkerResult {
            work_memory: Default::default(),
            activity_report: String::new(),
            last_model_error: None,
            status: super::super::WorkerStatus::Proposed,
            finished: true,
            answered: false,
            patch,
            touched,
            summary: "untrusted proposal".into(),
            plan: vec![],
            rereads: 0,
            cost_by_phase: BTreeMap::new(),
            trace,
            artifacts: vec![],
            steps: 0,
            input_tokens: 0,
            remote_input_tokens: Some(0),
            remote_calls: 0,
            model_wait_ms: 0,
            gateway_overhead_ms: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            phase_bytes: BTreeMap::new(),
            models: BTreeMap::new(),
            seconds: 0,
        };
        let model = ScriptedModel::new([]);
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let mut agents = Agents {
                scope,
                model: &model,
                root: dir.path().to_owned(),
                need: Need::default(),
                budget: Budget::default(),
                cancel: &cancel,
                deadline: None,
                checks: vec![],
                policy: ExecutionPolicy {
                    sandbox: crate::effects::SandboxMode::FullAccess,
                    network: false,
                },
                readonly: false,
                parent_scope: None,
                depth: 0,
                control: super::super::WorkerControl::default(),
                skills: vec![],
                parent_model: None,
                reasoning_effort: None,
                context_limit: 96000,
                automatic: false,
                lifecycle: vec![],
                children: BTreeMap::new(),
                serial: 1,
            };
            let (tx, rx) = mpsc::channel();
            drop(tx);
            agents.children.insert(
                "child-1".into(),
                Child {
                    goal: "bad patch".into(),
                    cancel: Arc::new(AtomicBool::new(false)),
                    rx,
                    handle: None,
                    result: Some(result),
                    artifact: None,
                    merged: false,
                    readonly: false,
                    scope: Some(FileScope {
                        writes: [PathBuf::from("lib.rs")].into(),
                        reads: BTreeSet::new(),
                    }),
                },
            );
            let err = agents
                .call("merge_agent", &json!({"id":"child-1"}), &mut parent)
                .unwrap_err();
            assert!(err.to_string().contains("WRITE_SCOPE_CONFLICT"), "{err}");
        });
        assert!(!parent.dir().join("undeclared.rs").exists());
        assert!(!dir.path().join("undeclared.rs").exists());
    }
}
