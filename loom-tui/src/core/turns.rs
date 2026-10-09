//! Core-owned task projection for remote turns.
//!
//! Until Core plans work itself, one remote turn is one task. Core confirms it when the
//! turn starts, links sessions the runtime spawns for it, and on completion records the
//! boundary that actually changed (diff → files → code-graph symbols and groups) and the
//! measured cost. Tasks are kept per session so they survive switching away and back.
use super::graph::CodeGraph;
use super::telemetry::TurnCost;
use crate::agent::*;
use std::{collections::HashMap, sync::Arc, time::Instant};

const TITLE_CHARS: usize = 60;

struct OpenTurn {
    session: String,
    task: String,
    started: Instant,
    diff: String,
}

#[derive(Default)]
pub struct TurnTasks {
    pending_title: Option<String>,
    open: Option<OpenTurn>,
    tasks: HashMap<String, Vec<ActivityTask>>,
    serial: u64,
}

impl TurnTasks {
    pub fn on_command(&mut self, command: &AgentCommand) {
        if let AgentCommand::Submit { text, .. } = command {
            self.pending_title = Some(text.clone());
        }
    }

    /// Observe one remote event; returns projection events to emit after it.
    pub fn on_event(
        &mut self,
        event: &AgentEvent,
        cost: Option<&TurnCost>,
        graph: impl FnOnce() -> anyhow::Result<Arc<CodeGraph>>,
    ) -> Vec<AgentEvent> {
        let session = event.session_id.clone();
        match &event.update {
            AgentUpdate::SessionLoaded { snapshot, .. } => self
                .tasks
                .get(&snapshot.id)
                .into_iter()
                .flatten()
                .map(|t| AgentEvent::scoped(&snapshot.id, AgentUpdate::TaskUpdated(t.clone())))
                .collect(),
            AgentUpdate::TurnStarted { id } => {
                let Some(session) = session else {
                    return Vec::new();
                };
                self.serial += 1;
                let task_id = format!(
                    "turn-{}",
                    id.clone().unwrap_or_else(|| self.serial.to_string())
                );
                let title = self
                    .pending_title
                    .take()
                    .map(|t| one_line(&t))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| format!("작업 {}", self.serial));
                let task = ActivityTask {
                    id: task_id.clone(),
                    title,
                    status: TaskStatus::Running,
                    session: Some(session.clone()),
                    inputs: vec!["Worker: 연결된 세션".into()],
                    ..ActivityTask::default()
                };
                self.open = Some(OpenTurn {
                    session: session.clone(),
                    task: task_id.clone(),
                    started: Instant::now(),
                    diff: String::new(),
                });
                vec![
                    AgentEvent::scoped(&session, AgentUpdate::TaskStarted { id: task_id }),
                    self.store(&session, task),
                ]
            }
            AgentUpdate::ActivityUpdated(agent) => {
                let Some(open) = &self.open else {
                    return Vec::new();
                };
                let (owner, parent) = (open.session.clone(), open.task.clone());
                let id = format!("agent-{}", agent.id);
                let existing = self.find(&owner, &id).cloned();
                let task = ActivityTask {
                    id,
                    parent: Some(parent),
                    title: if agent.name.is_empty() {
                        existing
                            .as_ref()
                            .map(|t| t.title.clone())
                            .unwrap_or_else(|| "하위 에이전트".into())
                    } else {
                        format!("하위 에이전트 · {}", agent.name)
                    },
                    status: agent_status(&agent.status),
                    session: Some(agent.id.clone()),
                    reason: (!agent.detail.is_empty()).then(|| agent.detail.clone()),
                    inputs: vec!["연결된 런타임이 만든 하위 세션".into()],
                    ..ActivityTask::default()
                };
                vec![self.store(&owner, task)]
            }
            AgentUpdate::DiffUpdated(diff) => {
                if let Some(open) = &mut self.open {
                    open.diff.clone_from(diff);
                }
                Vec::new()
            }
            AgentUpdate::TurnCompleted { status, error, .. } => {
                let Some(open) = self.open.take() else {
                    return Vec::new();
                };
                let Some(mut task) = self.find(&open.session, &open.task).cloned() else {
                    return Vec::new();
                };
                task.status = match status.as_str() {
                    "completed" => TaskStatus::Completed,
                    "interrupted" | "cancelled" => TaskStatus::Cancelled,
                    _ => TaskStatus::Failed,
                };
                task.reason = error.clone();
                task.elapsed_ms =
                    Some(open.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
                task.partition = Some(Box::new(partition(&open.diff, graph().ok().as_deref())));
                task.cost = cost.map(task_cost);
                let id = task.id.clone();
                vec![
                    self.store(&open.session, task),
                    AgentEvent::scoped(&open.session, AgentUpdate::TaskCompleted { id }),
                ]
            }
            _ => Vec::new(),
        }
    }

    fn find(&self, session: &str, id: &str) -> Option<&ActivityTask> {
        self.tasks.get(session)?.iter().find(|t| t.id == id)
    }

    fn store(&mut self, session: &str, task: ActivityTask) -> AgentEvent {
        let list = self.tasks.entry(session.to_owned()).or_default();
        match list.iter_mut().find(|t| t.id == task.id) {
            Some(old) => *old = task.clone(),
            None => list.push(task.clone()),
        }
        AgentEvent::scoped(session, AgentUpdate::TaskUpdated(task))
    }
}

fn one_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut out: String = line.chars().take(TITLE_CHARS).collect();
    if line.chars().count() > TITLE_CHARS {
        out.push('…');
    }
    out
}

fn agent_status(status: &str) -> TaskStatus {
    match status {
        "completed" | "idle" => TaskStatus::Completed,
        "failed" | "errored" => TaskStatus::Failed,
        "interrupted" | "cancelled" | "shutdown" => TaskStatus::Cancelled,
        _ => TaskStatus::Running,
    }
}

fn task_cost(cost: &TurnCost) -> TaskCost {
    TaskCost {
        orient_bytes: cost.bytes.orient,
        validate_bytes: cost.bytes.validate,
        edit_bytes: cost.bytes.edit,
        message_bytes: cost.bytes.message,
        input_tokens: cost.tokens.input,
        cached_input_tokens: cost.tokens.cached_input,
        output_tokens: cost.tokens.output,
        model_calls: cost.tokens.reports,
    }
}

/// New-side line numbers each file's diff touches (pure deletions mark their position).
fn changed_lines(diff: &str) -> Vec<(String, Vec<usize>)> {
    let mut files: Vec<(String, Vec<usize>)> = Vec::new();
    let mut line = 0usize;
    for text in diff.lines() {
        if let Some(path) = text.strip_prefix("+++ ") {
            if path != "/dev/null" {
                files.push((
                    path.strip_prefix("b/").unwrap_or(path).to_owned(),
                    Vec::new(),
                ));
            }
        } else if let Some(rest) = text.strip_prefix("--- ") {
            // A deleted file has no new side; remember it by its old path.
            if rest != "/dev/null" && diff.contains(&format!("{text}\n+++ /dev/null")) {
                files.push((
                    rest.strip_prefix("a/").unwrap_or(rest).to_owned(),
                    Vec::new(),
                ));
            }
        } else if let Some(hunk) = text.strip_prefix("@@ ") {
            line = hunk
                .split_whitespace()
                .find_map(|part| part.strip_prefix('+'))
                .and_then(|range| range.split(',').next())
                .and_then(|start| start.parse().ok())
                .unwrap_or(0);
        } else if text.starts_with('-') {
            // A deletion lands between new-side lines; attribute it to the item there.
            if let Some((_, lines)) = files.last_mut() {
                lines.push(line.max(1));
            }
        } else if text.starts_with('+') {
            if let Some((_, lines)) = files.last_mut() {
                lines.push(line);
            }
            line += 1;
        } else if text.starts_with(' ') {
            line += 1;
        }
    }
    files
}

pub fn partition(diff: &str, graph: Option<&CodeGraph>) -> TaskPartition {
    let changes = changed_lines(diff);
    let files: Vec<String> = changes.iter().map(|(f, _)| f.clone()).collect();
    let mut symbols: Vec<String> = Vec::new();
    if let Some(graph) = graph {
        for (file, lines) in &changes {
            for &line in lines {
                if let Some(symbol) = graph.symbol_at(file, line)
                    && !symbol.is_pseudo()
                    && !symbols.contains(&symbol.id)
                {
                    symbols.push(symbol.id.clone());
                }
            }
        }
    }
    let groups = graph.map(|g| groups(g, &symbols)).unwrap_or_default();
    let reason = if files.is_empty() {
        "파일 변경 없음 — 단일 실행".to_owned()
    } else if graph.is_none() {
        "code graph를 만들지 못해 symbol 경계 없이 기록 — 단일 실행".to_owned()
    } else if groups.len() >= 2 {
        format!(
            "graph상 독립 덩어리 {}개 — Split Gate 도입 전이라 단일 실행 (PR 규모에선 분할 이득이 측정되지 않음)",
            groups.len()
        )
    } else {
        "변경이 하나로 연결됨 — 단일 실행".to_owned()
    };
    TaskPartition {
        mode: ExecutionMode::Single,
        reason,
        files,
        symbols,
        groups,
    }
}

/// Weakly connected components of `symbols` under resolved references.
fn groups(graph: &CodeGraph, symbols: &[String]) -> Vec<Vec<String>> {
    let index: HashMap<&str, usize> = symbols
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let mut parent: Vec<usize> = (0..symbols.len()).collect();
    fn root(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for (i, id) in symbols.iter().enumerate() {
        for target in graph.resolve(id) {
            if let Some(&j) = index.get(target.id.as_str()) {
                let (a, b) = (root(&mut parent, i), root(&mut parent, j));
                parent[a] = b;
            }
        }
    }
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut slot: HashMap<usize, usize> = HashMap::new();
    for (i, id) in symbols.iter().enumerate() {
        let r = root(&mut parent, i);
        let k = *slot.entry(r).or_insert_with(|| {
            out.push(Vec::new());
            out.len() - 1
        });
        out[k].push(id.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{graph::GraphIndex, repository::RepositoryExplorer};

    fn repo() -> (tempfile::TempDir, Arc<CodeGraph>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            // The tree after the change: graphs are built on the post-turn revision.
            "fn helper() {}\nfn run() {\n    helper();\n    helper();\n}\nfn other() {}\n",
        )
        .unwrap();
        let graph = GraphIndex::default()
            .graph(
                &RepositoryExplorer::new(dir.path())
                    .unwrap()
                    .snapshot()
                    .unwrap(),
            )
            .unwrap();
        (dir, graph)
    }

    const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -2,2 +2,3 @@ fn run() {\n     helper();\n+    helper();\n }\n@@ -5 +6 @@\n-fn other() { }\n+fn other() {}\ndiff --git a/CHANGELOG.md b/CHANGELOG.md\n--- a/CHANGELOG.md\n+++ b/CHANGELOG.md\n@@ -1 +1,2 @@\n+- fix\n";

    #[test]
    fn partition_maps_diff_to_symbols_and_independent_groups() {
        let (_dir, graph) = repo();
        let p = partition(DIFF, Some(&graph));
        assert_eq!(p.files, ["src/lib.rs", "CHANGELOG.md"]);
        assert_eq!(p.symbols, ["src/lib.rs::run", "src/lib.rs::other"]);
        assert_eq!(
            p.groups.len(),
            2,
            "run and other do not reference each other"
        );
        assert_eq!(p.mode, ExecutionMode::Single);
        assert!(p.reason.contains("독립 덩어리 2개"));
    }

    #[test]
    fn turn_becomes_a_task_with_child_session_partition_and_cost() {
        let (_dir, graph) = repo();
        let mut tasks = TurnTasks::default();
        tasks.on_command(&AgentCommand::Submit {
            text: "로그인 오류 고쳐줘\n자세히는...".into(),
            model: "m".into(),
            effort: None,
            images: vec![],
            skills: vec![],
        });
        let ev = |u| AgentEvent::scoped("main", u);
        let none = || -> anyhow::Result<Arc<CodeGraph>> { anyhow::bail!("unused") };
        let started = tasks.on_event(
            &ev(AgentUpdate::TurnStarted {
                id: Some("t1".into()),
            }),
            None,
            none,
        );
        let AgentUpdate::TaskUpdated(task) = &started[1].update else {
            panic!()
        };
        assert_eq!(task.title, "로그인 오류 고쳐줘");
        assert_eq!(task.session.as_deref(), Some("main"));
        let child = tasks.on_event(
            &ev(AgentUpdate::ActivityUpdated(Agent {
                id: "thread-sub".into(),
                name: "explorer".into(),
                status: "running".into(),
                detail: String::new(),
            })),
            None,
            none,
        );
        let AgentUpdate::TaskUpdated(child) = &child[0].update else {
            panic!()
        };
        assert_eq!(child.parent.as_deref(), Some("turn-t1"));
        assert_eq!(child.session.as_deref(), Some("thread-sub"));
        tasks.on_event(&ev(AgentUpdate::DiffUpdated(DIFF.into())), None, none);
        let mut cost = crate::core::telemetry::PhaseLedger::default();
        cost.observe(Some("main"), &AgentUpdate::TurnStarted { id: None });
        let cost = cost
            .observe(
                Some("main"),
                &AgentUpdate::TurnCompleted {
                    id: None,
                    status: "completed".into(),
                    error: None,
                },
            )
            .unwrap();
        let done = tasks.on_event(
            &ev(AgentUpdate::TurnCompleted {
                id: Some("t1".into()),
                status: "completed".into(),
                error: None,
            }),
            Some(&cost),
            || Ok(Arc::clone(&graph)),
        );
        let AgentUpdate::TaskUpdated(task) = &done[0].update else {
            panic!()
        };
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.partition.as_ref().unwrap().symbols.len(), 2);
        assert!(task.cost.is_some());
        let replay = tasks.on_event(
            &AgentEvent::global(AgentUpdate::SessionLoaded {
                snapshot: SessionSnapshot {
                    id: "main".into(),
                    title: None,
                    status: None,
                    turns: None,
                },
                switching: true,
                model: None,
                permissions: Permissions::default(),
            }),
            None,
            none,
        );
        assert_eq!(
            replay.len(),
            2,
            "both tasks come back when the session is re-entered"
        );
    }
}
