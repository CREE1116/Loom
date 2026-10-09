//! The UI/Core boundary. No provider protocol or transport payloads belong here.
pub(crate) const PROGRESS_TITLE: &str = "진행 상태";
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Assistant,
    Tool,
    Change,
    System,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub status: String,
    pub expanded: bool,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub status: String,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerKind {
    Local,
    Small,
    Medium,
    Flagship,
}
impl WorkerKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "LOCAL",
            Self::Small => "SMALL",
            Self::Medium => "MEDIUM",
            Self::Flagship => "FLAGSHIP",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskStatus {
    #[default]
    Pending,
    Ready,
    Running,
    Blocked,
    Completed,
    Failed,
    Cancelled,
    NeedsReview,
}

/// Core's actual execution decision; scoped independent graph groups may run in parallel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    #[default]
    Single,
    Split,
}

/// The task's boundary as Core knows it: files and symbols it changes, and how the
/// code graph groups them. Filled once the change is known; never a model's guess.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskPartition {
    pub mode: ExecutionMode,
    pub reason: String,
    pub files: Vec<String>,
    pub symbols: Vec<String>,
    /// Weakly connected groups of `symbols` in the code graph (independent pieces).
    pub groups: Vec<Vec<String>>,
}

/// Measured remote cost of a task. Bytes are context text by phase; tokens are the
/// runtime's reported usage. Missing usage stays `None`, never zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TaskCost {
    pub orient_bytes: u64,
    pub validate_bytes: u64,
    pub edit_bytes: u64,
    pub message_bytes: u64,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub model_calls: u32,
}

/// A read-only UI projection. Task ownership and critical-path decisions stay in Core.
#[derive(Clone, Debug, Default)]
pub struct ActivityTask {
    pub id: String,
    pub parent: Option<String>,
    pub title: String,
    pub worker: Option<WorkerKind>,
    pub model: Option<String>,
    pub status: TaskStatus,
    pub dependencies: Vec<String>,
    pub reason: Option<String>,
    pub critical: bool,
    pub elapsed_ms: Option<u64>,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// Session that carries out the task; the UI can enter it and come back.
    pub session: Option<String>,
    pub partition: Option<Box<TaskPartition>>,
    pub cost: Option<TaskCost>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkPhase {
    #[default]
    Thinking,
    Writing,
    Tool,
    Command,
    Shell,
}

#[derive(Clone, Debug, Default)]
pub struct TokenUsage {
    pub total: Option<u64>,
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub output: Option<u64>,
    pub reasoning_output: Option<u64>,
    pub last: Option<u64>,
    pub context_window: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct EffortOption {
    pub value: String,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct ModelProfile {
    pub id: String,
    pub efforts: Vec<EffortOption>,
    pub default_effort: Option<String>,
}

/// Display metadata from the provider catalog; rates are USD per million tokens.
#[derive(Clone, Debug)]
pub struct ModelCatalogEntry {
    pub id: String,
    pub cost: String,
    pub price: Option<(f64, f64)>,
    pub tools: bool,
    pub context: Option<u64>,
    /// Readable model name when the id does not carry one (LMX ids are content hashes).
    pub name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ModelCatalogSource {
    pub name: String,
    pub count: Option<usize>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct QuotaWindow {
    pub used_percent: u32,
    pub duration_minutes: Option<u64>,
    pub resets_at: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct QuotaSnapshot {
    pub id: String,
    pub label: String,
    pub model: Option<String>,
    pub primary: Option<QuotaWindow>,
    pub secondary: Option<QuotaWindow>,
    pub credits_available: Option<bool>,
    pub credits_unlimited: Option<bool>,
    pub spend_control_reached: Option<bool>,
    pub reached: Option<LimitKind>,
}
impl QuotaSnapshot {
    pub fn can_resume(&self) -> bool {
        if self.spend_control_reached == Some(true) || self.reached.is_some() {
            return false;
        }
        if self.credits_unlimited == Some(true) || self.credits_available == Some(true) {
            return true;
        }
        let windows: Vec<_> = self.primary.iter().chain(&self.secondary).collect();
        !windows.is_empty() && windows.iter().all(|window| window.used_percent < 100)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    UsageExhausted,
    RateLimited,
}

/// Opaque choice token. Only the backend knows how to execute it.
#[derive(Clone, Debug)]
pub struct Choice {
    pub label: String,
    pub result: String,
}

#[derive(Clone, Debug)]
pub struct Approval {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub detail: String,
    pub choices: Vec<Choice>,
    pub answering: bool,
    pub expanded: bool,
}

#[derive(Clone, Debug)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub prompt: String,
    pub options: Vec<QuestionOption>,
    pub allow_custom: bool,
    pub secret: bool,
}

#[derive(Clone, Debug)]
pub struct InputRequest {
    pub id: String,
    pub questions: Vec<Question>,
    pub blocking: bool,
}

/// A choice index or explicit text; defaults never count as user answers.
#[derive(Clone, Debug, PartialEq)]
pub enum AnswerValue {
    Option(usize),
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuestionAnswer {
    pub question_id: String,
    pub value: AnswerValue,
}

impl InputRequest {
    pub fn validate_answers(
        &self,
        answers: &[QuestionAnswer],
    ) -> anyhow::Result<Vec<(String, String)>> {
        anyhow::ensure!(
            answers.len() == self.questions.len(),
            "모든 질문에 답해주세요."
        );
        let mut result = Vec::new();
        for question in &self.questions {
            let matching: Vec<_> = answers
                .iter()
                .filter(|a| a.question_id == question.id)
                .collect();
            anyhow::ensure!(matching.len() == 1, "질문별 답변은 정확히 하나여야 합니다.");
            let value = match &matching[0].value {
                AnswerValue::Option(index) => question
                    .options
                    .get(*index)
                    .map(|option| option.label.clone())
                    .ok_or_else(|| anyhow::anyhow!("유효하지 않은 선택지입니다."))?,
                AnswerValue::Text(text) => {
                    anyhow::ensure!(
                        question.allow_custom && !text.trim().is_empty() && text.len() <= 16_384,
                        "직접 입력이 허용된 질문에 1~16384바이트를 입력해주세요."
                    );
                    text.clone()
                }
            };
            result.push((question.id.clone(), value));
        }
        Ok(result)
    }
}

/// A conversation title from a request: its first line with text, whitespace collapsed,
/// so a request pasted after a blank line still names the conversation.
pub fn title_of(text: &str, chars: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(chars).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub status: String,
    pub updated_at: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub path: String,
    pub description: String,
}

#[derive(Clone, Debug, Default)]
pub struct Permissions {
    pub approval: Option<String>,
    pub sandbox: Option<String>,
    /// `allow` or `deny`; `None` when the backend does not report it.
    pub network: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TurnSnapshot {
    pub id: Option<String>,
    pub status: String,
    pub entries: Vec<Entry>,
    pub activities: Vec<Agent>,
}

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub id: String,
    pub title: Option<String>,
    pub status: Option<String>,
    pub turns: Option<Vec<TurnSnapshot>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalAction {
    Show,
    Set(String),
    Pause,
    Resume,
    Clear,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalState {
    pub objective: String,
    pub status: String,
    pub tokens_used: Option<u64>,
    pub token_budget: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Goal,
    Compact,
    AgentSendRead,
    AgentStopRead,
    AgentControl,
    AsyncAnswer,
    CloseSession,
    Usage,
    Initialize,
    OpenSession,
    SwitchSession,
    History,
    ActivityHistory,
    Sessions,
    MoreSessions,
    Skills,
    Models,
    Submit,
    Interrupt,
    Permissions,
    Trust,
    Config,
}

#[derive(Clone, Debug)]
pub enum AgentCommand {
    Goal(GoalAction),
    Compact,
    Inquiry(String),
    DirectAgent {
        id: String,
        text: Option<String>,
    },
    RefreshUsage,
    Explore(String),
    Answer {
        request_id: String,
        answers: Vec<QuestionAnswer>,
    },
    CancelInput { request_id: String },
    Submit {
        text: String,
        model: String,
        effort: Option<String>,
        skills: Vec<Skill>,
        images: Vec<crate::attachment::ImageAttachment>,
    },
    Interrupt,
    Reply {
        request_id: String,
        choice: String,
    },
    OpenSession {
        id: Option<String>,
        model: String,
    },
    Fork {
        turn_id: String,
        model: String,
    },
    Rewind {
        turn_id: String,
        model: String,
    },
    PreviewRewind {
        turn_id: String,
    },
    RefreshSessions,
    MoreSessions(String),
    /// Remove a saved conversation other than the open one.
    DeleteSession(String),
    RefreshSkills,
    RefreshModels,
    ReadActivity(String),
    UpdatePermission {
        kind: String,
        value: String,
    },
    TrustWorkspace(bool),
}

/// The scope prevents a late event from altering another conversation.
#[derive(Clone, Debug)]
pub struct AgentEvent {
    pub session_id: Option<String>,
    pub update: AgentUpdate,
}

impl AgentEvent {
    pub fn global(update: AgentUpdate) -> Self {
        Self {
            session_id: None,
            update,
        }
    }
    pub fn scoped(session: impl Into<String>, update: AgentUpdate) -> Self {
        Self {
            session_id: Some(session.into()),
            update,
        }
    }
}

#[derive(Clone, Debug)]
pub enum AgentUpdate {
    /// Generation speed of a local model, while it writes and after it finished.
    ModelSpeed {
        tokens_per_second: f32,
    },
    ModelResolved {
        requested: String,
        provider: String,
        model: String,
    },
    RewindPreview {
        turn_id: String,
        files: usize,
        added: usize,
        removed: usize,
        blocked: Option<String>,
    },
    GoalUpdated(Option<GoalState>),
    CompressionState {
        running: bool,
        error: Option<String>,
    },
    ModelProfilesLoaded(Vec<ModelProfile>),
    EffortObserved(Option<String>),
    WorkPhaseUpdated(WorkPhase),
    TokenUsageUpdated(TokenUsage),
    QuotaUpdated(QuotaSnapshot),
    ExecutionLimited {
        kind: LimitKind,
        reason: String,
    },
    ExecutionLimitCleared,
    TaskUpdated(ActivityTask),
    InputRequested(InputRequest),
    InputResolved(String),
    InputReplyFailed {
        id: String,
        reason: String,
    },
    TaskStarted {
        id: String,
    },
    TaskCompleted {
        id: String,
    },
    SessionLoaded {
        snapshot: SessionSnapshot,
        switching: bool,
        model: Option<String>,
        permissions: Permissions,
    },
    TurnStarted {
        id: Option<String>,
    },
    TurnSubmitted {
        id: Option<String>,
        status: String,
    },
    TurnCompleted {
        id: Option<String>,
        status: String,
        error: Option<String>,
    },
    EntryUpdated(Entry),
    EntryDelta {
        id: String,
        kind: Kind,
        text: String,
    },
    ActivityUpdated(Agent),
    ActivityHistory {
        id: String,
        status: String,
        entries: Vec<Entry>,
    },
    ApprovalRequested(Approval),
    ApprovalResolved(String),
    ApprovalReplyFailed {
        id: String,
        reason: String,
    },
    PermissionsUpdated {
        permissions: Permissions,
        defaults_only: bool,
    },
    PermissionApplied,
    PermissionFailed(String),
    SessionsLoaded {
        sessions: Vec<SessionSummary>,
        cursor: Option<String>,
        append: bool,
    },
    SessionsFailed(String),
    SkillsLoaded {
        skills: Vec<Skill>,
        errors: Vec<String>,
    },
    ModelsLoaded(Vec<String>),
    ModelCatalogLoaded(Vec<ModelCatalogEntry>),
    ModelCatalogLoading,
    ModelCatalogSources(Vec<ModelCatalogSource>),
    UsageUpdated(String),
    DiffUpdated(String),
    StatusUpdated {
        status: String,
        busy: bool,
    },
    OperationStarted {
        id: u64,
        operation: Operation,
    },
    OperationFinished(u64),
    SubmissionFailed(String),
    InterruptFailed(String),
    TrustRequired(String),
    TrustResolved(bool),
    TrustFailed(String),
    Disconnected(String),
    Notice(String),
    Error(String),
}

pub trait AgentBackend {
    fn command(&mut self, command: AgentCommand) -> anyhow::Result<()>;
    fn poll(&mut self) -> anyhow::Result<Option<AgentEvent>>;
    /// Rebuild direct connections; ongoing turns keep their current provider.
    fn reload_providers(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("이 실행 엔진에서는 공급자 갱신을 지원하지 않습니다. Loom 엔진으로 시작하세요.")
    }
    fn update_model_policy(&mut self, _call_seconds: u64, _attempts: usize) {}
    fn update_execution_budget(&mut self, _steps: usize, _seconds: u64) {}
    /// Stop owned work and detach; saved history is not deleted or archived.
    fn shutdown(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
