//! Optional orchestration inspector, using only Core-owned task projections.
use super::*;
use crate::agent::{ActivityTask, TaskStatus};

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
impl App {
    pub(super) fn update_task(&mut self, task: ActivityTask) {
        if task.status == TaskStatus::Running {
            self.active_tasks.insert(task.id.clone());
        } else {
            self.active_tasks.remove(&task.id);
        }
        if let Some(old) = self.tasks.iter_mut().find(|old| old.id == task.id) {
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
        if self.tasks.is_empty() {
            return rows;
        }
        rows.push(("작업 그래프".into(), Tone::Accent, None));
        for (index, task) in self.tasks.iter().enumerate() {
            let (icon, _, tone) = status(task.status);
            let indent = format!(
                "{}{}",
                "  ".repeat(self.task_depth(task)),
                if task.parent.is_some() { "└ " } else { "" }
            );
            let worker = task.worker.map(|w| w.label()).unwrap_or("CORE");
            let label = format!(
                "{indent}{icon} {} {} [{worker}]{}",
                index + 1,
                task.title,
                if task.critical { " · critical" } else { "" }
            );
            for line in wrap(&label, width) {
                rows.push((line, tone, Some(Action::Task(task.id.clone()))));
            }
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
        if details
            && let Some(task) = self
                .selected_task
                .as_ref()
                .and_then(|id| self.tasks.iter().find(|task| &task.id == id))
        {
            let summary = std::mem::take(&mut rows);
            rows.push(("← 작업 목록".into(), Tone::Muted, Some(Action::Agents)));
            rows.push((format!("작업 상세 · {}", task.title), Tone::Accent, None));
            for value in [
                format!("ID: {}", task.id),
                format!(
                    "Worker: {}",
                    task.worker.map(|w| w.label()).unwrap_or("CORE")
                ),
                format!("상태: {}", status(task.status).1),
                format!(
                    "Elapsed: {}",
                    task.elapsed_ms
                        .map(|ms| format!("{:.2}s", ms as f64 / 1000.0))
                        .unwrap_or("미계측".into())
                ),
            ] {
                for line in wrap(&value, width) {
                    rows.push((line, Tone::Normal, None));
                }
            }
            if task.critical {
                rows.push(("Critical path · Core에서 지정".into(), Tone::Warning, None));
            }
            if let Some(reason) = &task.reason {
                for line in wrap(&format!("이유: {reason}"), width) {
                    rows.push((line, Tone::Warning, None));
                }
            }
            for (heading, values) in [
                (
                    "Depends on",
                    task.dependencies
                        .iter()
                        .map(|id| format!("{id} · {}", self.task_label(id)))
                        .collect::<Vec<_>>(),
                ),
                ("Inputs", task.inputs.clone()),
                ("Outputs", task.outputs.clone()),
            ] {
                rows.push((heading.into(), Tone::Accent, None));
                if values.is_empty() {
                    rows.push(("—".into(), Tone::Muted, None));
                }
                for value in values {
                    for line in wrap(&value, width) {
                        rows.push((line, Tone::Normal, None));
                    }
                }
            }
            rows.push(("Cost/cache: 미계측".into(), Tone::Muted, None));
            rows.push((String::new(), Tone::Normal, None));
            rows.extend(summary);
        }
        rows
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
        }
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
        assert!(rows.iter().any(|r| r.0.contains("미계측")));
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
}
