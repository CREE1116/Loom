use super::*;
use crate::agent::{AgentEvent, AgentUpdate, Permissions, SessionSnapshot};

impl App {
    pub fn agent_command(&self, intent: &Intent) -> Option<AgentCommand> {
        Some(match intent {
            Intent::Answer(id, answers) => AgentCommand::Answer {
                request_id: id.clone(),
                answers: answers.clone(),
            },
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
            Intent::Fork(turn) => AgentCommand::Fork {
                turn_id: turn.clone(),
                model: self.model.clone(),
            },
            Intent::RefreshSessions => AgentCommand::RefreshSessions,
            Intent::LoadMoreSessions(cursor) => AgentCommand::MoreSessions(cursor.clone()),
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
            AgentUpdate::ModelProfilesLoaded(profiles) => {
                self.model_profiles = profiles;
                self.reconcile_effort();
            }
            AgentUpdate::EffortObserved(effort) => self.runtime_effort = effort,
            AgentUpdate::WorkPhaseUpdated(phase) => self.work_phase = phase,
            AgentUpdate::TokenUsageUpdated(usage) => self.set_tokens(usage),
            AgentUpdate::QuotaUpdated(quota) => self.update_quota(quota),
            AgentUpdate::ExecutionLimited { kind, reason } => self.execution_limited(kind, reason),
            AgentUpdate::ExecutionLimitCleared => self.clear_execution_limit(),
            AgentUpdate::TaskUpdated(task) => self.update_task(task),
            AgentUpdate::InputRequested(request) => self.input_requested(request),
            AgentUpdate::InputResolved(id) => self.input_resolved(&id),
            AgentUpdate::InputReplyFailed { id, reason } => self.input_reply_failed(&id, &reason),
            AgentUpdate::TaskStarted { id } => {
                self.active_tasks.insert(id);
            }
            AgentUpdate::TaskCompleted { id } => {
                self.active_tasks.remove(&id);
            }
            AgentUpdate::SessionLoaded {
                snapshot,
                switching,
                model,
                permissions,
            } => {
                if switching {
                    // Recovery from a failed initial connection is still the same
                    // unsent conversation. Normal session switches stay isolated.
                    let unsent = if self.thread.is_none() {
                        Some((std::mem::take(&mut self.queued), self.queue_paused))
                    } else {
                        None
                    };
                    self.reset_session_view();
                    if let Some((unsent, paused)) = unsent {
                        self.queued = unsent;
                        self.queue_paused = paused || self.execution_limit.is_some();
                    }
                }
                self.set_session(snapshot);
                self.apply_permissions(permissions, false);
                if let Some(model) = model {
                    self.model = model;
                    self.reconcile_effort();
                }
            }
            AgentUpdate::TurnStarted { id } => {
                self.diff.clear();
                self.selected_diff_entry = None;
                self.recent_change_ids.clear();
                self.turn = id;
                self.work_phase = crate::agent::WorkPhase::Thinking;
                self.status = "실행 중".into();
                self.busy = true;
            }
            AgentUpdate::TurnSubmitted { id, status } => {
                self.submitted = None;
                self.submitted_skills.clear();
                self.submitted_model = None;
                self.submitted_effort = None;
                if !id
                    .as_ref()
                    .is_some_and(|id| self.finished_turns.contains(id))
                {
                    self.turn = id;
                    self.busy = status == "inProgress";
                    self.status = status;
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
                let forced =
                    completed.is_some_and(|id| self.force_after_interrupt.as_deref() == Some(id));
                if let Some(id) = completed {
                    self.finished_turns.insert(id.into());
                }
                self.busy = false;
                self.turn = None;
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
                }
            }
            AgentUpdate::EntryUpdated(entry) => {
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
                self.work_phase = if kind == Kind::Tool {
                    crate::agent::WorkPhase::Tool
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
                    self.notify("승인 요청 도착 · 화면의 승인 카드에서 선택하세요");
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
            AgentUpdate::UsageUpdated(usage) => self.usage = usage,
            AgentUpdate::DiffUpdated(diff) => self.diff = diff,
            AgentUpdate::StatusUpdated { status, busy } => {
                self.status = if self.execution_limit.is_some() {
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
            AgentUpdate::Error(message) => self.error(message),
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
            self.entries[index] = entry;
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
    }

    fn reset_session_view(&mut self) {
        let preferences = self.preferences.clone();
        let model = self.model.clone();
        let skills = std::mem::take(&mut self.skills);
        let models = std::mem::take(&mut self.models);
        let editor = std::mem::take(&mut self.editor);
        let selected = std::mem::take(&mut self.selected_skills);
        let trust = self.workspace_trust;
        let restricted = self.restricted_workspace;
        let pending = std::mem::take(&mut self.pending);
        let profiles = std::mem::take(&mut self.model_profiles);
        let effort = self.effort.clone();
        let quotas = std::mem::take(&mut self.quotas);
        let execution_limit = self.execution_limit;
        *self = App::new(false);
        self.panel_open = preferences.panel_open;
        self.preferences = preferences;
        self.model = model;
        self.skills = skills;
        self.skills_loaded = true;
        self.models = models;
        self.editor = editor;
        self.selected_skills = selected;
        self.workspace_trust = trust;
        self.restricted_workspace = restricted;
        self.pending = pending;
        self.model_profiles = profiles;
        self.effort = effort;
        self.quotas = quotas;
        self.execution_limit = execution_limit;
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
        if self.status == "연결 중" {
            self.status = "대기".into();
        }
    }
}
