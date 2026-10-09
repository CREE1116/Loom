use super::*;
use crate::agent::{AgentEvent, AgentUpdate, Permissions, SessionSnapshot};

impl App {
    pub fn agent_command(&self, intent: &Intent) -> Option<AgentCommand> {
        Some(match intent {
            Intent::Goal(action) => AgentCommand::Goal(action.clone()),
            Intent::Compact => AgentCommand::Compact,
            Intent::Inquiry(question) => AgentCommand::Inquiry(question.clone()),
            Intent::DirectAgent(id, text) => AgentCommand::DirectAgent {
                id: id.clone(),
                text: text.clone(),
            },
            Intent::Answer(id, answers) => AgentCommand::Answer {
                request_id: id.clone(),
                answers: answers.clone(),
            },
            Intent::CancelQuestions(id) => AgentCommand::CancelInput { request_id: id.clone() },
            Intent::RefreshUsage => AgentCommand::RefreshUsage,
            Intent::Explore(query) => AgentCommand::Explore(query.clone()),
            Intent::Submit(text) => AgentCommand::Submit {
                text: text.clone(),
                model: self
                    .submitted_model
                    .as_deref()
                    .unwrap_or(&self.model)
                    .into(),
                skills: self.submitted_skills.clone(),
                images: self.submitted_images.clone(),
                effort: self.submitted_effort.clone(),
            },
            Intent::Interrupt => AgentCommand::Interrupt,
            Intent::Reply(id, choice) => AgentCommand::Reply {
                request_id: id.clone(),
                choice: choice.clone(),
            },
            Intent::Session(id) => AgentCommand::OpenSession {
                id: id.clone(),
                model: self.model.clone(),
            },
            Intent::Rewind(turn) => AgentCommand::Rewind {
                turn_id: turn.clone(),
                model: self.model.clone(),
            },
            Intent::PreviewRewind(turn) => AgentCommand::PreviewRewind {
                turn_id: turn.clone(),
            },
            Intent::Fork(turn) => AgentCommand::Fork {
                turn_id: turn.clone(),
                model: self.model.clone(),
            },
            Intent::RefreshSessions => AgentCommand::RefreshSessions,
            Intent::LoadMoreSessions(cursor) => AgentCommand::MoreSessions(cursor.clone()),
            Intent::DeleteSession(id) => AgentCommand::DeleteSession(id.clone()),
            Intent::RefreshSkills => AgentCommand::RefreshSkills,
            Intent::RefreshModels => AgentCommand::RefreshModels,
            Intent::ReadAgent(id) => AgentCommand::ReadActivity(id.clone()),
            Intent::UpdatePermission(kind, value) => AgentCommand::UpdatePermission {
                kind: kind.clone(),
                value: value.clone(),
            },
            Intent::TrustWorkspace(trusted) => AgentCommand::TrustWorkspace(*trusted),
            _ => return None,
        })
    }

    pub fn apply_event(&mut self, event: AgentEvent) {
        if event
            .session_id
            .as_deref()
            .is_some_and(|id| self.thread.as_deref() != Some(id))
        {
            return;
        }
        match event.update {
            AgentUpdate::ModelResolved {
                requested,
                provider,
                model,
            } => {
                self.resolved_model = Some((requested, provider, model));
            }
            AgentUpdate::RewindPreview {
                turn_id,
                files,
                added,
                removed,
                blocked,
            } => {
                if self.rewind_confirm.as_deref() == Some(turn_id.as_str()) {
                    self.rewind_preview = Some((files, added, removed, blocked));
                }
            }
            AgentUpdate::GoalUpdated(goal) => {
                // Backends report the goal after every turn; only a change is news.
                let changed = format!("{:?}", self.goal) != format!("{goal:?}");
                self.goal = goal;
                if changed {
                    self.notify(if self.goal.is_some() {
                        "목표 상태를 갱신했습니다."
                    } else {
                        "목표를 해제했습니다."
                    });
                }
            }
            AgentUpdate::CompressionState { running, error } => {
                self.compacting = running;
                if let Some(error) = error {
                    self.queue_paused = true;
                    self.error(format!(
                        "컨텍스트 압축 실패: {error} · 대기열은 /queue resume으로 재개하세요."
                    ));
                } else {
                    self.notify(if running {
                        "컨텍스트 압축 중…"
                    } else {
                        "컨텍스트 압축 완료"
                    });
                }
            }
            AgentUpdate::ModelProfilesLoaded(profiles) => {
                self.model_profiles = profiles;
                self.reconcile_effort();
                self.apply_initial_model_default();
            }
            AgentUpdate::EffortObserved(effort) => self.runtime_effort = effort,
            AgentUpdate::WorkPhaseUpdated(phase) => self.work_phase = phase,
            AgentUpdate::ModelSpeed { tokens_per_second } => {
                self.model_speed = Some((self.model.clone(), tokens_per_second));
            }
            AgentUpdate::TokenUsageUpdated(usage) => self.set_tokens(usage),
            AgentUpdate::QuotaUpdated(quota) => self.update_quota(quota),
            AgentUpdate::ExecutionLimited { kind, reason } => self.execution_limited(kind, reason),
            AgentUpdate::ExecutionLimitCleared => self.clear_execution_limit(),
            AgentUpdate::TaskUpdated(task) => self.update_task(task),
            AgentUpdate::InputRequested(request) => self.input_requested(request),
            AgentUpdate::InputResolved(id) => self.input_resolved(&id),
            AgentUpdate::InputReplyFailed { id, reason } => self.input_reply_failed(&id, &reason),
            AgentUpdate::TaskStarted { id } => {
                self.welcome_dismissed = true;
                self.active_tasks.insert(id);
            }
            AgentUpdate::TaskCompleted { id } => {
                if self.active_tasks.remove(&id) && self.active_tasks.is_empty() && !self.busy {
                    self.event_notice(self.preferences.notify_completed, "작업이 완료되었습니다.");
                }
            }
            AgentUpdate::SessionLoaded {
                snapshot,
                switching,
                model,
                permissions,
            } => {
                let starting = self.thread.is_none();
                let branch_origin = self.branch_pending.take();
                let source_title = self.session_title.clone();
                let previous_sessions = if branch_origin.is_some() {
                    self.sessions.clone()
                } else {
                    Vec::new()
                };
                if let Some(origin) = &branch_origin {
                    if snapshot.id == *origin {
                        self.error("분기 생성 실패: 원본 세션과 같은 ID가 반환되었습니다.");
                        return;
                    }
                }
                if switching {
                    // Recovery from a failed initial connection is still the same
                    // unsent conversation. Normal session switches stay isolated.
                    let unsent = if self.thread.is_none() {
                        Some((std::mem::take(&mut self.queued), self.queue_paused))
                    } else {
                        None
                    };
                    let local_history = if self.thread.is_none() {
                        Some(std::mem::take(&mut self.input_history))
                    } else {
                        None
                    };
                    self.reset_session_view();
                    if let Some((unsent, paused)) = unsent {
                        self.queued = unsent;
                        self.queue_paused = paused || self.execution_limit.is_some();
                    }
                    self.set_session(snapshot);
                    if let Some(local_history) = local_history {
                        for text in local_history {
                            if self.input_history.last() != Some(&text) {
                                self.input_history.push(text);
                            }
                        }
                    }
                } else {
                    self.set_session(snapshot);
                }
                let loaded = self.thread.clone().unwrap_or_default();
                if starting {
                    self.startup_view();
                }
                match self.pending_enter.take() {
                    Some((target, origin, title)) if target == loaded => {
                        self.session_trail.push((origin, title));
                    }
                    _ => {
                        if self
                            .session_trail
                            .last()
                            .is_some_and(|(id, _)| *id == loaded)
                        {
                            self.session_trail.pop();
                        }
                    }
                }
                if let Some(origin) = branch_origin {
                    self.session_title = format!(
                        "{} · {source_title}",
                        if self.preferences.language == Language::English {
                            "Branch"
                        } else {
                            "분기"
                        }
                    );
                    self.sessions = previous_sessions;
                    for (id, title) in [
                        (origin, source_title),
                        (loaded.clone(), self.session_title.clone()),
                    ] {
                        if !self.sessions.iter().any(|session| session.id == id) {
                            self.sessions.insert(
                                0,
                                SessionSummary {
                                    id,
                                    title,
                                    status: "idle".into(),
                                    updated_at: None,
                                },
                            );
                        }
                    }
                    self.notify(format!(
                        "{} · {}",
                        if self.preferences.language == Language::English {
                            "Branch opened"
                        } else {
                            "새 분기 시작"
                        },
                        loaded
                    ));
                }
                self.apply_permissions(permissions, false);
                if let Some(model) = model {
                    self.model = model;
                    self.reconcile_effort();
                }
            }
            AgentUpdate::TurnStarted { id } => {
                self.reviewed_diff.clear();
                self.reviewed_changes.clear();
                self.welcome_dismissed = true;
                self.diff.clear();
                self.selected_diff_entry = None;
                self.recent_change_ids.clear();
                self.turn = id;
                self.work_phase = crate::agent::WorkPhase::Thinking;
                self.status = if self.interrupt_requested {
                    "중단됨"
                } else {
                    "실행 중"
                }
                .into();
                self.busy = true;
            }
            AgentUpdate::TurnSubmitted { id, status } => {
                self.submitted = None;
                self.submitted_skills.clear();
                if id
                    .as_ref()
                    .is_some_and(|id| self.finished_turns.contains(id))
                {
                    self.submitted_images.clear();
                } else {
                    self.active_images = std::mem::take(&mut self.submitted_images);
                }
                self.submitted_model = None;
                self.submitted_effort = None;
                if !id
                    .as_ref()
                    .is_some_and(|id| self.finished_turns.contains(id))
                {
                    self.turn = id;
                    self.busy = status == "inProgress";
                    self.status = if self.interrupt_requested {
                        "중단됨".into()
                    } else {
                        status
                    };
                }
            }
            AgentUpdate::TurnCompleted { id, status, error } => {
                if id
                    .as_ref()
                    .is_some_and(|id| self.turn.as_ref().is_some_and(|current| id != current))
                {
                    return;
                }
                let completed = id.as_deref().or(self.turn.as_deref());
                let newly_completed = completed.is_some_and(|id| !self.finished_turns.contains(id))
                    && status == "completed";
                let forced =
                    completed.is_some_and(|id| self.force_after_interrupt.as_deref() == Some(id));
                if let Some(id) = completed {
                    self.finished_turns.insert(id.into());
                }
                self.busy = false;
                self.interrupt_requested = false;
                self.turn = None;
                self.active_images.clear();
                if forced {
                    self.force_after_interrupt = None;
                    self.queue_paused = false;
                } else if status != "completed" {
                    self.queue_paused = !self.queued.is_empty();
                }
                self.status = if self.execution_limit.is_some() {
                    "원격 실행 제한 · 중단됨".into()
                } else {
                    status
                };
                if let Some(error) = error {
                    self.error(error);
                } else if newly_completed {
                    self.event_notice(self.preferences.notify_completed, "작업이 완료되었습니다.");
                }
            }
            AgentUpdate::EntryUpdated(entry) => {
                if self.interrupt_requested && entry.status == "inProgress" {
                    return;
                }
                self.entry_times.entry(entry.id.clone()).or_insert_with(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                });
                if entry.status == "inProgress" {
                    self.work_phase = if matches!(entry.kind, Kind::Tool | Kind::Change) {
                        crate::agent::WorkPhase::Tool
                    } else {
                        crate::agent::WorkPhase::Writing
                    };
                }
                self.upsert_entry(entry);
            }
            AgentUpdate::EntryDelta { id, kind, text } => {
                if self.interrupt_requested {
                    return;
                }
                self.entry_times.entry(id.clone()).or_insert_with(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                });
                self.work_phase = if kind == Kind::Tool {
                    if matches!(
                        self.work_phase,
                        crate::agent::WorkPhase::Command | crate::agent::WorkPhase::Shell
                    ) {
                        self.work_phase
                    } else {
                        crate::agent::WorkPhase::Tool
                    }
                } else {
                    crate::agent::WorkPhase::Writing
                };
                if !self.entries.iter().any(|entry| entry.id == id) {
                    self.upsert_entry(Entry {
                        id: id.clone(),
                        title: if kind == Kind::Tool {
                            "도구"
                        } else {
                            "에이전트"
                        }
                        .into(),
                        kind,
                        body: String::new(),
                        status: "inProgress".into(),
                        expanded: false,
                    });
                }
                if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
                    entry.body.push_str(&text);
                }
                self.invalidate_chat_rows();
            }
            AgentUpdate::ActivityUpdated(mut activity) => {
                if let Some(known) = self.agents.iter_mut().find(|a| a.id == activity.id) {
                    known.status = activity.status;
                    known.detail = activity.detail;
                } else {
                    if activity.name.is_empty() {
                        activity.name = format!("에이전트 {}", self.agents.len() + 1);
                    }
                    self.agents.push(activity);
                }
            }
            AgentUpdate::ActivityHistory {
                id,
                status,
                entries,
            } => {
                if self
                    .agents
                    .get(self.selected_agent)
                    .is_some_and(|a| a.id == id)
                {
                    self.agent_entries = entries;
                }
                if let Some(agent) = self.agents.iter_mut().find(|a| a.id == id) {
                    agent.status = status;
                }
            }
            AgentUpdate::ApprovalRequested(approval) => {
                if !self.readonly && !self.approvals.iter().any(|a| a.id == approval.id) {
                    self.approvals.push(approval);
                    self.approval_popup_dismissed = false;
                    self.focus = None;
                    self.event_notice(
                        self.preferences.notify_approval,
                        "승인 요청 도착 · 화면의 승인 카드에서 선택하세요",
                    );
                }
            }
            AgentUpdate::ApprovalResolved(id) => {
                self.approvals.retain(|a| a.id != id);
                if self.approvals.is_empty() {
                    self.approval_popup_dismissed = false;
                }
            }
            AgentUpdate::ApprovalReplyFailed { id, reason } => {
                self.approval_reply_failed(&id, &reason)
            }
            AgentUpdate::PermissionsUpdated {
                permissions,
                defaults_only,
            } => self.apply_permissions(permissions, defaults_only),
            AgentUpdate::PermissionApplied => self.permission_applied(),
            AgentUpdate::PermissionFailed(reason) => self.permission_failed(&reason),
            AgentUpdate::SessionsLoaded {
                sessions,
                cursor,
                append,
            } => {
                if !append {
                    self.sessions.clear();
                }
                for mut session in sessions {
                    session.title = one_line(&session.title, 55);
                    if self.thread.as_deref() == Some(&session.id)
                        && self.session_title == "새 대화"
                    {
                        self.session_title.clone_from(&session.title);
                    }
                    if let Some(known) = self.sessions.iter_mut().find(|s| s.id == session.id) {
                        *known = session;
                    } else {
                        self.sessions.push(session);
                    }
                }
                self.sessions_next_cursor = cursor;
                self.sessions_loaded = true;
            }
            AgentUpdate::SessionsFailed(reason) => {
                self.sessions_loaded = true;
                self.error(reason);
            }
            AgentUpdate::SkillsLoaded { skills, errors } => {
                self.skills = skills;
                self.skill_errors = errors;
                self.skills_loaded = true;
            }
            AgentUpdate::ModelsLoaded(models) => {
                self.models = models;
            }
            AgentUpdate::ModelCatalogLoaded(catalog) => self.model_catalog = catalog,
            AgentUpdate::ModelCatalogLoading => self.model_catalog_loading = true,
            AgentUpdate::ModelCatalogSources(sources) => {
                self.model_catalog_sources = sources;
                self.model_catalog_loading = false;
            }
            AgentUpdate::UsageUpdated(usage) => self.usage = usage,
            AgentUpdate::DiffUpdated(diff) => self.diff = diff,
            AgentUpdate::StatusUpdated { status, busy } => {
                self.status = if self.interrupt_requested && busy {
                    "중단됨".into()
                } else if self.execution_limit.is_some() {
                    if busy {
                        "원격 실행 제한 · 종료 대기".into()
                    } else {
                        "원격 실행 제한 · 중단됨".into()
                    }
                } else {
                    status
                };
                self.busy = busy;
            }
            AgentUpdate::OperationStarted { id, operation } => {
                self.pending.insert(id, operation);
                if matches!(operation, Operation::History | Operation::ActivityHistory) {
                    self.history_pending = true;
                }
                if operation == Operation::SwitchSession {
                    self.busy = true;
                }
                if operation == Operation::Sessions {
                    self.sessions_loaded = false;
                }
            }
            AgentUpdate::OperationFinished(id) => {
                if matches!(
                    self.pending.remove(&id),
                    Some(Operation::History | Operation::ActivityHistory)
                ) {
                    self.history_pending = false;
                }
            }
            AgentUpdate::SubmissionFailed(reason) => self.fail_submission(reason),
            AgentUpdate::InterruptFailed(reason) => self.interrupt_failed(&reason),
            AgentUpdate::TrustRequired(path) => self.trust_prompt = Some(path),
            AgentUpdate::TrustResolved(trusted) => {
                self.trust_prompt = None;
                self.trust_pending = false;
                self.workspace_trust = Some(trusted);
                self.restricted_workspace = !trusted;
            }
            AgentUpdate::TrustFailed(reason) => {
                self.trust_pending = false;
                self.error(reason);
            }
            AgentUpdate::Disconnected(reason) => {
                self.status = "연결 끊김".into();
                self.queue_paused = true;
                self.error(format!("{reason} · 창을 다시 열어 재연결하세요"));
            }
            AgentUpdate::Notice(message) => self.notify(message),
            AgentUpdate::Error(message) => {
                self.branch_pending = None;
                self.error(message);
            }
        }
    }

    pub fn upsert_entry(&mut self, mut entry: Entry) {
        if entry.kind == Kind::Change {
            self.recent_change_ids.insert(entry.id.clone());
        }
        if self.session_title == "새 대화"
            && entry.kind == Kind::User
            && !entry.body.trim().is_empty()
        {
            self.session_title = one_line(entry.body.lines().next().unwrap_or(""), 55);
        }
        if let Some(turn) = &self.turn {
            self.entry_turns.insert(entry.id.clone(), turn.clone());
        }
        entry.expanded |= self.expanded.contains(&entry.id);
        if let Some(index) = self.entries.iter().position(|old| old.id == entry.id) {
            // Everything stacks downward: the live progress (and the text the model is
            // writing now) stays the last entry, and the final reply takes its place there.
            let progress = self.entries[index].title == crate::agent::PROGRESS_TITLE && entry.kind == Kind::Assistant;
            if progress {
                self.entries.remove(index);
                self.entries.push(entry);
            } else {
                self.entries[index] = entry;
            }
        } else {
            self.entries.push(entry);
        }
        self.invalidate_chat_rows();
    }

    fn apply_permissions(&mut self, permissions: Permissions, defaults_only: bool) {
        if permissions.approval.is_some() && (!defaults_only || self.approval_mode.is_none()) {
            self.approval_mode = permissions.approval;
        }
        if permissions.sandbox.is_some() && (!defaults_only || self.sandbox_mode.is_none()) {
            self.sandbox_mode = permissions.sandbox;
        }
        if permissions.network.is_some() && (!defaults_only || self.network_mode.is_none()) {
            self.network_mode = permissions.network;
        }
    }

    fn reset_session_view(&mut self) {
        let model_engine = self.model_engine.clone();
        let model_memory_dirty = self.model_memory_dirty;
        let model_default_allowed = self.model_default_allowed;
        let preferences = self.preferences.clone();
        let model = self.model.clone();
        let skills = std::mem::take(&mut self.skills);
        let models = std::mem::take(&mut self.models);
        let editor = std::mem::take(&mut self.editor);
        let images = std::mem::take(&mut self.images);
        let selected = std::mem::take(&mut self.selected_skills);
        let trust = self.workspace_trust;
        let restricted = self.restricted_workspace;
        let pending = std::mem::take(&mut self.pending);
        let profiles = std::mem::take(&mut self.model_profiles);
        let catalog = std::mem::take(&mut self.model_catalog);
        let catalog_sources = std::mem::take(&mut self.model_catalog_sources);
        let catalog_loading = self.model_catalog_loading;
        let provider_accounts = std::mem::take(&mut self.provider_accounts);
        let local_models = std::mem::take(&mut self.local_models);
        let checkpoints = std::mem::take(&mut self.runtime_checkpoints);
        let backend = self.runtime_backend.clone();
        let routing_max_cost = self.routing_max_cost;
        let runtime_reload = self.runtime_reload;
        let effort = self.effort.clone();
        let quotas = std::mem::take(&mut self.quotas);
        let execution_limit = self.execution_limit;
        let execution_limit_model = self.execution_limit_model.take();
        let trail = std::mem::take(&mut self.session_trail);
        let pending_enter = self.pending_enter.take();
        let notice_log = std::mem::take(&mut self.notice_log);
        *self = App::new(false);
        self.session_trail = trail;
        self.pending_enter = pending_enter;
        self.notice_log = notice_log;
        self.panel_open = preferences.panel_open;
        self.preferences = preferences;
        self.model_engine = model_engine;
        self.model_memory_dirty = model_memory_dirty;
        self.model_default_allowed = model_default_allowed;
        self.model = model;
        self.skills = skills;
        self.skills_loaded = true;
        self.models = models;
        self.editor = editor;
        self.images = images;
        self.selected_skills = selected;
        self.workspace_trust = trust;
        self.restricted_workspace = restricted;
        self.pending = pending;
        self.model_profiles = profiles;
        self.model_catalog = catalog;
        self.model_catalog_sources = catalog_sources;
        self.model_catalog_loading = catalog_loading;
        self.provider_accounts = provider_accounts;
        self.local_models = local_models;
        self.runtime_checkpoints = checkpoints;
        self.runtime_backend = backend;
        self.routing_max_cost = routing_max_cost;
        self.runtime_reload = runtime_reload;
        self.effort = effort;
        self.quotas = quotas;
        self.execution_limit = execution_limit;
        self.execution_limit_model = execution_limit_model;
    }

    fn set_session(&mut self, snapshot: SessionSnapshot) {
        self.thread = Some(snapshot.id);
        if let Some(title) = snapshot.title {
            self.session_title = one_line(title.lines().next().unwrap_or(&title), 55);
        }
        if let Some(status) = &snapshot.status {
            self.status.clone_from(status);
        }
        if snapshot.status.is_some() || snapshot.turns.is_some() || self.turn.is_none() {
            self.busy = snapshot.status.as_deref() == Some("active");
        }
        if let Some(turns) = snapshot.turns {
            self.recent_change_ids.clear();
            for turn in turns {
                self.recent_change_ids.clear();
                if turn.status != "inProgress" {
                    if let Some(id) = &turn.id {
                        self.finished_turns.insert(id.clone());
                    }
                } else {
                    self.turn = turn.id.clone();
                    self.busy = true;
                }
                for entry in turn.entries {
                    let id = entry.id.clone();
                    self.upsert_entry(entry);
                    if let Some(turn_id) = &turn.id {
                        self.entry_turns.insert(id, turn_id.clone());
                    }
                }
                for activity in turn.activities {
                    self.apply_event(AgentEvent::global(AgentUpdate::ActivityUpdated(activity)));
                }
            }
        }
        if self.preferences.input_history == crate::preferences::InputHistory::Clear {
            self.input_history.clear();
        } else if self.input_history.is_empty() {
            self.input_history = self
                .entries
                .iter()
                .filter(|entry| entry.kind == Kind::User && !entry.body.trim().is_empty())
                .map(|entry| entry.body.clone())
                .collect();
        }
        if self.status == "연결 중" {
            self.status = "대기".into();
        }
    }
}
