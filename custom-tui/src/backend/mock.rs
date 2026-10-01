//! An independent Core fixture. It uses the product contract, never Codex JSON.
use crate::agent::*;
use anyhow::{Result, bail};
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

struct Stream {
    turn: String,
    entry: String,
    text: String,
    offset: usize,
    tick: Instant,
}

pub struct MockCore {
    events: VecDeque<AgentEvent>,
    session: SessionSnapshot,
    stream: Option<Stream>,
    serial: u64,
    approval: bool,
    readonly: bool,
    input: Option<InputRequest>,
}

fn entry(id: &str, kind: Kind, title: &str, body: impl Into<String>) -> Entry {
    Entry {
        id: id.into(),
        kind,
        title: title.into(),
        body: body.into(),
        status: "completed".into(),
        expanded: false,
    }
}

impl MockCore {
    pub fn new(readonly: bool) -> Self {
        let output = (1..=60)
            .map(|n| format!("예시 출력 {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let session = SessionSnapshot {
            id: "demo-main".into(),
            title: None,
            status: Some("데모 · 실제 실행 없음".into()),
            turns: Some(vec![TurnSnapshot {
                id: Some("demo-first-turn".into()),
                status: "completed".into(),
                entries: vec![
                    entry(
                        "demo-user",
                        Kind::User,
                        "› 나",
                        "새 TUI 엔진을 만들고 조작 흐름을 검토해줘.",
                    ),
                    entry(
                        "demo-response",
                        Kind::Assistant,
                        "● 에이전트",
                        "대화는 넓게 읽고, 도구·변경·활동은 필요할 때 펼치는 구성이야. 클릭과 Tab/Enter로 같은 조작을 수행할 수 있어.\n```rust\nlet message = \"안녕하세요\";\nprintln!(\"{message}\");\n```",
                    ),
                    entry(
                        "demo-tool",
                        Kind::Tool,
                        "TUI 구조 읽기 (예시)",
                        format!("위치: Loom\n{output}"),
                    ),
                    entry(
                        "demo-collab",
                        Kind::Tool,
                        "에이전트 작업",
                        "활동 탭에서 에이전트별 작업을 확인하세요.",
                    ),
                ],
                activities: vec![
                    Agent {
                        id: "demo-agent-a".into(),
                        name: String::new(),
                        status: "running".into(),
                        detail: "입력과 포커스 검토".into(),
                    },
                    Agent {
                        id: "demo-agent-b".into(),
                        name: String::new(),
                        status: "completed".into(),
                        detail: "화면 구성 검토 완료".into(),
                    },
                ],
            }]),
        };
        let mut core = Self {
            events: VecDeque::new(),
            session,
            stream: None,
            serial: 0,
            approval: !readonly,
            readonly,
            input: None,
        };
        core.loaded(false, "fixture");
        core.emit(AgentUpdate::ModelsLoaded(vec![
            "fixture".into(),
            "fixture-fast".into(),
        ]));
        core.emit(AgentUpdate::SkillsLoaded {
            skills: vec![Skill {
                name: "demo-review".into(),
                path: "/demo/SKILL.md".into(),
                description: "화면 검토 스킬 예시 · 실제 실행 없음".into(),
            }],
            errors: vec![],
        });
        core.scoped(AgentUpdate::DiffUpdated("--- a/docs/tui-plan.md\n+++ b/docs/tui-plan.md\n@@ -1,2 +1,3 @@\n-기존 TUI 수정\n+새 Rust 화면 엔진\n+키보드와 클릭 조작\n+에이전트별 별도 터미널".into()));
        if !readonly {
            core.scoped(AgentUpdate::ApprovalRequested(Approval {
                id: "demo-approval".into(),
                title: "파일 변경 승인".into(),
                summary: "대상: TUI 구조 읽기 (예시)\n요청 이유: 화면 예시 승인 요청".into(),
                detail: "화면 예시 승인 요청 · 실제 실행 없음".into(),
                choices: vec![
                    Choice {
                        label: "이번 요청 허용".into(),
                        result: "allow".into(),
                    },
                    Choice {
                        label: "거절".into(),
                        result: "deny".into(),
                    },
                    Choice {
                        label: "취소".into(),
                        result: "cancel".into(),
                    },
                ],
                answering: false,
                expanded: false,
            }));
        } else {
            core.emit(AgentUpdate::Notice(
                "데모 에이전트 열람 · 실제 런타임 연결 없음".into(),
            ));
        }
        core
    }
    fn emit(&mut self, update: AgentUpdate) {
        self.events.push_back(AgentEvent::global(update));
    }
    fn scoped(&mut self, update: AgentUpdate) {
        self.events
            .push_back(AgentEvent::scoped(self.session.id.clone(), update));
    }
    fn loaded(&mut self, switching: bool, model: &str) {
        self.emit(AgentUpdate::SessionLoaded {
            snapshot: self.session.clone(),
            switching,
            model: Some(model.into()),
            permissions: Permissions::default(),
        });
    }
    fn add_entry(&mut self, new: Entry) {
        if let Some(turn) = self.session.turns.as_mut().and_then(|t| t.last_mut()) {
            turn.entries.push(new.clone());
        }
        self.scoped(AgentUpdate::EntryUpdated(new));
    }
    fn complete(&mut self, stream: Stream, status: &str) {
        if let Some(turn) = self.session.turns.as_mut().and_then(|t| t.last_mut()) {
            turn.status = status.into();
            if let Some(entry) = turn.entries.iter_mut().find(|e| e.id == stream.entry) {
                entry.body = stream.text[..stream.offset].into();
                entry.status = status.into();
                let updated = entry.clone();
                self.scoped(AgentUpdate::EntryUpdated(updated));
            }
        }
        self.scoped(AgentUpdate::TurnCompleted {
            id: Some(stream.turn),
            status: status.into(),
            error: None,
        });
    }
}

impl AgentBackend for MockCore {
    fn command(&mut self, command: AgentCommand) -> Result<()> {
        if self.readonly
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
                if let Some(request) = &self.input
                    && request.id == request_id
                {
                    match request.validate_answers(&answers) {
                        Ok(_) => {
                            self.input = None;
                            self.scoped(AgentUpdate::InputResolved(request_id));
                            self.emit(AgentUpdate::Notice(
                                "데모 답변 수신 완료 · 실제 실행 없음".into(),
                            ));
                        }
                        Err(error) => self.scoped(AgentUpdate::InputReplyFailed {
                            id: request_id,
                            reason: error.to_string(),
                        }),
                    }
                }
            }
            AgentCommand::Explore(_) => bail!("Code exploration is owned by the native Core"),
            AgentCommand::Submit { text, .. } => {
                if self.stream.is_some() || self.approval || self.input.is_some() {
                    self.scoped(AgentUpdate::SubmissionFailed(
                        "현재 실행 또는 승인 처리가 끝나지 않았습니다.".into(),
                    ));
                    return Ok(());
                }
                if text.trim() == "질문 데모" {
                    let request = InputRequest {
                        id: "demo-question".into(),
                        blocking: true,
                        questions: vec![
                            Question {
                                id: "scope".into(),
                                header: "구현 범위".into(),
                                prompt: "어느 기능부터 구현할까요?".into(),
                                options: vec![
                                    QuestionOption {
                                        label: "질문 UI".into(),
                                        description: "선택지와 직접 입력을 먼저 완성합니다.".into(),
                                    },
                                    QuestionOption {
                                        label: "활동 패널".into(),
                                        description: "진행 상황과 대기 이유를 표시합니다.".into(),
                                    },
                                ],
                                allow_custom: true,
                                secret: false,
                            },
                            Question {
                                id: "details".into(),
                                header: "추가 요청".into(),
                                prompt: "구현 시 고려할 사항을 입력해주세요.".into(),
                                options: vec![],
                                allow_custom: true,
                                secret: false,
                            },
                        ],
                    };
                    self.input = Some(request.clone());
                    self.scoped(AgentUpdate::InputRequested(request));
                    self.scoped(AgentUpdate::TurnSubmitted {
                        id: None,
                        status: "idle".into(),
                    });
                    return Ok(());
                }
                self.serial += 1;
                let turn = format!("demo-turn-{}", self.serial);
                let id = format!("demo-stream-{}", self.serial);
                self.session
                    .turns
                    .get_or_insert_with(Vec::new)
                    .push(TurnSnapshot {
                        id: Some(turn.clone()),
                        status: "inProgress".into(),
                        entries: vec![],
                        activities: vec![],
                    });
                self.scoped(AgentUpdate::TurnStarted {
                    id: Some(turn.clone()),
                });
                self.add_entry(entry(
                    &format!("demo-user-{}", self.serial),
                    Kind::User,
                    "› 나",
                    text,
                ));
                let mut response = entry(&id, Kind::Assistant, "● 에이전트", "");
                response.status = "inProgress".into();
                self.add_entry(response);
                self.scoped(AgentUpdate::TurnSubmitted {
                    id: Some(turn.clone()),
                    status: "inProgress".into(),
                });
                self.scoped(AgentUpdate::StatusUpdated {
                    status: "데모 스트리밍".into(),
                    busy: true,
                });
                self.stream = Some(Stream {
                    turn,
                    entry: id,
                    text:
                        "예시 응답이야. 대기열에 넣은 메시지는 이전 답변이 끝나면 순서대로 처리돼."
                            .into(),
                    offset: 0,
                    tick: Instant::now(),
                });
            }
            AgentCommand::Interrupt => {
                if let Some(input) = self.input.take() {
                    self.scoped(AgentUpdate::InputResolved(input.id));
                }
                if let Some(stream) = self.stream.take() {
                    self.complete(stream, "interrupted");
                }
            }
            AgentCommand::Reply { request_id, choice } => {
                if self.approval
                    && request_id == "demo-approval"
                    && matches!(choice.as_str(), "allow" | "deny" | "cancel")
                {
                    self.approval = false;
                    self.scoped(AgentUpdate::ApprovalResolved(request_id));
                    self.emit(AgentUpdate::Notice(
                        "데모 승인 선택 완료 · 실제 실행 없음".into(),
                    ));
                } else {
                    self.scoped(AgentUpdate::ApprovalReplyFailed {
                        id: request_id,
                        reason: "유효하지 않은 승인 선택지".into(),
                    });
                }
            }
            AgentCommand::RefreshSessions | AgentCommand::MoreSessions(_) => {
                self.emit(AgentUpdate::SessionsLoaded {
                    sessions: vec![
                        SessionSummary {
                            id: "demo-main".into(),
                            title: "TUI 화면 검토".into(),
                            status: "idle".into(),
                            updated_at: None,
                        },
                        SessionSummary {
                            id: "demo-saved".into(),
                            title: "예시 · 이전 화면 검토".into(),
                            status: "idle".into(),
                            updated_at: None,
                        },
                    ],
                    cursor: None,
                    append: false,
                })
            }
            AgentCommand::OpenSession { id, model } => {
                if self.stream.is_some() || self.approval || self.input.is_some() {
                    bail!("실행과 승인 처리를 끝낸 뒤 대화를 전환하세요.");
                }
                let title = match id.as_deref() {
                    Some("demo-main") => "TUI 화면 검토",
                    Some(_) => "예시 · 이전 화면 검토",
                    None => "새 대화",
                };
                self.session = SessionSnapshot {
                    id: id.unwrap_or_else(|| "demo-new".into()),
                    title: Some(title.into()),
                    status: Some("데모 · 실제 실행 없음".into()),
                    turns: Some(vec![]),
                };
                self.loaded(true, &model);
                self.emit(AgentUpdate::Notice(
                    "새 데모 대화 · 실제 세션 전환 없음".into(),
                ));
            }
            AgentCommand::Fork { turn_id, model } => {
                if self.stream.is_some() || self.approval || self.input.is_some() {
                    bail!("완료된 대화만 분기할 수 있습니다.");
                }
                let turns = self.session.turns.get_or_insert_with(Vec::new);
                let index = turns
                    .iter()
                    .position(|t| t.id.as_deref() == Some(&turn_id))
                    .ok_or_else(|| anyhow::anyhow!("분기할 대화가 없습니다."))?;
                turns.truncate(index + 1);
                self.session.id = format!("demo-branch-{turn_id}");
                self.session.title = Some("분기 · TUI 화면 검토".into());
                self.loaded(true, &model);
                self.emit(AgentUpdate::Notice(
                    "새 데모 분기 · 원본 대화는 유지됨".into(),
                ));
            }
            AgentCommand::ReadActivity(id) => self.scoped(AgentUpdate::ActivityHistory {
                id,
                status: "completed".into(),
                entries: self
                    .session
                    .turns
                    .as_ref()
                    .into_iter()
                    .flatten()
                    .flat_map(|t| &t.entries)
                    .filter(|e| e.kind == Kind::Assistant)
                    .cloned()
                    .collect(),
            }),
            AgentCommand::UpdatePermission { .. } => self.scoped(AgentUpdate::PermissionApplied),
            AgentCommand::RefreshModels => self.emit(AgentUpdate::ModelsLoaded(vec![
                "fixture".into(),
                "fixture-fast".into(),
            ])),
            AgentCommand::RefreshSkills => {}
            AgentCommand::TrustWorkspace(trusted) => self.emit(AgentUpdate::TrustResolved(trusted)),
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<AgentEvent>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        if let Some(mut stream) = self.stream.take() {
            if stream.tick.elapsed() >= Duration::from_millis(35) {
                if let Some(part) = stream.text[stream.offset..].graphemes(true).next() {
                    let text = part.to_owned();
                    stream.offset += text.len();
                    stream.tick = Instant::now();
                    self.scoped(AgentUpdate::EntryDelta {
                        id: stream.entry.clone(),
                        kind: Kind::Assistant,
                        text,
                    });
                    self.stream = Some(stream);
                } else {
                    self.complete(stream, "completed");
                }
            } else {
                self.stream = Some(stream);
            }
        }
        Ok(self.events.pop_front())
    }
}
