use custom_tui::{
    agent::{AgentBackend, AgentCommand, AgentEvent, AgentUpdate, Operation},
    app::App,
    backend::codex::{CodexBackend, Options},
    core::{backend::CoreBackend, repository::RepositoryExplorer},
};
use serde_json::{Value, json};
use std::{
    fs,
    net::TcpListener,
    sync::{Arc, Barrier, mpsc},
    thread,
    time::{Duration, Instant},
};
use tungstenite::{Message, accept};

enum Control {
    Send(Value),
    Stop,
}
struct Fixture {
    backend: CodexBackend,
    app: App,
    wire: mpsc::Receiver<Value>,
    control: mpsc::Sender<Control>,
    server: Option<thread::JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new(readonly: bool) -> Self {
        Self::with_restore(readonly, false)
    }
    fn with_restore(readonly: bool, occupied: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (wire_tx, wire) = mpsc::channel();
        let (control, controls) = mpsc::channel();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(10)))
                .unwrap();
            let mut socket = accept(stream).unwrap();
            loop {
                while let Ok(control) = controls.try_recv() {
                    match control {
                        Control::Send(value) => {
                            socket.send(Message::Text(value.to_string())).unwrap();
                        }
                        Control::Stop => return,
                    }
                }
                let message = match socket.read() {
                    Ok(Message::Text(text)) => serde_json::from_str::<Value>(&text).unwrap(),
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue;
                    }
                    Err(_) | Ok(Message::Close(_)) => return,
                    _ => continue,
                };
                wire_tx.send(message.clone()).unwrap();
                let Some(method) = message["method"].as_str() else {
                    continue;
                };
                if message["id"].is_null() {
                    continue;
                }
                let result = match method {
                    "initialize" => json!({}),
                    "config/read" => {
                        json!({"config":{"sandbox_mode":"read-only","approval_policy":"on-request"}})
                    }
                    "thread/start" => {
                        json!({"thread":{"id":"main","status":{"type":"idle"}},"model":"fixture"})
                    }
                    "thread/resume" => {
                        if occupied {
                            socket.send(Message::Text(json!({"id":message["id"],"error":{"message":"thread-store conflict: thread occupied already has an active writer"}}).to_string())).unwrap();
                            continue;
                        }
                        json!({"thread":{"id":message["params"]["threadId"],"status":{"type":"idle"}},"model":"fixture"})
                    }
                    "thread/fork" => {
                        json!({"thread":{"id":"branch","status":{"type":"idle"}},"model":"fixture"})
                    }
                    "thread/read" => {
                        json!({"thread":{"id":message["params"]["threadId"],"status":{"type":"idle"},"turns":[]}})
                    }
                    "skills/list" | "thread/list" => json!({"data":[]}),
                    "model/list" => {
                        json!({"data":[{"model":"fixture","supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Fast"},{"reasoningEffort":"high","description":"Deep"}],"defaultReasoningEffort":"low"}]})
                    }
                    "account/rateLimits/read" => {
                        json!({"rateLimits":{"limitId":"fixture","normalModelSlug":"fixture","primary":{"usedPercent":20,"windowDurationMins":300,"resetsAt":1990000000}}})
                    }
                    "turn/start" => {
                        // Completion is allowed to arrive before the start reply.
                        socket.send(Message::Text(json!({"method":"turn/completed","params":{"threadId":"main","turn":{"id":"short-turn","status":"completed"}}}).to_string())).unwrap();
                        json!({"turn":{"id":"short-turn","status":"inProgress"}})
                    }
                    "thread/settings/update"
                    | "turn/interrupt"
                    | "thread/unsubscribe"
                    | "config/value/write" => json!({}),
                    _ => panic!("Unexpected RPC: {method}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":message["id"],"result":result}).to_string(),
                    ))
                    .unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let backend = CodexBackend::connect(
            &endpoint,
            Options {
                cwd: directory.path().into(),
                model: None,
                initial_session: occupied.then(|| "occupied".into()),
                readonly,
                auto_restore: occupied,
            },
        )
        .unwrap();
        let mut fixture = Self {
            backend,
            app: App::new(readonly),
            wire,
            control,
            server: Some(server),
            _directory: directory,
        };
        if occupied {
            custom_tui::runtime::remember_thread(fixture._directory.path(), "occupied").unwrap();
            fixture.app.editor.insert("preserved instruction");
            fixture.app.activate(custom_tui::engine::Action::Send);
        }
        if !readonly {
            fixture
                .backend
                .command(AgentCommand::TrustWorkspace(false))
                .unwrap();
        }
        fixture.until(|app| app.thread.is_some() && app.pending.is_empty());
        fixture
    }
    fn until(&mut self, ready: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            while let Some(event) = self.backend.poll().unwrap() {
                self.app.apply_event(event);
            }
            if ready(&self.app) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Backend did not reach expected state: {}",
                self.app.status
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn send(&self, value: Value) {
        self.control.send(Control::Send(value)).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Stop);
        if let Some(server) = self.server.take() {
            server.join().unwrap();
        }
    }
}

#[test]
fn codex_backend_preserves_permissions_and_authoritative_completion() {
    let mut fixture = Fixture::new(false);
    let messages: Vec<_> = fixture.wire.try_iter().collect();
    let start = messages
        .iter()
        .find(|m| m["method"] == "thread/start")
        .unwrap();
    assert_eq!(start["params"]["sandbox"], "workspace-write");
    assert_eq!(start["params"]["approvalPolicy"], "on-request");
    fixture
        .backend
        .command(AgentCommand::Submit {
            text: "short request".into(),
            model: "fixture".into(),
            effort: None,
            skills: vec![],
        })
        .unwrap();
    fixture.until(|app| {
        app.finished_turns.contains("short-turn")
            && !app.pending.values().any(|op| *op == Operation::Submit)
    });
    assert!(!fixture.app.busy);
    assert!(fixture.app.turn.is_none());
    fixture
        .backend
        .command(AgentCommand::Fork {
            turn_id: "short-turn".into(),
            model: "fixture".into(),
        })
        .unwrap();
    fixture.until(|app| app.thread.as_deref() == Some("branch"));
    assert!(fixture.app.notice.contains("새 분기 시작"));
}

#[test]
fn approval_tokens_are_backend_owned_and_duplicate_replies_are_not_sent() {
    let mut fixture = Fixture::new(false);
    fixture.send(json!({"id":"gate","method":"item/commandExecution/requestApproval","params":{"threadId":"main","command":"cargo test","availableDecisions":["decline"]}}));
    fixture.until(|app| app.approvals.len() == 1);
    let approval = fixture.app.approvals[0].clone();
    fixture
        .backend
        .command(AgentCommand::Reply {
            request_id: approval.id.clone(),
            choice: "invalid".into(),
        })
        .unwrap();
    fixture.until(|app| app.notice.contains("응답 실패"));
    let command = AgentCommand::Reply {
        request_id: approval.id.clone(),
        choice: approval.choices[0].result.clone(),
    };
    fixture.backend.command(command.clone()).unwrap();
    fixture.backend.command(command).unwrap();
    // A subsequent request gives an ordering barrier for the wire reader.
    fixture
        .backend
        .command(AgentCommand::RefreshSessions)
        .unwrap();
    fixture.until(|app| app.sessions_loaded && app.pending.is_empty());
    let replies: Vec<_> = fixture
        .wire
        .try_iter()
        .filter(|m| m["id"] == "gate")
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["result"], json!({"decision":"decline"}));
    assert_eq!(
        fixture.app.approvals.len(),
        1,
        "Only runtime confirmation resolves approval"
    );
    fixture.send(json!({"method":"serverRequest/resolved","params":{"requestId":"gate"}}));
    fixture.until(|app| app.approvals.is_empty());
}

#[test]
fn readonly_backend_rejects_mutation_and_events_do_not_cross_sessions() {
    let mut fixture = Fixture::new(true);
    assert!(
        fixture
            .backend
            .command(AgentCommand::Submit {
                text: "no".into(),
                model: String::new(),
                effort: None,
                skills: vec![]
            })
            .is_err()
    );
    fixture.send(json!({"method":"item/agentMessage/delta","params":{"threadId":"other","itemId":"wrong","delta":"wrong scope"}}));
    fixture.send(json!({"method":"item/agentMessage/delta","params":{"threadId":"main","itemId":"right","delta":"visible"}}));
    fixture.until(|app| app.entries.iter().any(|e| e.id == "right"));
    assert!(!fixture.app.entries.iter().any(|e| e.id == "wrong"));
}

#[test]
fn session_projection_keeps_previous_page_on_invalid_refresh() {
    use custom_tui::backend::codec;
    let mut app = App::new(false);
    app.apply_event(AgentEvent::global(codec::session_page(
        &json!({"data":[{"id":"one","preview":"Original"}],"nextCursor":"next"}),
        false,
    )));
    app.apply_event(AgentEvent::global(codec::session_page(
        &json!({"data":[{"id":"one","preview":"Updated"},{"id":"two"}]}),
        true,
    )));
    assert_eq!(app.sessions.len(), 2);
    assert_eq!(app.sessions[0].title, "Updated");
    app.apply_event(AgentEvent::global(codec::session_page(
        &json!({"broken":[]}),
        false,
    )));
    assert_eq!(app.sessions.len(), 2);
    assert!(app.notice.contains("이전 목록"));
}

#[test]
fn four_workers_share_one_index_and_one_search() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("auth.rs"),
        "pub fn refresh_session() {}\n",
    )
    .unwrap();
    let explorer = Arc::new(RepositoryExplorer::new(directory.path()).unwrap());
    let barrier = Arc::new(Barrier::new(4));
    let jobs: Vec<_> = (0..4)
        .map(|_| {
            let explorer = Arc::clone(&explorer);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                explorer.explore("refresh_session", 1024).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
    assert_eq!(explorer.counts(), (1, 1));
    assert!(
        results
            .iter()
            .all(|result| Arc::ptr_eq(result, &results[0]))
    );
    assert_eq!(results[0].evidence[0].line, 1);
    assert!(results[0].evidence[0].reference.starts_with("repo://"));
}

#[test]
fn repository_changes_invalidate_queries_but_running_workers_keep_their_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.rs");
    fs::write(&path, "fn refresh_session() { old(); }\n").unwrap();
    let explorer = RepositoryExplorer::new(directory.path()).unwrap();
    let old_snapshot = explorer.snapshot().unwrap();
    let old = explorer
        .explore_snapshot(&old_snapshot, "refresh_session", 1024)
        .unwrap();
    let stamp = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, "fn refresh_session() { new(); }\n").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(stamp)
        .unwrap();
    let current = explorer.explore("refresh_session", 1024).unwrap();
    assert_ne!(old.revision, current.revision);
    assert!(old.evidence[0].excerpt.contains("old()"));
    assert!(current.evidence[0].excerpt.contains("new()"));
    assert!(Arc::ptr_eq(
        &old,
        &explorer
            .explore_snapshot(&old_snapshot, "refresh_session", 1024)
            .unwrap()
    ));
    fs::remove_file(path).unwrap();
    assert!(
        explorer
            .explore("refresh_session", 1024)
            .unwrap()
            .evidence
            .is_empty()
    );
}

#[test]
fn local_exploration_runs_through_core_without_a_remote_command() {
    struct NoRemote;
    impl AgentBackend for NoRemote {
        fn command(&mut self, _: AgentCommand) -> anyhow::Result<()> {
            panic!("LOCAL must never call remote");
        }
        fn poll(&mut self) -> anyhow::Result<Option<AgentEvent>> {
            Ok(None)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("auth.rs"),
        "fn refresh_session() {}\n",
    )
    .unwrap();
    let mut core = CoreBackend::new(Box::new(NoRemote), directory.path(), true).unwrap();
    core.command(AgentCommand::Explore("refresh_session".into()))
        .unwrap();
    let mut app = App::new(false);
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut done = false;
    while !done {
        if let Some(event) = core.poll().unwrap() {
            done = matches!(event.update, AgentUpdate::TaskCompleted { .. });
            app.apply_event(event);
        } else {
            thread::sleep(Duration::from_millis(2));
        }
        assert!(Instant::now() < deadline);
    }
    assert!(app.entries[0].body.contains("refresh_session"));
    assert!(app.entries[0].body.contains("API 호출 0"));
    assert_eq!(core.repository().counts(), (1, 1));
    assert_eq!(app.tasks.len(), 1);
    assert_eq!(
        app.tasks[0].status,
        custom_tui::agent::TaskStatus::Completed
    );
    assert_eq!(
        app.tasks[0].worker,
        Some(custom_tui::agent::WorkerKind::Local)
    );
    assert!(app.tasks[0].elapsed_ms.is_some());
    assert!(app.tasks[0].outputs[0].starts_with("repo://"));
}

#[test]
fn silent_work_animates_and_stops_after_completion_or_approval() {
    use custom_tui::agent::{Approval, Choice};
    let mut app = App::new(false);
    app.busy = true;
    app.status = "실행 중".into();
    let first = app.render(80, 24).cells[17].text.clone();
    let now = Instant::now() + Duration::from_millis(120);
    assert!(app.tick(now));
    let next = app.render(80, 24).cells[17].text.clone();
    assert_ne!(first, next, "Animation must advance without model output");
    app.apply_event(AgentEvent::global(AgentUpdate::ApprovalRequested(
        Approval {
            id: "request".into(),
            title: "Approval".into(),
            summary: String::new(),
            detail: String::new(),
            choices: vec![Choice {
                label: "Deny".into(),
                result: "deny".into(),
            }],
            answering: false,
            expanded: false,
        },
    )));
    app.tick(now + Duration::from_millis(120));
    let waiting = app.render(80, 24).cells[17].text.clone();
    assert!(!app.tick(now + Duration::from_millis(240)));
    assert_eq!(waiting, app.render(80, 24).cells[17].text);
    app.apply_event(AgentEvent::global(AgentUpdate::ApprovalResolved(
        "request".into(),
    )));
    app.apply_event(AgentEvent::global(AgentUpdate::TurnCompleted {
        id: None,
        status: "completed".into(),
        error: None,
    }));
    app.tick(now + Duration::from_millis(360));
    assert_eq!(app.render(80, 24).cells[17].text, "✓");
    assert!(!app.tick(now + Duration::from_millis(480)));
}

#[test]
fn evidence_is_bounded_and_snapshots_cannot_cross_repository_roots() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    fs::write(
        first.path().join("many.rs"),
        "fn shared_symbol() { /* 한글 증거 */ }\n".repeat(1000),
    )
    .unwrap();
    let explorer = RepositoryExplorer::new(first.path()).unwrap();
    let snapshot = explorer.snapshot().unwrap();
    let result = explorer
        .explore_snapshot(&snapshot, "shared_symbol", 1024)
        .unwrap();
    assert!(result.truncated);
    assert!(result.compact().chars().count() < 1400);
    assert!(result.evidence.len() < 24);
    assert!(
        RepositoryExplorer::new(second.path())
            .unwrap()
            .explore_snapshot(&snapshot, "shared_symbol", 1024)
            .is_err()
    );
}

#[test]
fn questions_round_trip_validates_all_answers_and_awaits_server_resolution() {
    use custom_tui::agent::{AnswerValue, QuestionAnswer};
    let mut fixture = Fixture::new(false);
    fixture.send(json!({"id":101,"method":"item/tool/requestUserInput","params":{
        "threadId":"main","turnId":"turn","itemId":"input","isBlocking":true,
        "questions":[
            {"id":"scope","header":"Scope","question":"Which scope?","isOther":true,"options":[{"label":"UI","description":"First milestone"}]},
            {"id":"detail","header":"Detail","question":"Any details?","isSecret":true}
        ]}}));
    fixture.until(|app| app.input_forms.len() == 1);
    let invalid = AgentCommand::Answer {
        request_id: "101".into(),
        answers: vec![],
    };
    fixture.backend.command(invalid).unwrap();
    fixture.until(|app| !app.input_forms[0].answering);
    let command = AgentCommand::Answer {
        request_id: "101".into(),
        answers: vec![
            QuestionAnswer {
                question_id: "scope".into(),
                value: AnswerValue::Option(0),
            },
            QuestionAnswer {
                question_id: "detail".into(),
                value: AnswerValue::Text("custom detail".into()),
            },
        ],
    };
    fixture.backend.command(command.clone()).unwrap();
    fixture.backend.command(command).unwrap();
    assert!(
        fixture
            .backend
            .command(AgentCommand::OpenSession {
                id: None,
                model: String::new()
            })
            .is_err()
    );
    fixture
        .backend
        .command(AgentCommand::RefreshSessions)
        .unwrap();
    fixture.until(|app| app.sessions_loaded && app.pending.is_empty());
    let replies: Vec<_> = fixture.wire.try_iter().filter(|m| m["id"] == 101).collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0]["result"],
        json!({"answers":{"scope":{"answers":["UI"]},"detail":{"answers":["custom detail"]}}})
    );
    assert_eq!(
        fixture.app.input_forms.len(),
        1,
        "Only runtime resolution removes a form"
    );
    fixture.send(json!({"method":"serverRequest/resolved","params":{"requestId":101}}));
    fixture.until(|app| app.input_forms.is_empty());
}

#[test]
fn malformed_and_foreign_question_requests_are_rejected() {
    let mut fixture = Fixture::new(false);
    for (id, thread, questions) in [
        (201, "other", json!([{"id":"x","question":"q"}])),
        (202, "main", json!([])),
        (
            203,
            "main",
            json!([{"id":"x","question":"q"},{"id":"x","question":"q"}]),
        ),
        (
            204,
            "main",
            json!([{"id":"x","question":"q","options":"bad"}]),
        ),
    ] {
        fixture.send(json!({"id":id,"method":"item/tool/requestUserInput","params":{"threadId":thread,"questions":questions}}));
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut rejected = std::collections::HashSet::new();
    while rejected.len() < 4 {
        while let Some(event) = fixture.backend.poll().unwrap() {
            fixture.app.apply_event(event);
        }
        for message in fixture.wire.try_iter() {
            if let Some(id) = message["id"].as_i64()
                && (201..=204).contains(&id)
            {
                assert!(!message["error"].is_null());
                rejected.insert(id);
            }
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    assert!(fixture.app.input_forms.is_empty());
}

#[test]
fn selected_effort_is_validated_and_forwarded_to_the_runtime() {
    let mut fixture = Fixture::new(false);
    fixture.until(|app| !app.model_profiles.is_empty());
    fixture
        .backend
        .command(AgentCommand::Submit {
            text: "request".into(),
            model: "fixture".into(),
            effort: Some("unsupported".into()),
            skills: vec![],
        })
        .unwrap();
    fixture.until(|app| app.notice.contains("reasoning effort"));
    fixture
        .backend
        .command(AgentCommand::Submit {
            text: "request".into(),
            model: "fixture".into(),
            effort: Some("high".into()),
            skills: vec![],
        })
        .unwrap();
    fixture.until(|app| app.finished_turns.contains("short-turn") && app.pending.is_empty());
    let turns: Vec<_> = fixture
        .wire
        .try_iter()
        .filter(|m| m["method"] == "turn/start")
        .collect();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0]["params"]["effort"], "high");
}

#[test]
fn quota_failures_pause_remote_work_preserve_queue_and_require_manual_resume() {
    use custom_tui::{agent::LimitKind, app::QueuedMessage, engine::Action};
    let mut fixture = Fixture::new(false);
    fixture.send(json!({"method":"thread/tokenUsage/updated","params":{"threadId":"main","tokenUsage":{
        "total":{"totalTokens":12400,"inputTokens":10000,"cachedInputTokens":7000,"outputTokens":2400,"reasoningOutputTokens":400},"last":{"totalTokens":3200},"modelContextWindow":128000}}}));
    fixture.until(|app| app.tokens.total == Some(12400));
    fixture.app.queued.push_back(QueuedMessage {
        text: "preserve this".into(),
        model: "fixture".into(),
        effort: Some("high".into()),
        skills: vec![],
    });
    fixture.send(json!({"method":"error","params":{"threadId":"main","turnId":"limited","willRetry":false,"error":{"message":"Quota exhausted","codexErrorInfo":"usageLimitExceeded"}}}));
    fixture.until(|app| app.execution_limit == Some(LimitKind::UsageExhausted));
    assert!(fixture.app.next_queued().is_none());
    fixture.app.activate(Action::ResumeQueue);
    assert!(fixture.app.queue_paused);
    fixture
        .backend
        .command(AgentCommand::Submit {
            text: "must not send".into(),
            model: "fixture".into(),
            effort: None,
            skills: vec![],
        })
        .unwrap();
    fixture.until(|app| app.notice.contains("제한"));
    fixture.backend.command(AgentCommand::RefreshUsage).unwrap();
    fixture.until(|app| app.execution_limit.is_none() && app.pending.is_empty());
    assert!(fixture.app.queue_paused && fixture.app.next_queued().is_none());
    assert_eq!(fixture.app.queued[0].text, "preserve this");
    assert!(!fixture.wire.try_iter().any(|m| m["method"] == "turn/start"));
}

#[test]
fn runtime_retry_is_distinct_from_hard_quota_exhaustion() {
    let mut fixture = Fixture::new(false);
    fixture.send(json!({"method":"error","params":{"threadId":"main","turnId":"retry","willRetry":true,"error":{"message":"Retry soon","codexErrorInfo":"rateLimitExceeded"}}}));
    fixture.until(|app| app.notice.contains("재시도"));
    assert!(fixture.app.execution_limit.is_none());
    assert!(!fixture.app.queue_paused);
}

#[test]
fn occupied_recent_session_recovers_without_losing_unsent_work_or_history() {
    let mut fixture = Fixture::with_restore(false, true);
    assert_eq!(fixture.app.thread.as_deref(), Some("main"));
    assert_eq!(
        custom_tui::runtime::last_thread(fixture._directory.path()).unwrap(),
        "occupied"
    );
    let messages: Vec<_> = fixture.wire.try_iter().collect();
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["method"] == "thread/resume")
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["method"] == "thread/start")
            .count(),
        1
    );
    assert!(!messages.iter().any(|m| m["method"] == "turn/start"));
    assert_eq!(
        fixture.app.next_queued(),
        Some(custom_tui::app::Intent::Submit(
            "preserved instruction".into()
        ))
    );
}

#[test]
fn shutdown_interrupts_owned_turn_then_detaches_without_archiving_history() {
    let mut fixture = Fixture::new(false);
    fixture.control.send(Control::Send(json!({"method":"turn/started","params":{"threadId":"main","turn":{"id":"running","status":"inProgress"}}}))).unwrap();
    fixture.until(|app| app.turn.as_deref() == Some("running"));
    let _ = fixture.wire.try_iter().collect::<Vec<_>>();
    fixture.backend.shutdown().unwrap();
    let messages: Vec<_> = fixture.wire.try_iter().collect();
    let interruption = messages
        .iter()
        .position(|m| m["method"] == "turn/interrupt")
        .unwrap();
    let detach = messages
        .iter()
        .position(|m| m["method"] == "thread/unsubscribe")
        .unwrap();
    assert!(interruption < detach);
    assert!(!messages.iter().any(|m| m["method"] == "thread/archive"));
    assert!(
        fixture
            .backend
            .command(AgentCommand::RefreshModels)
            .is_err()
    );
}
#[test]
fn closing_readonly_view_does_not_interrupt_main_work() {
    let mut fixture = Fixture::new(true);
    fixture.control.send(Control::Send(json!({"method":"turn/started","params":{"threadId":"main","turn":{"id":"running","status":"inProgress"}}}))).unwrap();
    fixture.until(|app| app.turn.as_deref() == Some("running"));
    let _ = fixture.wire.try_iter().collect::<Vec<_>>();
    fixture.backend.shutdown().unwrap();
    let messages: Vec<_> = fixture.wire.try_iter().collect();
    assert!(!messages.iter().any(|m| m["method"] == "turn/interrupt"));
    assert!(messages.iter().any(|m| m["method"] == "thread/unsubscribe"));
}
