//! The UI/Core boundary. No provider protocol or transport payloads belong here.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
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
    Explore(String),
    Answer {
        request_id: String,
        answers: Vec<QuestionAnswer>,
    },
    Submit {
        text: String,
        model: String,
        skills: Vec<Skill>,
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
    RefreshSessions,
    MoreSessions(String),
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
}
