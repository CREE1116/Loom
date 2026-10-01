//! Transitional backend: Codex owns its execution loop until the native Core replaces it.
use super::codec;
use crate::{
    agent::*,
    runtime,
    transport::{Client, Event as RpcEvent},
    trust,
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
};

pub struct Options {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub initial_session: Option<String>,
    pub readonly: bool,
    pub auto_restore: bool,
}

struct Pending {
    method: String,
    operation: Operation,
    scope: Option<String>,
    generation: Option<u64>,
}

struct PendingApproval {
    wire_id: Value,
    choices: HashMap<String, Value>,
    answering: bool,
}

struct PendingInput {
    wire_id: Value,
    request: InputRequest,
    session: String,
    answering: bool,
}

pub struct CodexBackend {
    client: Client,
    options: Options,
    pending: HashMap<u64, Pending>,
    events: VecDeque<AgentEvent>,
    approvals: HashMap<String, PendingApproval>,
    inputs: HashMap<String, PendingInput>,
    session: Option<String>,
    turn: Option<String>,
    finished_turns: HashSet<String>,
    models: Vec<String>,
    entries: HashMap<String, Entry>,
    generation: u64,
    initialized: bool,
    trust_pending: bool,
    restricted: bool,
    busy: bool,
}

impl CodexBackend {
    pub fn connect(endpoint: &str, options: Options) -> Result<Self> {
        let client = Client::connect(endpoint)?;
        let trusted = trust::local_trust(&options.cwd);
        let trust_pending = !options.readonly && trusted != Some(true);
        let mut backend = Self {
            client,
            options,
            pending: HashMap::new(),
            events: VecDeque::new(),
            approvals: HashMap::new(),
            inputs: HashMap::new(),
            session: None,
            turn: None,
            finished_turns: HashSet::new(),
            models: vec![],
            entries: HashMap::new(),
            generation: 0,
            initialized: false,
            trust_pending,
            restricted: false,
            busy: false,
        };
        if trust_pending {
            backend.emit(AgentUpdate::TrustRequired(
                backend.options.cwd.display().to_string(),
            ));
        } else if !backend.options.readonly {
            backend.emit(AgentUpdate::TrustResolved(true));
        }
        let id = backend.client.initialize()?;
        backend.track(id, "initialize", Operation::Initialize, None, false);
        Ok(backend)
    }

    fn emit(&mut self, update: AgentUpdate) {
        self.events.push_back(AgentEvent::global(update));
    }
    fn scoped(&mut self, update: AgentUpdate) {
        self.events.push_back(AgentEvent {
            session_id: self.session.clone(),
            update,
        });
    }
    fn track(
        &mut self,
        id: u64,
        method: &str,
        operation: Operation,
        scope: Option<String>,
        session_bound: bool,
    ) {
        self.pending.insert(
            id,
            Pending {
                method: method.into(),
                operation,
                scope,
                generation: session_bound.then_some(self.generation),
            },
        );
        self.emit(AgentUpdate::OperationStarted { id, operation });
    }
    fn request(
        &mut self,
        method: &str,
        params: Value,
        operation: Operation,
        session_bound: bool,
    ) -> Result<()> {
        let id = self.client.request(method, params)?;
        self.track(id, method, operation, self.session.clone(), session_bound);
        Ok(())
    }
    fn in_flight(&self, operation: Operation) -> bool {
        self.pending.values().any(|p| p.operation == operation)
    }

    fn open_session(&mut self, requested: Option<String>, switching: bool) -> Result<()> {
        let (method, mut params) = match requested {
            Some(id) if self.options.readonly => {
                ("thread/read", json!({"threadId":id,"includeTurns":true}))
            }
            Some(id) => (
                "thread/resume",
                json!({"threadId":id,"excludeTurns":false,"model":self.options.model}),
            ),
            None => (
                "thread/start",
                json!({"cwd":self.options.cwd,"historyMode":"legacy","model":self.options.model}),
            ),
        };
        if self.restricted && !self.options.readonly {
            params["approvalPolicy"] = json!("on-request");
            params["sandbox"] = json!("workspace-write");
        }
        self.generation += 1;
        self.request(
            method,
            params,
            if switching {
                Operation::SwitchSession
            } else {
                Operation::OpenSession
            },
            true,
        )?;
        self.busy = true;
        Ok(())
    }

    fn handle_rpc(&mut self, message: Value) -> Result<()> {
        if let Some(method) = message["method"].as_str() {
            let params = &message["params"];
            if !message["id"].is_null() {
                let allowed = !self.options.readonly
                    && params["threadId"].as_str() == self.session.as_deref();
                if allowed && method == "item/tool/requestUserInput" {
                    match codec::input_request(&message["id"], params) {
                        Ok(request) => {
                            if !self.inputs.contains_key(&request.id) {
                                self.inputs.insert(
                                    request.id.clone(),
                                    PendingInput {
                                        wire_id: message["id"].clone(),
                                        request: request.clone(),
                                        session: self.session.clone().unwrap(),
                                        answering: false,
                                    },
                                );
                                self.scoped(AgentUpdate::InputRequested(request));
                            }
                        }
                        Err(error) => {
                            self.client
                                .reject(message["id"].clone(), "Invalid question request")?;
                            self.scoped(AgentUpdate::Error(format!(
                                "질문 요청 형식 오류: {error}"
                            )));
                        }
                    }
                } else if allowed
                    && let Some((mut approval, choices)) =
                        codec::approval(&message["id"], method, params)
                {
                    if let Some(entry) = params["itemId"]
                        .as_str()
                        .and_then(|id| self.entries.get(id))
                    {
                        approval.summary = format!("대상: {}\n{}", entry.title, approval.summary);
                        approval.detail =
                            format!("{}\n{}\n\n{}", entry.title, entry.body, approval.detail);
                    }
                    self.approvals
                        .entry(approval.id.clone())
                        .or_insert(PendingApproval {
                            wire_id: message["id"].clone(),
                            choices,
                            answering: false,
                        });
                    self.scoped(AgentUpdate::ApprovalRequested(approval));
                } else {
                    self.client.reject(
                        message["id"].clone(),
                        "This UI does not support this server request",
                    )?;
                    self.emit(AgentUpdate::Error(format!(
                        "지원하지 않는 요청: {method}. 코어에 오류를 반환했습니다."
                    )));
                }
            } else {
                if method == "skills/changed" {
                    self.request(
                        "skills/list",
                        json!({"cwds":[self.options.cwd],"forceReload":true}),
                        Operation::Skills,
                        false,
                    )?;
                }
                if method == "serverRequest/resolved" {
                    self.approvals.remove(&params["requestId"].to_string());
                    if let Some(input) = self.inputs.remove(&params["requestId"].to_string()) {
                        self.events.push_back(AgentEvent::scoped(
                            input.session,
                            AgentUpdate::InputResolved(input.request.id),
                        ));
                    }
                }
                let current = params["threadId"]
                    .as_str()
                    .is_none_or(|id| self.session.as_deref() == Some(id));
                for event in codec::notification(method, params) {
                    if current {
                        match &event.update {
                            AgentUpdate::EntryUpdated(entry) => {
                                self.entries.insert(entry.id.clone(), entry.clone());
                            }
                            AgentUpdate::TurnStarted { id } => {
                                self.turn = id.clone();
                                self.busy = true;
                            }
                            AgentUpdate::TurnCompleted { id, .. } => {
                                if let Some(id) = id {
                                    self.finished_turns.insert(id.clone());
                                }
                                if id.as_ref().is_some_and(|id| {
                                    self.turn.as_ref().is_some_and(|current| current != id)
                                }) {
                                    continue;
                                }
                                self.turn = None;
                                self.busy = false;
                            }
                            AgentUpdate::StatusUpdated { busy, .. } => self.busy = *busy,
                            _ => {}
                        }
                    }
                    self.events.push_back(event);
                }
            }
            return Ok(());
        }
        let Some(id) = message["id"].as_u64() else {
            return Ok(());
        };
        let Some(pending) = self.pending.remove(&id) else {
            return Ok(());
        };
        self.emit(AgentUpdate::OperationFinished(id));
        if pending.generation.is_some_and(|g| g != self.generation) {
            return Ok(());
        }
        if !message["error"].is_null() {
            let error = message["error"]["message"].as_str().unwrap_or("RPC failed");
            if pending.method == "thread/resume"
                && self.options.auto_restore
                && ["not materialized", "not found", "does not exist"]
                    .iter()
                    .any(|s| error.contains(s))
            {
                self.options.auto_restore = false;
                runtime::forget_recent_thread(&self.options.cwd)?;
                self.emit(AgentUpdate::Notice(
                    "최근 대화가 저장되지 않아 새 대화를 시작합니다.".into(),
                ));
                return self.open_session(None, false);
            }
            if matches!(
                pending.operation,
                Operation::History | Operation::ActivityHistory | Operation::OpenSession
            ) && error.contains("not materialized")
            {
                let session = self
                    .session
                    .as_ref()
                    .or(self.options.initial_session.as_ref());
                if let Some(session) = session {
                    let request = self.client.request(
                        "thread/read",
                        json!({"threadId":session,"includeTurns":false}),
                    )?;
                    self.track(
                        request,
                        "metadata",
                        Operation::OpenSession,
                        self.session.clone(),
                        true,
                    );
                    self.emit(AgentUpdate::Notice(
                        "첫 사용자 메시지 전 · 아직 저장된 대화가 없습니다.".into(),
                    ));
                    return Ok(());
                }
            }
            let update = match pending.operation {
                Operation::Trust => {
                    AgentUpdate::TrustFailed(format!("폴더 신뢰 설정 실패: {error}"))
                }
                Operation::Permissions => AgentUpdate::PermissionFailed(error.into()),
                Operation::Interrupt => AgentUpdate::InterruptFailed(error.into()),
                Operation::Submit => AgentUpdate::SubmissionFailed(error.into()),
                Operation::Sessions | Operation::MoreSessions => {
                    AgentUpdate::SessionsFailed(error.into())
                }
                Operation::Skills => AgentUpdate::SkillsLoaded {
                    skills: vec![],
                    errors: vec![error.into()],
                },
                _ => AgentUpdate::Error(error.into()),
            };
            self.emit(update);
            if matches!(
                pending.operation,
                Operation::SwitchSession | Operation::OpenSession
            ) {
                self.busy = false;
                self.emit(AgentUpdate::StatusUpdated {
                    status: "오류".into(),
                    busy: false,
                });
            }
            return Ok(());
        }
        let result = &message["result"];
        match pending.operation {
            Operation::Initialize => {
                self.initialized = true;
                self.client.send(json!({"method":"initialized"}))?;
                self.request(
                    "config/read",
                    json!({"cwd":self.options.cwd,"includeLayers":false}),
                    Operation::Config,
                    false,
                )?;
                if !self.trust_pending {
                    self.open_session(self.options.initial_session.clone(), false)?;
                }
                self.request(
                    "skills/list",
                    json!({"cwds":[self.options.cwd]}),
                    Operation::Skills,
                    false,
                )?;
                self.request("model/list", json!({"limit":100}), Operation::Models, false)?;
            }
            Operation::Config => self.emit(AgentUpdate::PermissionsUpdated {
                permissions: codec::permissions(&result["config"], true),
                defaults_only: true,
            }),
            Operation::Trust => {
                self.trust_pending = false;
                self.restricted = false;
                self.emit(AgentUpdate::TrustResolved(true));
                if self.initialized {
                    self.open_session(self.options.initial_session.clone(), false)?;
                }
            }
            Operation::Permissions => self.scoped(AgentUpdate::PermissionApplied),
            Operation::Sessions | Operation::MoreSessions => self.emit(codec::session_page(
                result,
                pending.operation == Operation::MoreSessions,
            )),
            Operation::Skills => {
                let (skills, errors) = codec::skills(result);
                self.emit(AgentUpdate::SkillsLoaded { skills, errors });
            }
            Operation::Models => {
                if let Some(models) = result["data"].as_array() {
                    for model in models {
                        if let Some(name) = model["model"].as_str().or_else(|| model["id"].as_str())
                            && !self.models.iter().any(|m| m == name)
                        {
                            self.models.push(name.into());
                        }
                    }
                }
                self.emit(AgentUpdate::ModelsLoaded(self.models.clone()));
                if let Some(cursor) = result["nextCursor"].as_str() {
                    self.request(
                        "model/list",
                        json!({"limit":100,"cursor":cursor}),
                        Operation::Models,
                        false,
                    )?;
                }
            }
            Operation::OpenSession | Operation::SwitchSession | Operation::History => {
                let Some(snapshot) = codec::snapshot(&result["thread"]) else {
                    self.busy = false;
                    self.emit(AgentUpdate::StatusUpdated {
                        status: "오류".into(),
                        busy: false,
                    });
                    self.emit(AgentUpdate::Error("세션 응답이 올바르지 않습니다.".into()));
                    return Ok(());
                };
                self.session = Some(snapshot.id.clone());
                if pending.operation == Operation::SwitchSession {
                    self.entries.clear();
                    self.finished_turns.clear();
                }
                self.busy = snapshot.status.as_deref() == Some("active");
                if let Some(turns) = &snapshot.turns {
                    for entry in turns.iter().flat_map(|t| &t.entries) {
                        self.entries.insert(entry.id.clone(), entry.clone());
                    }
                    self.turn = turns
                        .iter()
                        .rev()
                        .find(|t| t.status == "inProgress")
                        .and_then(|t| t.id.clone());
                    for turn in turns.iter().filter(|t| t.status != "inProgress") {
                        if let Some(id) = &turn.id {
                            self.finished_turns.insert(id.clone());
                        }
                    }
                    self.busy |= self.turn.is_some();
                }
                if let Some(model) = result["model"].as_str() {
                    self.options.model = Some(model.into());
                }
                let remember = pending.method != "thread/start";
                if remember && !self.options.readonly {
                    runtime::remember_thread(&self.options.cwd, &snapshot.id)?;
                }
                self.emit(AgentUpdate::SessionLoaded {
                    snapshot,
                    switching: pending.operation == Operation::SwitchSession,
                    model: self.options.model.clone(),
                    permissions: codec::permissions(result, false),
                });
                if pending.method == "thread/fork" {
                    self.emit(AgentUpdate::Notice(
                        "새 분기 시작 · 원본 대화는 기록에 남습니다".into(),
                    ));
                }
            }
            Operation::Submit => {
                if !self.options.readonly
                    && let Some(id) = &self.session
                {
                    runtime::remember_thread(&self.options.cwd, id)?;
                }
                let id = result["turn"]["id"].as_str().map(str::to_owned);
                let status = result["turn"]["status"]
                    .as_str()
                    .unwrap_or("inProgress")
                    .to_owned();
                if !id
                    .as_ref()
                    .is_some_and(|id| self.finished_turns.contains(id))
                {
                    self.turn = id.clone();
                    self.busy = status == "inProgress";
                }
                self.scoped(AgentUpdate::TurnSubmitted { id, status });
            }
            Operation::ActivityHistory => {
                if pending.scope != self.session {
                    return Ok(());
                }
                let Some(snapshot) = codec::snapshot(&result["thread"]) else {
                    return Ok(());
                };
                self.scoped(AgentUpdate::ActivityHistory {
                    id: snapshot.id,
                    status: snapshot.status.unwrap_or_else(|| "unknown".into()),
                    entries: snapshot
                        .turns
                        .unwrap_or_default()
                        .into_iter()
                        .flat_map(|t| t.entries)
                        .collect(),
                });
            }
            Operation::Interrupt => {}
        }
        Ok(())
    }
}

impl AgentBackend for CodexBackend {
    fn command(&mut self, command: AgentCommand) -> Result<()> {
        if self.options.readonly
            && !matches!(
                command,
                AgentCommand::ReadActivity(_)
                    | AgentCommand::RefreshSessions
                    | AgentCommand::MoreSessions(_)
                    | AgentCommand::RefreshSkills
                    | AgentCommand::RefreshModels
            )
        {
            bail!("열람 창에서는 실행을 변경할 수 없습니다.");
        }
        match command {
            AgentCommand::Answer {
                request_id,
                answers,
            } => {
                let result = match self.inputs.get_mut(&request_id) {
                    Some(input)
                        if input.session == self.session.as_deref().unwrap_or("")
                            && !input.answering =>
                    {
                        match input.request.validate_answers(&answers) {
                            Ok(values) => {
                                let payload: serde_json::Map<String, Value> = values
                                    .into_iter()
                                    .map(|(id, value)| (id, json!({"answers":[value]})))
                                    .collect();
                                match self
                                    .client
                                    .reply(input.wire_id.clone(), json!({"answers":payload}))
                                {
                                    Ok(()) => {
                                        input.answering = true;
                                        Ok(())
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Some(input) if input.answering => return Ok(()),
                    _ => Err(anyhow::anyhow!("존재하지 않거나 다른 세션의 질문입니다.")),
                };
                if let Err(error) = result {
                    self.scoped(AgentUpdate::InputReplyFailed {
                        id: request_id,
                        reason: error.to_string(),
                    });
                }
            }
            AgentCommand::Explore(_) => bail!("Code exploration is owned by the native Core"),
            AgentCommand::Submit {
                text,
                model,
                skills,
            } => {
                let result = if self.trust_pending
                    || self.busy
                    || !self.approvals.is_empty()
                    || !self.inputs.is_empty()
                    || self.in_flight(Operation::Submit)
                {
                    Err(anyhow::anyhow!(
                        "현재 실행 또는 승인 처리가 끝나지 않았습니다."
                    ))
                } else if let Some(session) = self.session.clone() {
                    self.options.model = (!model.is_empty()).then_some(model);
                    self.request("turn/start", json!({"threadId":session,"input":codec::turn_input(&text,&skills),"model":self.options.model}), Operation::Submit, true)
                } else {
                    Err(anyhow::anyhow!("세션 연결이 준비되지 않았습니다."))
                };
                if let Err(error) = result {
                    self.scoped(AgentUpdate::SubmissionFailed(error.to_string()));
                }
            }
            AgentCommand::Interrupt => {
                if !self.in_flight(Operation::Interrupt)
                    && let Err(error) = self.request(
                        "turn/interrupt",
                        json!({"threadId":self.session,"turnId":self.turn}),
                        Operation::Interrupt,
                        true,
                    )
                {
                    self.scoped(AgentUpdate::InterruptFailed(error.to_string()));
                }
            }
            AgentCommand::Reply { request_id, choice } => {
                let result = match self.approvals.get_mut(&request_id) {
                    Some(approval) if !approval.answering => match approval.choices.get(&choice) {
                        Some(payload) => {
                            let sent = self.client.reply(approval.wire_id.clone(), payload.clone());
                            if sent.is_ok() {
                                approval.answering = true;
                            }
                            sent
                        }
                        None => Err(anyhow::anyhow!("지원하지 않는 승인 선택지")),
                    },
                    Some(_) => return Ok(()),
                    None => Err(anyhow::anyhow!("이미 처리되었거나 존재하지 않는 승인 요청")),
                };
                if let Err(error) = result {
                    self.scoped(AgentUpdate::ApprovalReplyFailed {
                        id: request_id,
                        reason: error.to_string(),
                    });
                }
            }
            AgentCommand::OpenSession { id, model } => {
                if self.busy
                    || self.in_flight(Operation::Submit)
                    || !self.approvals.is_empty()
                    || !self.inputs.is_empty()
                    || self.trust_pending
                {
                    bail!("실행과 승인 처리를 끝낸 뒤 대화를 전환하세요.");
                }
                let id = if id.as_deref() == Some("last") {
                    Some(runtime::last_thread(&self.options.cwd)?)
                } else {
                    id
                };
                self.options.auto_restore = false;
                self.options.model = (!model.is_empty()).then_some(model);
                self.open_session(id, true)?;
                self.emit(AgentUpdate::Notice("대화를 전환하는 중…".into()));
            }
            AgentCommand::Fork { turn_id, model } => {
                if self.busy
                    || !self.approvals.is_empty()
                    || !self.inputs.is_empty()
                    || !self.finished_turns.contains(&turn_id)
                {
                    bail!("완료된 대화만 분기할 수 있습니다.");
                }
                let mut params = json!({"threadId":self.session,"lastTurnId":turn_id,"excludeTurns":false,"model":model});
                if self.restricted {
                    params["approvalPolicy"] = json!("on-request");
                    params["sandbox"] = json!("workspace-write");
                }
                self.generation += 1;
                self.request("thread/fork", params, Operation::SwitchSession, true)?;
                self.busy = true;
                self.emit(AgentUpdate::Notice(
                    "선택한 대화에서 새 분기를 여는 중…".into(),
                ));
            }
            AgentCommand::RefreshSessions => {
                if !self.in_flight(Operation::Sessions) && !self.in_flight(Operation::MoreSessions)
                {
                    self.request(
                        "thread/list",
                        json!({"cwd":self.options.cwd,"sortKey":"updated_at","limit":15}),
                        Operation::Sessions,
                        false,
                    )?;
                }
            }
            AgentCommand::MoreSessions(cursor) => {
                if !self.in_flight(Operation::Sessions) && !self.in_flight(Operation::MoreSessions)
                {
                    self.request("thread/list", json!({"cwd":self.options.cwd,"sortKey":"updated_at","limit":15,"cursor":cursor}), Operation::MoreSessions, false)?;
                }
            }
            AgentCommand::RefreshSkills => self.request(
                "skills/list",
                json!({"cwds":[self.options.cwd],"forceReload":true}),
                Operation::Skills,
                false,
            )?,
            AgentCommand::RefreshModels => {
                if !self.in_flight(Operation::Models) {
                    self.models.clear();
                    self.request("model/list", json!({"limit":100}), Operation::Models, false)?;
                }
            }
            AgentCommand::ReadActivity(id) => {
                let operation = if self.options.readonly {
                    Operation::History
                } else {
                    Operation::ActivityHistory
                };
                if !self.in_flight(operation) {
                    self.request(
                        "thread/read",
                        json!({"threadId":id,"includeTurns":true}),
                        operation,
                        true,
                    )?;
                }
            }
            AgentCommand::UpdatePermission { kind, value } => {
                let result = self
                    .session
                    .as_ref()
                    .and_then(|id| permission_update_params(id, &kind, &value, &self.options.cwd));
                if let Some(params) = result {
                    if let Err(error) = self.request(
                        "thread/settings/update",
                        params,
                        Operation::Permissions,
                        true,
                    ) {
                        self.scoped(AgentUpdate::PermissionFailed(error.to_string()));
                    }
                } else {
                    self.scoped(AgentUpdate::PermissionFailed(
                        "올바르지 않은 권한 값 또는 세션 연결 없음".into(),
                    ));
                }
            }
            AgentCommand::TrustWorkspace(trusted) => {
                if !self.trust_pending {
                    return Ok(());
                }
                if trusted {
                    if let Some(key_path) = trust::trust_key_path(&self.options.cwd) {
                        if let Err(error) = self.request(
                            "config/value/write",
                            json!({"keyPath":key_path,"mergeStrategy":"upsert","value":"trusted"}),
                            Operation::Trust,
                            false,
                        ) {
                            self.emit(AgentUpdate::TrustFailed(error.to_string()));
                        }
                    } else {
                        self.emit(AgentUpdate::TrustFailed(
                            "폴더 경로를 처리할 수 없습니다.".into(),
                        ));
                    }
                } else {
                    self.trust_pending = false;
                    self.restricted = true;
                    self.emit(AgentUpdate::TrustResolved(false));
                    if self.initialized {
                        self.open_session(self.options.initial_session.clone(), false)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<AgentEvent>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        // Bound decoding so quiet or unknown messages cannot starve terminal input.
        for _ in 0..32 {
            match self.client.incoming.try_recv() {
                Ok(RpcEvent::Message(message)) => self.handle_rpc(message)?,
                Ok(RpcEvent::Disconnected(reason)) => self.emit(AgentUpdate::Disconnected(reason)),
                Err(_) => break,
            }
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }
}

pub fn permission_update_params(
    id: &str,
    kind: &str,
    value: &str,
    cwd: &std::path::Path,
) -> Option<Value> {
    match (kind, value) {
        ("sandbox", "readOnly") => {
            Some(json!({"threadId":id,"sandboxPolicy":{"type":"readOnly","networkAccess":false}}))
        }
        ("sandbox", "workspaceWrite") => Some(
            json!({"threadId":id,"sandboxPolicy":{"type":"workspaceWrite","writableRoots":[cwd],"networkAccess":false}}),
        ),
        ("sandbox", "dangerFullAccess") => {
            Some(json!({"threadId":id,"sandboxPolicy":{"type":"dangerFullAccess"}}))
        }
        ("approval", "untrusted" | "on-request" | "never") => {
            Some(json!({"threadId":id,"approvalPolicy":value}))
        }
        _ => None,
    }
}
