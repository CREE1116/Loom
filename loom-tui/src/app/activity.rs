//! Optional orchestration inspector, using only Core-owned task projections.
use super::*;
use crate::agent::{ActivityTask, ExecutionMode, TaskCost, TaskPartition, TaskStatus};

type Row = (String, Tone, Option<Action>);
fn status(status: TaskStatus) -> (&'static str, &'static str, Tone) {
    match status {
        TaskStatus::Running => ("●", "실행 중", Tone::Accent),
        TaskStatus::Completed => ("✓", "완료", Tone::Success),
        TaskStatus::Failed => ("!", "실패", Tone::Danger),
        TaskStatus::Cancelled => ("×", "취소", Tone::Muted),
        TaskStatus::Blocked => ("◌", "dependency 대기", Tone::Warning),
        TaskStatus::Pending => ("◌", "대기", Tone::Muted),
        TaskStatus::Ready => ("◌", "실행 준비", Tone::Normal),
        TaskStatus::NeedsReview => ("!", "검토 필요", Tone::Warning),
    }
}
const LIST_LIMIT: usize = 8;

fn worker_label(task: &ActivityTask) -> &str {
    if let Some(model) = task
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        return model;
    }
    match (task.worker, &task.session) {
        (Some(worker), _) => worker.label(),
        (None, Some(_)) => "세션",
        (None, None) => "코어",
    }
}

fn tokens(value: Option<u64>) -> String {
    match value {
        None => "미보고".into(),
        Some(n) if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        Some(n) if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        Some(n) => n.to_string(),
    }
}

/// `탐색 87% · 검증 9% · 편집 4%`; None when no context was measured.
fn phase_shares(cost: &TaskCost) -> Option<String> {
    let total = cost.orient_bytes + cost.validate_bytes + cost.edit_bytes + cost.message_bytes;
    (total > 0).then(|| {
        let pct = |n: u64| n * 100 / total;
        format!(
            "탐색 {}% · 검증 {}% · 편집 {}% · 응답 {}%",
            pct(cost.orient_bytes),
            pct(cost.validate_bytes),
            pct(cost.edit_bytes),
            pct(cost.message_bytes)
        )
    })
}

fn push_list(rows: &mut Vec<Row>, values: &[String], width: usize) {
    for value in values.iter().take(LIST_LIMIT) {
        for line in wrap(&format!("· {value}"), width) {
            rows.push((line, Tone::Normal, None));
        }
    }
    if values.len() > LIST_LIMIT {
        rows.push((
            format!("  외 {}개", values.len() - LIST_LIMIT),
            Tone::Muted,
            None,
        ));
    }
}

/// Last path segment pair of a symbol id: `src/walk.rs::Walk::next` → `Walk::next`.
fn short_symbol(id: &str) -> String {
    let parts: Vec<&str> = id.split("::").skip(1).collect();
    parts[parts.len().saturating_sub(2)..].join("::")
}

impl App {
    fn partition_rows(&self, rows: &mut Vec<Row>, task: &ActivityTask, width: usize) {
        panel_section(rows, "작업 구분", width);
        let Some(TaskPartition {
            mode,
            reason,
            files,
            symbols,
            groups,
        }) = task.partition.as_deref()
        else {
            let note = if task.status == TaskStatus::Running {
                "실행 중 · 수정 범위는 작업이 끝나면 확정됩니다"
            } else {
                "기록된 수정 범위 없음"
            };
            rows.push((note.into(), Tone::Muted, None));
            return;
        };
        let mode = match mode {
            ExecutionMode::Single => "단일 실행",
            ExecutionMode::Split => "분할 실행",
        };
        rows.push((mode.into(), Tone::Accent, None));
        for line in wrap(reason, width) {
            rows.push((line, Tone::Muted, None));
        }
        if !files.is_empty() {
            rows.push((format!("수정 파일 {}", files.len()), Tone::Normal, None));
            push_list(rows, files, width);
        }
        if !symbols.is_empty() {
            rows.push((format!("수정 symbol {}", symbols.len()), Tone::Normal, None));
            push_list(
                rows,
                &symbols.iter().map(|s| short_symbol(s)).collect::<Vec<_>>(),
                width,
            );
        }
        if groups.len() >= 2 {
            rows.push((format!("독립 덩어리 {}", groups.len()), Tone::Normal, None));
            let lines: Vec<String> = groups
                .iter()
                .enumerate()
                .map(|(i, group)| {
                    format!(
                        "{}: {}",
                        i + 1,
                        group
                            .iter()
                            .map(|s| short_symbol(s))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .collect();
            push_list(rows, &lines, width);
        }
    }

    fn cost_rows(&self, rows: &mut Vec<Row>, task: &ActivityTask, width: usize) {
        let Some(cost) = &task.cost else { return };
        panel_section(rows, "작업 비용", width);
        if let Some(shares) = phase_shares(cost) {
            for line in wrap(&shares, width) {
                rows.push((line, Tone::Normal, None));
            }
        }
        rows.push((
            format!(
                "input {} · cached {} · output {}",
                tokens(cost.input_tokens),
                tokens(cost.cached_input_tokens),
                tokens(cost.output_tokens)
            ),
            Tone::Muted,
            None,
        ));
        rows.push((
            format!("모델 호출 {}회", cost.model_calls),
            Tone::Muted,
            None,
        ));
    }

    fn session_rows(&self, rows: &mut Vec<Row>, task: &ActivityTask) {
        let Some(session) = &task.session else { return };
        if self.thread.as_deref() == Some(session.as_str()) {
            rows.push(("이 세션에서 실행된 작업".into(), Tone::Muted, None));
            return;
        }
        rows.push((
            "세션으로 들어가기 ▸".into(),
            Tone::Accent,
            Some(Action::EnterSession(session.clone())),
        ));
        rows.push((
            "새 창에서 열기 ↗".into(),
            Tone::Muted,
            Some(Action::OpenThread(session.clone())),
        ));
    }

    pub(super) fn update_task(&mut self, mut task: ActivityTask) {
        if task.status == TaskStatus::Running {
            self.active_tasks.insert(task.id.clone());
        } else {
            self.active_tasks.remove(&task.id);
        }
        if let Some(old) = self.tasks.iter_mut().find(|old| old.id == task.id) {
            // Progress-only updates omit identity; retain the last observed model.
            if task.model.as_deref().is_none_or(|model| model.trim().is_empty()) {
                task.model = old.model.clone();
            }
            *old = task;
        } else {
            self.tasks.push(task);
        }
    }
    fn task_label(&self, id: &str) -> String {
        self.tasks
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone())
            .unwrap_or_else(|| id.into())
    }
    fn task_depth(&self, task: &ActivityTask) -> usize {
        let mut parent = task.parent.as_deref();
        let mut seen = HashSet::from([task.id.as_str()]);
        let mut depth = 0;
        while let Some(id) = parent {
            if !seen.insert(id) || depth >= 4 {
                break;
            }
            depth += 1;
            parent = self
                .tasks
                .iter()
                .find(|t| t.id == id)
                .and_then(|t| t.parent.as_deref());
        }
        depth
    }
    pub(super) fn task_rows(&self, width: usize, details: bool) -> Vec<Row> {
        let mut rows = Vec::new();
        if details
            && let Some(task) = self
                .selected_task
                .as_ref()
                .and_then(|id| self.tasks.iter().find(|t| &t.id == id))
        {
            rows.push(("← 작업 목록".into(), Tone::Accent, Some(Action::Agents)));
            panel_section(&mut rows, "작업 상세", width);
            for line in wrap(&task.title, width) {
                rows.push((line, Tone::Accent, None));
            }
            let (_, state, tone) = status(task.status);
            for line in wrap(&format!("{} · {}", state, worker_label(task)), width) {
                rows.push((line, tone, None));
            }
            self.session_rows(&mut rows, task);
            if let Some(ms) = task.elapsed_ms {
                rows.push((
                    format!("소요 {:.2}초", ms as f64 / 1000.0),
                    Tone::Muted,
                    None,
                ));
            }
            if task.critical {
                rows.push(("핵심 경로 · Core 지정".into(), Tone::Warning, None));
            }
            self.partition_rows(&mut rows, task, width);
            self.cost_rows(&mut rows, task, width);
            if let Some(reason) = &task.reason {
                panel_section(&mut rows, "상태 설명", width);
                for line in wrap(reason, width) {
                    rows.push((line, tone, None));
                }
            }
            for (heading, values) in [
                (
                    "선행 작업",
                    task.dependencies
                        .iter()
                        .map(|id| format!("{id} · {}", self.task_label(id)))
                        .collect::<Vec<_>>(),
                ),
                ("입력", task.inputs.clone()),
                ("결과", task.outputs.clone()),
            ] {
                if values.is_empty() {
                    continue;
                }
                panel_section(&mut rows, heading, width);
                for value in values {
                    for line in wrap(&value, width) {
                        rows.push((line, Tone::Normal, None));
                    }
                }
            }
            return rows;
        }
        if self.tasks.is_empty() {
            if details {
                panel_section(&mut rows, "작업", width);
                rows.push(("아직 등록된 작업이 없습니다".into(), Tone::Muted, None));
            }
            return rows;
        }
        panel_section(&mut rows, "작업", width);
        let active = self
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Running)
            .count();
        let done = self
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .count();
        rows.push((
            format!(
                "전체 {} · 진행 {} · 완료 {}",
                self.tasks.len(),
                active,
                done
            ),
            Tone::Muted,
            None,
        ));
        for (index, task) in self.tasks.iter().enumerate() {
            let (icon, _, tone) = status(task.status);
            let indent = format!(
                "{}{}",
                "  ".repeat(self.task_depth(task)),
                if task.parent.is_some() { "└ " } else { "" }
            );
            let mut worker = worker_label(task).to_owned();
            if let Some(p) = &task.partition
                && !p.files.is_empty()
            {
                worker.push_str(&format!(" · 파일 {}", p.files.len()));
            }
            if let Some(shares) = task.cost.as_ref().and_then(phase_shares) {
                worker.push_str(&format!(" · {}", shares.split(" · ").next().unwrap_or("")));
            }
            let label = format!("{indent}{icon} {}. {} ▸", index + 1, task.title);
            for line in wrap(&label, width) {
                rows.push((line, tone, Some(Action::Task(task.id.clone()))));
            }
            rows.push((
                format!(
                    "{indent}  {worker}{}",
                    if task.critical {
                        " · 핵심 경로"
                    } else {
                        ""
                    }
                ),
                Tone::Muted,
                Some(Action::Task(task.id.clone())),
            ));
            if task.status == TaskStatus::Blocked {
                let waiting = task.reason.clone().unwrap_or_else(|| {
                    format!(
                        "{} 대기",
                        task.dependencies
                            .iter()
                            .map(|id| self.task_label(id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                });
                for line in wrap(&format!("{indent}  └ {waiting}"), width) {
                    rows.push((line, Tone::Muted, Some(Action::Task(task.id.clone()))));
                }
            } else if matches!(
                task.status,
                TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::NeedsReview
            ) && let Some(reason) = &task.reason
            {
                for line in wrap(&format!("{indent}  └ {reason}"), width) {
                    rows.push((line, tone, Some(Action::Task(task.id.clone()))));
                }
            }
        }
        rows
    }

    pub(crate) fn activity_model_retention_selfcheck(report: &mut crate::selfcheck::Report) {
        use crate::selfcheck::{Check, Dump};
        const SCENARIO: &str = "activity_model_retention";
        const MODEL: &str = "ollama/SparkLLM/Spark-X2.5-1.7B:latest";
        for width in [40, 80, 120] {
            let mut app = App::new(false);
            app.update_task(ActivityTask {
                id: "observed-model".into(),
                title: "모델 작업".into(),
                status: TaskStatus::Running,
                model: Some(MODEL.into()),
                ..Default::default()
            });
            app.update_task(ActivityTask {
                id: "observed-model".into(),
                title: "모델 작업".into(),
                status: TaskStatus::Completed,
                ..Default::default()
            });
            app.view = View::Agents;
            app.selected_task = Some("observed-model".into());
            let canvas = app.render(width, 24);
            let visible: String = (0..24).map(|y| canvas.plain_line(y).trim().to_owned()).collect();
            report.checks.push(Check {
                scenario: SCENARIO,
                name: format!("{width} 작업 완료 후 실제 엔진/모델 전체 표시"),
                pass: visible.contains(MODEL)
                    && worker_label(&app.tasks[0]) == MODEL
                    && app.active_tasks.is_empty(),
                detail: "모델 없는 상태 업데이트 뒤에도 마지막 관측 모델을 유지하며 좁은 화면에 줄바꿈합니다.".into(),
            });
            report.dumps.push(Dump {
                scenario: SCENARIO,
                label: format!("실제 작업 모델 {width}x24"),
                width,
                height: 24,
                rows: (0..24).map(|y| canvas.plain_line(y)).collect(),
            });
        }
    }

    pub(crate) fn activity_label_selfcheck(report: &mut crate::selfcheck::Report) {
        use crate::agent::WorkerKind;
        use crate::selfcheck::{Check, Dump};
        const SCENARIO: &str = "activity_worker_identity";
        for (worker, session, model, label) in [
            (None, None, None, "코어"),
            (None, Some("ollama-session"), None, "세션"),
            (
                Some(WorkerKind::Local),
                Some("ollama-session"),
                None,
                "LOCAL",
            ),
            (None, Some("ollama-session"), Some("qwen3:8b"), "qwen3:8b"),
        ] {
            let mut app = App::new(false);
            app.model = "unrelated-current-model".into();
            app.update_task(ActivityTask {
                id: "honest-task".into(),
                title: "연결된 작업".into(),
                worker,
                session: session.map(str::to_owned),
                model: model.map(str::to_owned),
                status: TaskStatus::Completed,
                ..ActivityTask::default()
            });
            for width in [40, 80, 140] {
                let list = app.task_rows(usize::from(width), false);
                app.selected_task = Some("honest-task".into());
                let details = app.task_rows(usize::from(width), true);
                app.selected_task = None;
                report.checks.push(Check {
                    scenario: SCENARIO,
                    name: format!("{width} 작업 식별 {label}"),
                    pass: list.iter().any(|r| r.0.trim() == label)
                        && details.iter().any(|r| r.0 == format!("완료 · {label}"))
                        && list.iter().chain(&details).all(|r| {
                            !r.0.contains("CODEX") && !r.0.contains("unrelated-current-model")
                        }),
                    detail: "작업의 실제 모델, 보고된 worker, 중립 라벨 순으로 표시합니다.".into(),
                });
                app.view = View::Agents;
                let canvas = app.render(width, 24);
                report.dumps.push(Dump {
                    scenario: SCENARIO,
                    label: format!("작업 식별 {label} {width}x24"),
                    width,
                    height: 24,
                    rows: (0..24).map(|y| canvas.plain_line(y)).collect(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentEvent, AgentUpdate, WorkerKind};
    fn task(id: &str, state: TaskStatus) -> ActivityTask {
        ActivityTask {
            id: id.into(),
            parent: None,
            title: format!("작업 {id}"),
            worker: Some(WorkerKind::Local),
            status: state,
            dependencies: vec![],
            reason: None,
            critical: false,
            elapsed_ms: None,
            inputs: vec![],
            outputs: vec![],
            ..Default::default()
        }
    }
    #[test]
    fn activity_labels_selfcheck_covers_reported_identity_and_neutral_fallbacks() {
        let mut report = crate::selfcheck::Report::default();
        App::activity_label_selfcheck(&mut report);
        println!("{}", report.text(true));
        assert_eq!(report.failures(), 0);
        assert_eq!(report.checks.len(), 12);
    }
    #[test]
    fn missing_worker_uses_neutral_session_or_core_label() {
        let mut task = task("native", TaskStatus::Running);
        task.worker = None;
        assert_eq!(worker_label(&task), "코어");
        task.session = Some("ollama-session".into());
        assert_eq!(worker_label(&task), "세션");
        task.worker = Some(WorkerKind::Local);
        assert_eq!(worker_label(&task), "LOCAL");
    }
    #[test]
    fn served_model_identifies_the_task_instead_of_the_current_session_setting() {
        let mut app = App::new(false);
        app.model = "unrelated-current-model".into();
        let mut task = task("native", TaskStatus::Completed);
        task.worker = None;
        task.session = Some("ollama-session".into());
        task.model = Some("qwen3:8b".into());
        assert_eq!(worker_label(&task), "qwen3:8b");
        app.update_task(task);
        let rows = app.task_rows(60, false);
        assert!(rows.iter().any(|r| r.0.contains("qwen3:8b")));
        assert!(
            rows.iter()
                .all(|r| !r.0.contains("CODEX") && !r.0.contains("unrelated-current-model"))
        );
        app.selected_task = Some("native".into());
        assert!(
            app.task_rows(60, true)
                .iter()
                .any(|r| r.0.contains("완료 · qwen3:8b"))
        );
        app.tasks[0].model = Some("  ".into());
        assert_eq!(worker_label(&app.tasks[0]), "세션");
    }
    #[test]
    fn model_retention_selfcheck_renders_the_full_observed_identity() {
        let mut report = crate::selfcheck::Report::default();
        App::activity_model_retention_selfcheck(&mut report);
        println!("{}", report.text(true));
        assert_eq!(report.failures(), 0);
        assert_eq!(report.checks.len(), 3);
    }
    #[test]
    fn task_status_updates_keep_the_last_reported_model() {
        let mut app = App::new(false);
        let mut started = task("native", TaskStatus::Running);
        started.model = Some("ollama/qwen3:8b".into());
        app.update_task(started);
        for omitted in [None, Some("  ".into())] {
            let mut finished = task("native", TaskStatus::Completed);
            finished.model = omitted;
            app.update_task(finished);
            assert_eq!(worker_label(&app.tasks[0]), "ollama/qwen3:8b");
            assert!(!app.active_tasks.contains("native"));
        }
        let mut rerouted = task("native", TaskStatus::Running);
        rerouted.model = Some("ollama/qwen3:14b".into());
        app.update_task(rerouted);
        assert_eq!(worker_label(&app.tasks[0]), "ollama/qwen3:14b");
    }
    #[test]
    fn long_reported_model_is_readable_in_narrow_task_details() {
        let mut app = App::new(false);
        let mut observed = task("native", TaskStatus::Completed);
        let model = "ollama/SparkLLM/Spark-X2.5-1.7B:latest";
        observed.model = Some(model.into());
        app.update_task(observed);
        app.selected_task = Some("native".into());
        let rows = app.task_rows(28, true);
        assert!(rows.iter().all(|row| unicode_width::UnicodeWidthStr::width(row.0.as_str()) <= 28));
        assert!(rows.iter().map(|row| row.0.as_str()).collect::<String>().contains(model));
    }
    #[test]
    fn a_session_id_does_not_claim_a_codex_worker() {
        let mut app = App::new(false);
        let mut unknown = task("unknown", TaskStatus::Running);
        unknown.worker = None;
        unknown.session = Some("main".into());
        app.tasks.push(unknown);
        app.selected_task = Some("unknown".into());
        let rows = app.task_rows(80, true);
        assert!(
            !rows.iter().any(|row| row.0.contains("CODEX")),
            "session ownership cannot identify the engine: {rows:?}"
        );
    }
    #[test]
    fn activity_projects_waiting_reasons_and_authoritative_updates_without_losing_draft() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.editor.insert("계속 쓸 초안");
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::TaskUpdated(task("analysis", TaskStatus::Running)),
        ));
        let mut blocked = task("patch", TaskStatus::Blocked);
        blocked.dependencies.push("analysis".into());
        blocked.reason = Some("원인 분석이 수정 제약을 결정함".into());
        blocked.critical = true;
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::TaskUpdated(blocked),
        ));
        app.activate(Action::Task("patch".into()));
        let rows = app.task_rows(50, true);
        assert!(rows.iter().any(|r| r.0.contains("원인 분석")));
        assert!(
            rows.iter()
                .any(|r| r.0.contains("analysis · 작업 analysis"))
        );
        assert!(rows.iter().any(|r| r.0.contains("작업 상세")));
        assert!(app.active_tasks.contains("analysis"));
        app.apply_event(AgentEvent::scoped(
            "other",
            AgentUpdate::TaskUpdated(task("analysis", TaskStatus::Failed)),
        ));
        assert!(app.active_tasks.contains("analysis"));
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::TaskUpdated(task("analysis", TaskStatus::Completed)),
        ));
        assert!(!app.active_tasks.contains("analysis"));
        for (w, h) in [(40, 12), (80, 24), (140, 40)] {
            app.render(w, h);
        }
        assert_eq!(app.editor.text, "계속 쓸 초안");
    }
    #[test]
    fn cyclic_parent_projection_is_bounded_and_failure_reason_is_visible() {
        let mut app = App::new(false);
        let mut a = task("a", TaskStatus::Failed);
        a.parent = Some("b".into());
        a.reason = Some("테스트 실패".into());
        let mut b = task("b", TaskStatus::Cancelled);
        b.parent = Some("a".into());
        app.update_task(a);
        app.update_task(b);
        assert!(
            app.task_rows(32, false)
                .iter()
                .any(|r| r.0.contains("테스트 실패"))
        );
    }
    #[test]
    fn confirmed_task_shows_boundary_cost_and_enters_and_returns_from_its_session() {
        use crate::agent::{Permissions, SessionSnapshot, TaskCost, TaskPartition};
        let loaded = |id: &str, title: &str| {
            AgentEvent::global(AgentUpdate::SessionLoaded {
                snapshot: SessionSnapshot {
                    id: id.into(),
                    title: Some(title.into()),
                    status: Some("idle".into()),
                    turns: Some(vec![]),
                },
                switching: true,
                model: None,
                permissions: Permissions::default(),
            })
        };
        let mut app = App::new(false);
        app.apply_event(loaded("main", "메인 작업"));
        let mut turn = task("turn-1", TaskStatus::Completed);
        turn.worker = None;
        turn.session = Some("main".into());
        turn.partition = Some(Box::new(TaskPartition {
            reason: "graph상 독립 덩어리 2개".into(),
            files: vec!["src/walk.rs".into()],
            symbols: vec![
                "src/walk.rs::Walk::next".into(),
                "src/walk.rs::helper".into(),
            ],
            groups: vec![
                vec!["src/walk.rs::Walk::next".into()],
                vec!["src/walk.rs::helper".into()],
            ],
            ..TaskPartition::default()
        }));
        turn.cost = Some(TaskCost {
            orient_bytes: 87,
            validate_bytes: 9,
            edit_bytes: 4,
            input_tokens: Some(3_300_000),
            cached_input_tokens: Some(0),
            model_calls: 44,
            ..TaskCost::default()
        });
        let mut child = task("agent-sub", TaskStatus::Running);
        child.worker = None;
        child.parent = Some("turn-1".into());
        child.session = Some("sub".into());
        app.apply_event(AgentEvent::scoped("main", AgentUpdate::TaskUpdated(turn)));
        app.apply_event(AgentEvent::scoped("main", AgentUpdate::TaskUpdated(child)));

        let list = app.task_rows(60, true);
        assert!(
            list.iter()
                .any(|r| r.0.contains("세션 · 파일 1 · 탐색 87%"))
        );
        app.activate(Action::Task("turn-1".into()));
        let detail: Vec<String> = app.task_rows(60, true).into_iter().map(|r| r.0).collect();
        for expected in [
            "작업 구분",
            "단일 실행",
            "수정 symbol 2",
            "Walk::next",
            "독립 덩어리 2",
            "작업 비용",
            "input 3.3M · cached 0",
            "모델 호출 44회",
            "이 세션에서 실행된 작업",
        ] {
            assert!(
                detail.iter().any(|l| l.contains(expected)),
                "missing {expected}: {detail:?}"
            );
        }

        app.activate(Action::Task("agent-sub".into()));
        let rows = app.task_rows(60, true);
        assert!(
            rows.iter()
                .any(|r| r.2 == Some(Action::OpenThread("sub".into())))
        );
        assert_eq!(
            app.activate(Action::EnterSession("sub".into())),
            Some(Intent::Session(Some("sub".into())))
        );
        app.apply_event(loaded("sub", "하위 세션"));
        assert_eq!(app.thread.as_deref(), Some("sub"));
        assert_eq!(
            app.session_trail,
            vec![("main".to_owned(), "메인 작업".to_owned())]
        );
        let screen = app.render(100, 30);
        assert!(
            screen
                .hits
                .iter()
                .any(|h| h.action == Action::ReturnSession),
            "return button in the header"
        );

        assert_eq!(
            app.activate(Action::ReturnSession),
            Some(Intent::Session(Some("main".into())))
        );
        app.apply_event(loaded("main", "메인 작업"));
        assert!(app.session_trail.is_empty());
        assert!(
            !app.render(100, 30)
                .hits
                .iter()
                .any(|h| h.action == Action::ReturnSession)
        );
    }

    #[test]
    fn entering_a_session_is_refused_while_the_turn_runs_and_points_to_a_new_window() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.busy = true;
        assert_eq!(app.activate(Action::EnterSession("sub".into())), None);
        assert!(app.pending_enter.is_none());
        assert_eq!(
            app.activate(Action::OpenThread("sub".into())),
            Some(Intent::OpenAgent("sub".into()))
        );
    }
}
