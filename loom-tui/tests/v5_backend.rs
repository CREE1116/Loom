use loom_tui::{
    agent::{AgentBackend, AgentUpdate, Kind},
    backend::{
        version_core::{Settings, Version},
        versions::VersionBackend,
    },
};
use loom_core::{
    effects::{ExecutionPolicy, SandboxMode},
    models::{Part, Response, ScriptedModel, Stop, Usage},
};
use loom_core::agent::{Core, Intent, Options};
use serde_json::json;
use std::{
    fs,
    process::Command,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
#[test]
fn v5_reconnect_restores_individual_tool_history_and_plan_help() {
    let d = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(d.path())
            .status()
            .unwrap()
            .success()
    );
    fs::write(d.path().join("a.txt"), "fixture").unwrap();
    let model = ScriptedModel::new([Response {
        parts: vec![
            Part::ToolCall {
                id: "a".into(),
                name: "change".into(),
                arguments: json!({"op":"create","target":"b.txt","content":"PRIVATE_DETAIL"}),
            },
            Part::ToolCall {
                id: "b".into(),
                name: "finish".into(),
                arguments: json!({"summary":"정리 완료"}),
            },
        ],
        usage: Usage::default(),
        stop: Stop::ToolUse,
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    }]);
    let mut core = Core::open(d.path()).unwrap();
    let task = core.execute(
        "파일 추가",
        Intent::Edit,
        Options {
            checks: vec!["git diff --check".into()],
            policy: ExecutionPolicy {
                sandbox: SandboxMode::FullAccess,
                network: false,
            },
            ..Default::default()
        },
        &model,
        Arc::new(AtomicBool::new(false)),
        &|_| {},
    )
    .unwrap();
    let durable_log = fs::read_to_string(
        task.records
            .with_file_name(format!("{}.events.jsonl", task.id)),
    )
    .unwrap();
    assert!(
        durable_log.contains("PRIVATE_DETAIL"),
        "raw tool evidence must remain durable outside the UI projection"
    );
    let mut backend = VersionBackend::with_model(
        Version::Loom,
        d.path(),
        Arc::new(ScriptedModel::new([])),
        Settings::default(),
    )
    .unwrap();
    let mut found = false;
    let mut help = false;
    while let Some(event) = backend.poll().unwrap() {
        match event.update {
            AgentUpdate::SessionLoaded { snapshot, .. } => {
                found = snapshot
                    .turns
                    .unwrap()
                    .iter()
                    .flat_map(|t| &t.entries)
                    .any(|e| {
                        e.kind == Kind::Tool
                            && e.id.contains("-tool-")
                            && e.title == "change · b.txt"
                            && !e.expanded
                            && !e.body.contains("PRIVATE_DETAIL")
                            && !e.body.contains("\"args\"")
                            && !e.body.contains("\"result\"")
                    });
            }
            AgentUpdate::Notice(text) => help |= text.contains("/plan"),
            _ => {}
        }
    }
    assert!(
        found,
        "reconnect must restore individual compact tool history without exposing raw tool evidence"
    );
    assert!(help, "V5 entry should expose user /plan command");
}

struct ParallelQuestionModel {
    paused: std::sync::atomic::AtomicBool,
    release: std::sync::atomic::AtomicBool,
    inquiry: std::sync::atomic::AtomicBool,
}
impl loom_core::models::ModelPort for ParallelQuestionModel {
    fn complete(
        &self,
        need: loom_core::models::Need,
        r: &loom_core::models::Request,
    ) -> Result<loom_core::models::Served, loom_core::models::ProviderError> {
        let have = |name: &str| {
            r.messages
                .iter()
                .flat_map(|m| &m.parts)
                .any(|p| matches!(p,Part::ToolCall{name:n,..} if n==name))
        };
        let question = r
            .system
            .as_ref()
            .is_some_and(|s| s.contains("read-only parallel inquiry"));
        let calls = if question {
            self.inquiry
                .store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(
                !r.tools
                    .iter()
                    .any(|t| matches!(t.name.as_str(), "change" | "run" | "agent" | "ask"))
            );
            if !have("open") {
                vec![("open", json!({"target":"a.txt"}))]
            } else {
                assert!(r.messages.iter().flat_map(|m|&m.parts).any(|p|matches!(p,Part::ToolResult{output,..} if output.contains("private-current-world"))));
                vec![("finish", json!({"summary":"본 작업은 계속 실행 중입니다."}))]
            }
        } else if !have("open") {
            vec![("open", json!({"target":"a.txt"}))]
        } else if !have("change") {
            vec![(
                "change",
                json!({"op":"edit","target":"a.txt","old":"original","new":"private-current-world"}),
            )]
        } else {
            self.paused.store(true, std::sync::atomic::Ordering::SeqCst);
            let start = std::time::Instant::now();
            while !self.release.load(std::sync::atomic::Ordering::SeqCst)
                && start.elapsed() < Duration::from_secs(10)
                && !r
                    .cancel
                    .as_ref()
                    .unwrap()
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            vec![("finish", json!({"summary":"메인 작업 완료"}))]
        };
        let parts = calls
            .into_iter()
            .enumerate()
            .map(|(i, (name, arguments))| Part::ToolCall {
                id: format!("{i}"),
                name: name.into(),
                arguments,
            })
            .collect();
        ScriptedModel::new([Response {
            parts,
            usage: Usage {
                input: Some(10),
                cached_input: Some(0),
                output: Some(5),
            },
            stop: Stop::ToolUse,
            model: "scripted".into(),
            latency: Duration::ZERO,
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        }])
        .complete(need, r)
    }
}
#[test]
fn inquiry_reads_private_world_while_main_execution_continues() {
    use loom_tui::agent::AgentCommand;
    use std::sync::atomic::Ordering;
    let d = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(d.path())
            .status()
            .unwrap()
            .success()
    );
    fs::write(d.path().join("a.txt"), "original").unwrap();
    let model = Arc::new(ParallelQuestionModel {
        paused: false.into(),
        release: false.into(),
        inquiry: false.into(),
    });
    let mut b = VersionBackend::with_model(
        Version::Loom,
        d.path(),
        model.clone(),
        Settings {
            checks: vec!["git diff --check".into()],
            policy: ExecutionPolicy {
                sandbox: SandboxMode::FullAccess,
                network: false,
            },
            ..Default::default()
        },
    )
    .unwrap();
    while b.poll().unwrap().is_some() {}
    b.command(AgentCommand::Submit {
        text: "메인 작업".into(),
        model: "auto".into(),
        effort: None,
        skills: vec![],
        images: vec![],
    })
    .unwrap();
    let started = std::time::Instant::now();
    let mut preparing = false;
    while !model.paused.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(5) {
        if let Some(event) = b.poll().unwrap() {
            if let AgentUpdate::EntryUpdated(entry) = event.update {
                preparing |= entry.kind == Kind::Assistant && entry.body == "대화 기록 불러오는 중";
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(model.paused.load(Ordering::SeqCst));
    let until = std::time::Instant::now() + Duration::from_millis(500);
    let mut waiting = false;
    while std::time::Instant::now() < until && (!waiting || !preparing) {
        if let Some(event) = b.poll().unwrap() {
            if let AgentUpdate::EntryUpdated(entry) = event.update {
                preparing |= entry.kind == Kind::Assistant && entry.body == "대화 기록 불러오는 중";
                waiting |=
                    entry.kind == Kind::Assistant && entry.body.contains("모델 응답 기다리는 중");
            }
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    assert!(
        preparing,
        "show progress before Core finishes repository preparation"
    );
    assert!(
        waiting,
        "model request must reach the visible progress entry while the provider is still running"
    );
    b.command(AgentCommand::Inquiry("현재 작업은?".into()))
        .unwrap();
    let mut answered = false;
    let mut prematurely_completed = false;
    let start = std::time::Instant::now();
    while !answered && start.elapsed() < Duration::from_secs(5) {
        if let Some(e) = b.poll().unwrap() {
            match e.update {
                AgentUpdate::EntryUpdated(entry)
                    if entry.title == "병렬 질문"
                        && entry.status == "completed"
                        && entry.kind == Kind::Assistant =>
                {
                    assert!(
                        entry.body.contains("병렬 질문"),
                        "parallel reply must be distinguishable from the main result"
                    );
                    answered = true
                }
                AgentUpdate::TurnCompleted { .. } => prematurely_completed = true,
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(answered);
    assert!(!prematurely_completed);
    assert_eq!(
        fs::read_to_string(d.path().join("a.txt")).unwrap(),
        "original"
    );
    model.release.store(true, Ordering::SeqCst);
    let start = std::time::Instant::now();
    while Core::open(d.path())
        .unwrap()
        .history()
        .unwrap()
        .iter()
        .all(|t| !t.pending())
        && start.elapsed() < Duration::from_secs(5)
    {
        b.poll().unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        Core::open(d.path())
            .unwrap()
            .history()
            .unwrap()
            .iter()
            .any(|t| t.pending())
    );
    b.shutdown().unwrap();
}

#[test]
fn explicit_answer_intent_keeps_the_same_v5_tools_and_worker() {
    let d = tempfile::tempdir().unwrap();
    assert!(Command::new("git").args(["init", "-q"]).arg(d.path()).status().unwrap().success());
    let model = Arc::new(ScriptedModel::with_candidate(
        loom_core::models::router::Candidate {
            endpoint: "scripted".into(),
            model: "scripted".into(),
            tier: loom_core::models::Tier::Flagship,
            cost: loom_core::models::CostClass::Local,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        },
        [Ok(Response {
        parts: vec![Part::Text("안녕하세요!".into())],
        stop: Stop::EndTurn, usage: Usage::default(), model: "scripted".into(), latency: Duration::ZERO,
        route: None, gateway_connection: None, gateway_upstream_ms: None
    })]));
    let outcome = loom_tui::backend::version_core::execute(
        Version::Loom, d.path(), "안녕", loom_tui::backend::version_core::Intent::Answer,
        Settings::default(), model.clone(), Arc::new(AtomicBool::new(false)), Arc::new(|_| {})
    ).unwrap();
    assert_eq!(outcome.status, "Answered");
    let requests = model.requests();
    assert!(requests[0].tools.iter().any(|tool| tool.name == "run"));
    assert_eq!(requests[0].messages.last().unwrap().text(), "안녕");
}
