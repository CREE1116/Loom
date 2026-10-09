//! Native LOCAL execution around a transitional remote worker backend.
use super::graph::{CodeGraph, GraphIndex};
use super::repository::RepositoryExplorer;
use super::telemetry::{PhaseLedger, TelemetryLog, TurnCost};
use super::turns::TurnTasks;
use crate::agent::*;
use anyhow::{Result, bail};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Instant,
};

struct LocalJob {
    scope: Option<String>,
    projection: ActivityTask,
    started: Instant,
}
struct ExploreOutput {
    body: String,
    references: Vec<String>,
}

pub struct CoreBackend {
    remote: Box<dyn AgentBackend>,
    repository: Arc<RepositoryExplorer>,
    graph: Arc<GraphIndex>,
    events: VecDeque<AgentEvent>,
    results_tx: mpsc::Sender<(String, Result<ExploreOutput, String>)>,
    results_rx: mpsc::Receiver<(String, Result<ExploreOutput, String>)>,
    tasks: HashMap<String, LocalJob>,
    session: Option<String>,
    may_read_workspace: bool,
    serial: u64,
    ledger: PhaseLedger,
    telemetry: TelemetryLog,
    last_turn_cost: Option<TurnCost>,
    turns: TurnTasks,
    checkpoints: super::checkpoints::Checkpoints,
    checkpoint_tx: mpsc::Sender<(String, String)>,
    checkpoint_rx: mpsc::Receiver<(String, String)>,
    checkpoint_pending: HashSet<(String, String)>,
    deferred_submit: Option<(Option<String>, AgentCommand)>,
    checkpoint_cancel: Arc<AtomicBool>,
    checkpoint_stopped: mpsc::Receiver<()>,
    rewind: Option<(String, String, super::checkpoints::RestorePlan)>,
    rewind_notice: bool,
}

impl CoreBackend {
    pub fn new(
        remote: Box<dyn AgentBackend>,
        root: &Path,
        may_read_workspace: bool,
    ) -> Result<Self> {
        let (results_tx, results_rx) = mpsc::channel();
        let (checkpoint_tx, jobs) = mpsc::channel::<(String, String)>();
        let (completed, checkpoint_rx) = mpsc::channel();
        let (stopped, checkpoint_stopped) = mpsc::channel();
        let checkpoint_cancel = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&checkpoint_cancel);
        let mut checkpoints = super::checkpoints::Checkpoints::background(root, cancel.clone());
        std::thread::spawn(move || {
            loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let (session, turn) = match jobs.recv_timeout(std::time::Duration::from_millis(50))
                {
                    Ok(job) => job,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                // One worker preserves checkpoint order without blocking terminal input.
                let _ = checkpoints.record(&session, &turn);
                if completed.send((session, turn)).is_err() {
                    break;
                }
            }
            let _ = stopped.send(());
        });
        Ok(Self {
            remote,
            repository: Arc::new(RepositoryExplorer::new(root)?),
            graph: Arc::new(GraphIndex::default()),
            events: VecDeque::new(),
            results_tx,
            results_rx,
            tasks: HashMap::new(),
            session: None,
            may_read_workspace,
            serial: 0,
            ledger: PhaseLedger::default(),
            telemetry: TelemetryLog::new(&crate::runtime::state_dir(root)),
            last_turn_cost: None,
            turns: TurnTasks::default(),
            checkpoints: super::checkpoints::Checkpoints::new(root),
            checkpoint_tx,
            checkpoint_rx,
            checkpoint_pending: HashSet::new(),
            deferred_submit: None,
            checkpoint_cancel,
            checkpoint_stopped,
            rewind: None,
            rewind_notice: false,
        })
    }

    /// Every future worker receives a clone of this same handle.
    pub fn repository(&self) -> Arc<RepositoryExplorer> {
        Arc::clone(&self.repository)
    }

    /// Code graph of the current repository revision, shared like the repository index.
    pub fn code_graph(&self) -> Result<Arc<CodeGraph>> {
        let snapshot = self.repository.snapshot()?;
        self.graph.graph(&snapshot)
    }

    /// Phase split and usage of the most recent completed remote turn.
    pub fn last_turn_cost(&self) -> Option<&TurnCost> {
        self.last_turn_cost.as_ref()
    }

    pub fn telemetry_path(&self) -> &Path {
        self.telemetry.path()
    }

    fn emit(&mut self, session_id: Option<String>, update: AgentUpdate) {
        self.events.push_back(AgentEvent { session_id, update });
    }
}

impl AgentBackend for CoreBackend {
    fn command(&mut self, command: AgentCommand) -> Result<()> {
        if matches!(&command, AgentCommand::Interrupt)
            && let Some((scope, _)) = self.deferred_submit.take()
        {
            self.emit(
                scope,
                AgentUpdate::SubmissionFailed("코드 복원 기록을 기다리던 전송을 취소했습니다.".into()),
            );
            return Ok(());
        }
        if matches!(&command, AgentCommand::Submit { .. }) {
            self.checkpoint_results();
            if self.deferred_submit.is_some() {
                bail!("이미 전송을 기다리는 메시지가 있습니다.");
            }
            // Completion has no frozen revision. Accept one submission without
            // letting it edit the workspace until the preceding snapshot ends.
            if !self.checkpoint_pending.is_empty() {
                self.deferred_submit = Some((self.session.clone(), command));
                return Ok(());
            }
        }
        if let AgentCommand::PreviewRewind { turn_id } = &command {
            self.checkpoint_results();
            let preview = if !self.checkpoint_pending.is_empty() {
                Err(anyhow::anyhow!("코드 복원 기록을 만드는 중입니다. 잠시 뒤 다시 선택하세요."))
            } else if !self.may_read_workspace || !self.tasks.is_empty() {
                Err(anyhow::anyhow!("작업을 마친 뒤 코드를 되돌리세요."))
            } else {
                self.session.as_deref().ok_or_else(|| anyhow::anyhow!("연결된 세션이 없습니다."))
                    .and_then(|session| self.checkpoints.plan(session, turn_id))
            };
            let (files, added, removed, blocked) = match preview {
                Ok(plan) => (plan.paths.len(), plan.added, plan.removed, None),
                Err(error) => (0, 0, 0, Some(error.to_string())),
            };
            self.emit(self.session.clone(), AgentUpdate::RewindPreview { turn_id: turn_id.clone(), files, added, removed, blocked });
            return Ok(());
        }
        let command = if let AgentCommand::Rewind { turn_id, model } = command {
            self.checkpoint_results();
            if !self.checkpoint_pending.is_empty() {
                bail!("코드 복원 기록을 만드는 중입니다. 완료 후 다시 되돌리세요.");
            }
            if !self.may_read_workspace || !self.tasks.is_empty() {
                bail!("작업을 마친 뒤 코드를 되돌리세요.");
            }
            let session = self
                .session
                .clone()
                .ok_or_else(|| anyhow::anyhow!("연결된 세션이 없습니다."))?;
            let plan = self.checkpoints.plan(&session, &turn_id)?;
            self.remote.command(AgentCommand::Fork {
                turn_id: turn_id.clone(),
                model,
            })?;
            self.rewind = Some((session, turn_id, plan));
            return Ok(());
        } else {
            command
        };
        let AgentCommand::Explore(query) = command else {
            self.turns.on_command(&command);
            return self.remote.command(command);
        };
        if !self.may_read_workspace {
            bail!("폴더 실행 모드를 선택한 뒤 탐색하세요.");
        }
        if self.tasks.len() >= 4 {
            bail!("LOCAL 탐색 네 개가 실행 중입니다. 완료 후 다시 요청하세요.");
        }
        self.serial += 1;
        let id = format!("local-explore-{}", self.serial);
        let scope = self.session.clone();
        let projection = ActivityTask {
            id: id.clone(),
            parent: None,
            title: format!("공유 코드 탐색: {query}"),
            worker: Some(WorkerKind::Local),
            status: TaskStatus::Running,
            dependencies: vec![],
            reason: None,
            critical: false,
            elapsed_ms: None,
            inputs: vec![format!("Query: {query}")],
            outputs: vec![],
            ..Default::default()
        };
        self.tasks.insert(
            id.clone(),
            LocalJob {
                scope: scope.clone(),
                projection: projection.clone(),
                started: Instant::now(),
            },
        );
        self.emit(scope.clone(), AgentUpdate::TaskStarted { id: id.clone() });
        self.emit(scope.clone(), AgentUpdate::TaskUpdated(projection));
        self.emit(
            scope,
            AgentUpdate::EntryUpdated(Entry {
                id: id.clone(),
                kind: Kind::Tool,
                title: format!("LOCAL · 코드 탐색: {query}"),
                body: String::new(),
                status: "inProgress".into(),
                expanded: false,
            }),
        );
        let repository = Arc::clone(&self.repository);
        let tx = self.results_tx.clone();
        std::thread::spawn(move || {
            let result = repository
                .explore(&query, 4096)
                .map(|r| {
                    let (builds, searches) = repository.counts();
                    ExploreOutput {
                        body: format!(
                            "{}\nAPI 호출 0 · index builds {builds} · unique searches {searches}",
                            r.compact()
                        ),
                        references: r
                            .evidence
                            .iter()
                            .map(|item| item.reference.clone())
                            .collect(),
                    }
                })
                .map_err(|error| error.to_string());
            let _ = tx.send((id, result));
        });
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.deferred_submit = None;
        self.stop_checkpoints();
        self.remote.shutdown()
    }

    fn poll(&mut self) -> Result<Option<AgentEvent>> {
        self.checkpoint_results();
        if self.checkpoint_pending.is_empty()
            && let Some((scope, command)) = self.deferred_submit.take()
        {
            let result = if scope == self.session {
                self.remote.command(command.clone())
            } else {
                Err(anyhow::anyhow!(
                    "세션이 변경되어 기다리던 전송을 취소했습니다."
                ))
            };
            match result {
                Ok(()) => self.turns.on_command(&command),
                Err(error) => self.emit(scope, AgentUpdate::SubmissionFailed(error.to_string())),
            }
        }
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        if let Ok((id, result)) = self.results_rx.try_recv() {
            if let Some(mut job) = self.tasks.remove(&id) {
                let scope = job.scope;
                if scope != self.session {
                    self.emit(scope, AgentUpdate::TaskCompleted { id });
                    return Ok(self.events.pop_front());
                }
                let (body, status) = match result {
                    Ok(output) => {
                        job.projection.status = TaskStatus::Completed;
                        job.projection.outputs = output.references;
                        (output.body, "completed")
                    }
                    Err(error) => {
                        job.projection.status = TaskStatus::Failed;
                        job.projection.reason = Some(error.clone());
                        (error, "failed")
                    }
                };
                job.projection.elapsed_ms =
                    Some(job.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
                self.emit(scope.clone(), AgentUpdate::TaskUpdated(job.projection));
                self.emit(
                    scope.clone(),
                    AgentUpdate::EntryUpdated(Entry {
                        id: id.clone(),
                        kind: Kind::Tool,
                        title: "LOCAL · 공유 코드 탐색".into(),
                        body,
                        status: status.into(),
                        expanded: true,
                    }),
                );
                self.emit(scope, AgentUpdate::TaskCompleted { id });
            }
            return Ok(self.events.pop_front());
        }
        let event = self.remote.poll()?;
        if self.rewind_notice && event.as_ref().is_some_and(|event| matches!(&event.update, AgentUpdate::Notice(message) if message.starts_with("새 분기 시작") || message.starts_with("새 데모 분기"))) {
            self.rewind_notice = false;
            return Ok(None);
        }
        if let Some(event) = &event {
            if let Some(cost) = self
                .ledger
                .observe(event.session_id.as_deref(), &event.update)
            {
                if let Err(error) = self.telemetry.append(&cost) {
                    self.emit(
                        None,
                        AgentUpdate::Notice(format!("비용 기록 실패: {error}")),
                    );
                }
                self.last_turn_cost = Some(cost);
            }
            match &event.update {
                AgentUpdate::SessionLoaded {
                    snapshot,
                    switching,
                    ..
                } => {
                    if *switching {
                        if let Some((old, turn, plan)) = self.rewind.take() {
                            self.rewind_notice = true;
                            let result = if snapshot.id == old {
                                Err(anyhow::anyhow!(
                                    "원본과 같은 세션이 반환되어 코드 복원을 취소했습니다."
                                ))
                            } else {
                                self.checkpoints.restore(&plan).and_then(|_| {
                                    self.checkpoints.inherit(&old, &snapshot.id, &turn)
                                })
                            };
                            self.emit(
                                None,
                                match result {
                                    Ok(()) => AgentUpdate::Notice(format!(
                                        "대화와 코드 복원 완료 · {}개 파일",
                                        plan.paths.len()
                                    )),
                                    Err(error) => AgentUpdate::Error(format!(
                                        "코드 복원 실패 · 대화 분기는 생성됨: {error}"
                                    )),
                                },
                            );
                        }
                    }
                    self.session = Some(snapshot.id.clone())
                }
                AgentUpdate::TurnCompleted { id: Some(turn), .. } => {
                    if self.may_read_workspace {
                        if let Some(session) = self.session.clone() {
                            // Missing checkpoints are reported only when a rewind is requested.
                            let job = (session, turn.clone());
                            if self.checkpoint_pending.insert(job.clone())
                                && self.checkpoint_tx.send(job.clone()).is_err()
                            {
                                self.checkpoint_pending.remove(&job);
                            }
                        }
                    }
                }
                AgentUpdate::Error(_) | AgentUpdate::Disconnected(_) => self.rewind = None,
                AgentUpdate::TrustResolved(_) => self.may_read_workspace = true,
                AgentUpdate::TrustRequired(_) => self.may_read_workspace = false,
                _ => {}
            }
        }
        Ok(event)
    }
}

impl CoreBackend {
    fn stop_checkpoints(&self) {
        self.checkpoint_cancel.store(true, Ordering::Relaxed);
        // Let the worker kill its private Git process before the app exits.
        let _ = self
            .checkpoint_stopped
            .recv_timeout(std::time::Duration::from_millis(250));
    }
    fn checkpoint_results(&mut self) {
        let mut changed = false;
        while let Ok(job) = self.checkpoint_rx.try_recv() {
            self.checkpoint_pending.remove(&job);
            changed = true;
        }
        if changed {
            self.checkpoints.reload();
        }
    }
}

impl Drop for CoreBackend {
    fn drop(&mut self) {
        self.stop_checkpoints();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::App, engine::Action};
    use std::{fs, path::PathBuf, process::Command, sync::Mutex, time::Duration};

    struct EditingRemote {
        root: PathBuf,
        commands: Arc<Mutex<Vec<AgentCommand>>>,
        events: VecDeque<AgentEvent>,
        fail_submit: bool,
    }

    impl AgentBackend for EditingRemote {
        fn command(&mut self, command: AgentCommand) -> Result<()> {
            self.commands.lock().unwrap().push(command.clone());
            if matches!(command, AgentCommand::Submit { .. }) {
                if self.fail_submit {
                    bail!("test dispatch failed");
                }
                fs::write(self.root.join("code"), "next turn edits")?;
            }
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<AgentEvent>> {
            Ok(self.events.pop_front())
        }
    }

    struct PausedCheckpoint {
        dir: tempfile::TempDir,
        core: CoreBackend,
        commands: Arc<Mutex<Vec<AgentCommand>>>,
        completed: mpsc::Sender<(String, String)>,
        job: (String, String),
    }

    impl PausedCheckpoint {
        fn new(fail_submit: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            assert!(
                Command::new("git")
                    .args(["init", "-q"])
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
            fs::write(dir.path().join("code"), "prior turn edits").unwrap();
            let commands = Arc::new(Mutex::new(Vec::new()));
            let remote = EditingRemote {
                root: dir.path().to_owned(),
                commands: Arc::clone(&commands),
                fail_submit,
                events: VecDeque::from([AgentEvent::scoped(
                    "main",
                    AgentUpdate::TurnCompleted {
                        id: Some("prior".into()),
                        status: "completed".into(),
                        error: None,
                    },
                )]),
            };
            let mut core = CoreBackend::new(Box::new(remote), dir.path(), true).unwrap();
            core.session = Some("main".into());
            // Hold the dispatched job before hashing; no sleep or Git timing race.
            let (dispatch, jobs) = mpsc::channel();
            let (completed, results) = mpsc::channel();
            core.checkpoint_tx = dispatch;
            core.checkpoint_rx = results;
            assert!(matches!(
                core.poll().unwrap().unwrap().update,
                AgentUpdate::TurnCompleted { .. }
            ));
            let job = jobs.try_recv().unwrap();
            Self {
                dir,
                core,
                commands,
                completed,
                job,
            }
        }

        fn finish_snapshot(&self) {
            let root = self.dir.path().to_owned();
            let completed = self.completed.clone();
            let job = self.job.clone();
            std::thread::spawn(move || {
                let mut checkpoints = super::super::checkpoints::Checkpoints::background(
                    &root,
                    Arc::new(AtomicBool::new(false)),
                );
                checkpoints.record(&job.0, &job.1).unwrap();
                completed.send(job).unwrap();
            })
            .join()
            .unwrap();
        }

        fn bytes(&self) -> String {
            fs::read_to_string(self.dir.path().join("code")).unwrap()
        }
    }

    fn submission(app: &mut App) -> AgentCommand {
        app.thread = Some("main".into());
        app.model = "test".into();
        app.editor.insert("edit code");
        let intent = app.activate(Action::Send).unwrap();
        app.agent_command(&intent).unwrap()
    }

    fn next_task_title(core: &mut CoreBackend) -> String {
        let projection = core.turns.on_event(
            &AgentEvent::scoped(
                "main",
                AgentUpdate::TurnStarted {
                    id: Some("probe".into()),
                },
            ),
            None,
            || unreachable!("turn start does not need a code graph"),
        );
        projection
            .into_iter()
            .find_map(|event| match event.update {
                AgentUpdate::TaskUpdated(task) => Some(task.title),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn checkpoint_boundary_defers_next_submit_until_snapshot_finishes() {
        let mut fixture = PausedCheckpoint::new(false);
        let mut app = App::new(false);
        let command = submission(&mut app);
        let start = Instant::now();
        fixture.core.command(command.clone()).unwrap();
        app.editor.insert("second queued");
        assert!(app.activate(Action::Send).is_none());
        app.editor.insert("third queued");
        assert!(app.activate(Action::Send).is_none());
        app.editor.insert("next draft");
        assert!(!app.queue_paused);
        assert_eq!(app.submitted.as_deref(), Some("edit code"));
        assert_eq!(app.queued[0].text, "second queued");
        assert_eq!(app.queued[1].text, "third queued");
        assert!(
            fixture.core.command(command).is_err(),
            "a second deferred command must not replace the first"
        );
        assert_eq!(
            next_task_title(&mut fixture.core),
            "작업 1",
            "waiting must not update TurnTasks"
        );
        fixture.core.command(AgentCommand::RefreshModels).unwrap();
        assert!(fixture.core.poll().unwrap().is_none());
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "pending snapshot blocked terminal input"
        );
        assert_eq!(fixture.commands.lock().unwrap().len(), 1);
        assert_eq!(fixture.bytes(), "prior turn edits");

        fixture.finish_snapshot();
        assert_eq!(
            fixture.bytes(),
            "prior turn edits",
            "only poll may dispatch the waiting submission"
        );
        assert!(fixture.core.poll().unwrap().is_none());
        assert_eq!(next_task_title(&mut fixture.core), "edit code");
        assert!(fixture.core.poll().unwrap().is_none());
        assert_eq!(
            fixture.commands.lock().unwrap().len(),
            2,
            "dispatch exactly once"
        );
        assert_eq!(fixture.bytes(), "next turn edits");
        assert!(!app.queue_paused);
        assert_eq!(app.editor.text, "next draft");
        let saved: serde_json::Value = serde_json::from_slice(
            &fs::read(crate::runtime::state_dir(fixture.dir.path()).join("checkpoints.json"))
                .unwrap(),
        )
        .unwrap();
        let tree = saved["main"][0]["tree"].as_str().unwrap();
        let prior = Command::new("git")
            .args(["show", &format!("{tree}:code")])
            .current_dir(fixture.dir.path())
            .output()
            .unwrap();
        assert!(prior.status.success());
        assert_eq!(prior.stdout, b"prior turn edits");
    }

    #[test]
    fn checkpoint_boundary_interrupt_cancels_deferred_submit_even_with_ready_snapshot() {
        for ready in [false, true] {
            let mut fixture = PausedCheckpoint::new(false);
            let mut app = App::new(false);
            let command = submission(&mut app);
            fixture.core.command(command).unwrap();
            if ready {
                fixture.finish_snapshot();
            }
            fixture.core.command(AgentCommand::Interrupt).unwrap();
            let event = fixture.core.poll().unwrap().unwrap();
            assert!(
                matches!(&event.update, AgentUpdate::SubmissionFailed(reason) if reason.contains("취소"))
            );
            app.apply_event(event);
            assert_eq!(app.queued[0].text, "edit code");
            assert!(app.queue_paused && app.submitted.is_none());
            if !ready {
                fixture.finish_snapshot();
            }
            assert!(fixture.core.poll().unwrap().is_none());
            assert!(fixture.core.poll().unwrap().is_none());
            assert!(
                fixture.commands.lock().unwrap().is_empty(),
                "cancelled work must never reach the remote"
            );
            assert_eq!(fixture.bytes(), "prior turn edits");
            assert_eq!(next_task_title(&mut fixture.core), "작업 1");
        }
    }

    #[test]
    fn checkpoint_boundary_delayed_dispatch_failure_rolls_back_submission() {
        let mut fixture = PausedCheckpoint::new(true);
        let mut app = App::new(false);
        let command = submission(&mut app);
        fixture.core.command(command).unwrap();
        fixture.finish_snapshot();
        let event = fixture.core.poll().unwrap().unwrap();
        assert!(
            matches!(&event.update, AgentUpdate::SubmissionFailed(reason) if reason == "test dispatch failed")
        );
        app.apply_event(event);
        assert_eq!(app.queued[0].text, "edit code");
        assert_eq!(app.queued[0].model, "test");
        assert!(app.queue_paused && app.submitted.is_none());
        assert!(fixture.core.poll().unwrap().is_none());
        assert_eq!(fixture.commands.lock().unwrap().len(), 1);
        assert_eq!(fixture.bytes(), "prior turn edits");
        assert_eq!(next_task_title(&mut fixture.core), "작업 1");
    }
}
