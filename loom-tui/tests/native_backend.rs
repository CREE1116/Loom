use loom_tui::{agent::*, app::App, backend::native::NativeBackend};
use loom_core::{
    control::ExecutionOptions,
    models::{Part, Response, ScriptedModel, Stop, Usage},
};
use serde_json::json;
use std::{
    fs,
    process::Command,
    sync::{Arc, atomic::{AtomicBool, Ordering}},
    time::{Duration, Instant},
};

/// Model policy (cost ceiling, connections) comes from a private directory, never the
/// user's own `~/.loom/providers.toml`, so a changed personal setting cannot fail tests.
fn isolate_model_state() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        // SAFETY: set once, before any test in this binary reads model policy.
        unsafe { std::env::set_var("LOOM_MODEL_STATE_DIR", dir) };
    });
}

struct BlockingModel(Arc<AtomicBool>);
struct ProfiledModel(ScriptedModel);

#[test]
fn native_task_cost_counts_failed_model_requests() {
    isolate_model_state();
    struct Port;
    impl loom_core::models::ModelPort for Port {
        fn complete(&self,_need:loom_core::models::Need,_request:&loom_core::models::Request)->Result<loom_core::models::Served,loom_core::models::ProviderError> {
            Err(loom_core::models::ProviderError::new(loom_core::models::ErrorKind::BadRequest,"simulated model failure"))
        }
    }
    let d=tempfile::tempdir().unwrap();
    Command::new("git").current_dir(d.path()).args(["init","-q"]).status().unwrap();
    let mut backend=NativeBackend::with_model(d.path(),Arc::new(Port),ExecutionOptions {max_attempts:1,..Default::default()}).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {text:":edit create a file".into(),model:"local".into(),effort:None,skills:vec![],images:vec![]}).unwrap();
    let deadline=Instant::now()+Duration::from_secs(10);
    let mut calls=None;
    let mut ended=false;
    while Instant::now()<deadline {
        if let Some(event)=backend.poll().unwrap() {
            match event.update {
                AgentUpdate::TaskUpdated(task)=>{if let Some(cost)=task.cost {calls=Some(cost.model_calls);}}
                AgentUpdate::TurnCompleted {..}=>{ended=true;break;}
                _=>{}
            }
        } else {std::thread::sleep(Duration::from_millis(5));}
    }
    assert!(ended);
    assert_eq!(calls,Some(1),"failed requests disappeared from the displayed task cost");
}

#[test]
fn native_graph_dispatch_projects_split_and_keeps_child_activity_history() {
    isolate_model_state();
    struct Port { left: ScriptedModel, right: ScriptedModel }
    impl loom_core::models::ModelPort for Port {
        fn complete(&self, need: loom_core::models::Need, request: &loom_core::models::Request)
            -> Result<loom_core::models::Served, loom_core::models::ProviderError> {
            if request.messages.iter().any(|m|m.text().contains("Owned write files: left.rs")) {
                self.left.complete(need, request)
            } else {self.right.complete(need, request)}
        }
    }
    fn worker(path: &str) -> ScriptedModel {
        ScriptedModel::new([
            ("read",json!({"path":path})),
            ("edit",json!({"path":path,"old":"{ 1 }","new":"{ 2 }"})),
            ("finish",json!({"summary":format!("{path} scoped proposal")})),
        ].map(|(name,args)|Response {parts:vec![Part::ToolCall {id:name.into(),name:name.into(),arguments:args}],usage:Usage::default(),stop:Stop::ToolUse,model:"scripted".into(),latency:Duration::ZERO,route:None,gateway_connection:None,gateway_upstream_ms:None}))
    }
    let d = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(d.path()).args(["init","-q"]).status().unwrap();
    fs::write(d.path().join("left.rs"),"pub fn left_value() -> u32 { 1 }\n").unwrap();
    fs::write(d.path().join("right.rs"),"pub fn right_value() -> u32 { 1 }\n").unwrap();
    let mut backend = NativeBackend::with_model(d.path(),Arc::new(Port {left:worker("left.rs"),right:worker("right.rs")}),ExecutionOptions {checks:Some(vec![if cfg!(windows) { "exit /b 0" } else { "true" }.into()]),..Default::default()}).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {text:":edit Change left_value and right_value to return 2".into(),model:"scripted".into(),effort:None,skills:vec![],images:vec![]}).unwrap();
    let deadline=Instant::now()+Duration::from_secs(15);
    let mut split=false;
    let mut child=None;
    let mut ready=false;
    while Instant::now()<deadline {
        if let Some(event)=backend.poll().unwrap() {
            match event.update {
                AgentUpdate::TaskUpdated(task) if task.parent.is_none() => {split |= task.partition.is_some_and(|p|p.mode==ExecutionMode::Split && p.groups.len()==2);}
                AgentUpdate::ActivityUpdated(activity) => {if activity.status=="completed" {child=Some(activity.id);}}
                AgentUpdate::ApprovalRequested(_) => {ready=true;break;}
                _=>{}
            }
        } else {std::thread::sleep(Duration::from_millis(5));}
    }
    assert!(ready,"merged proposal did not reach approval");
    assert!(split,"Native replaced Core's actual Split decision with Single");
    let child=child.expect("merged child never reached completed state");
    backend.command(AgentCommand::ReadActivity(child.clone())).unwrap();
    let events:Vec<_>=std::iter::from_fn(||backend.poll().unwrap()).collect();
    assert!(events.iter().any(|event|matches!(&event.update,AgentUpdate::ActivityHistory {id,entries,status} if id==&child && status=="completed" && entries.iter().any(|entry|entry.title.contains("수정 완료")))),"child history lost observed tool results: {events:?}");
    backend.shutdown().unwrap();
    let mut restored=NativeBackend::with_model(d.path(),Arc::new(ScriptedModel::new([])),ExecutionOptions::default()).unwrap();
    while restored.poll().unwrap().is_some() {}
    restored.command(AgentCommand::ReadActivity(child.clone())).unwrap();
    assert!(std::iter::from_fn(||restored.poll().unwrap()).any(|event|matches!(event.update,AgentUpdate::ActivityHistory {id,entries,status} if id==child && status=="completed" && entries.iter().any(|entry|entry.title.contains("수정 완료")))),"child history must survive reopening Loom");
}
impl loom_core::models::ModelPort for ProfiledModel {
    fn complete(&self, need: loom_core::models::Need, request: &loom_core::models::Request)
        -> Result<loom_core::models::Served, loom_core::models::ProviderError> {
        loom_core::models::ModelPort::complete(&self.0, need, request)
    }
    fn model_profiles(&self) -> Vec<loom_core::models::ModelProfile> {
        vec![loom_core::models::ModelProfile {
            candidate: loom_core::models::router::Candidate { endpoint: "scripted".into(), model: "scripted".into(), tier: loom_core::models::Tier::Local, cost: loom_core::models::CostClass::Local, price: Some((0.0,0.0)), tools: true, context: None },
            unavailable_reason: None, supported_efforts: vec!["high".into()], default_effort: Some("high".into()),
        }]
    }
}

#[test]
fn native_model_picker_lists_routes_and_preserves_selection_on_session_change() {
    isolate_model_state();
    let d = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(d.path()).args(["init", "-q"]).status().unwrap();
    let model = Arc::new(ProfiledModel(ScriptedModel::new([Response {
        parts: vec![Part::Text("answer".into())], usage: Usage::default(), stop: Stop::EndTurn,
        model: "actually-served".into(), latency: Duration::ZERO, route: None,
        gateway_connection: None, gateway_upstream_ms: None,
    }])));
    let mut backend = NativeBackend::with_model(d.path(), model.clone(), ExecutionOptions::default()).unwrap();
    let startup: Vec<_> = std::iter::from_fn(|| backend.poll().unwrap()).collect();
    assert!(startup.iter().any(|event| matches!(&event.update,
        AgentUpdate::Notice(text) if text.contains(":inspect") && text.contains(":chat")
    )), "startup must explain Native's explicit inspect/chat modes");
    assert!(startup.iter().any(|e| matches!(&e.update, AgentUpdate::ModelsLoaded(names) if names == &vec!["auto".to_owned(), "scripted/scripted".to_owned()])));
    backend.command(AgentCommand::Submit { text: ":chat arbitrary message".into(), model: "scripted/scripted".into(), effort: Some("high".into()), skills: vec![], images: vec![] }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut resolved = false;
    let mut complete = false;
    while Instant::now() < deadline {
        if let Some(event) = backend.poll().unwrap() {
            match event.update {
                AgentUpdate::ModelResolved { requested, provider, model } => { assert_eq!(requested, "scripted/scripted"); assert_eq!(provider, "scripted"); assert_eq!(model, "actually-served"); resolved = true; }
                AgentUpdate::TurnCompleted { .. } => { complete = true; break; }
                _ => {}
            }
        } else { std::thread::sleep(Duration::from_millis(5)); }
    }
    assert!(resolved && complete);
    assert_eq!(model.0.requests()[0].model, "scripted/scripted");
    assert_eq!(model.0.requests()[0].reasoning_effort.as_deref(), Some("high"));
    backend.command(AgentCommand::OpenSession { id: None, model: "scripted/scripted".into() }).unwrap();
    assert!(std::iter::from_fn(|| backend.poll().unwrap()).any(|event| matches!(event.update, AgentUpdate::SessionLoaded { model: Some(model), .. } if model == "scripted/scripted")));
}
impl loom_core::models::ModelPort for BlockingModel {
    fn complete(&self, _need: loom_core::models::Need, _request: &loom_core::models::Request)
        -> Result<loom_core::models::Served, loom_core::models::ProviderError> {
        self.0.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(2));
        Err(loom_core::models::ProviderError::new(
            loom_core::models::ErrorKind::Transient, "simulated provider read timeout"))
    }
}

#[test]
fn native_chat_interrupt_completes_while_provider_is_blocked() {
    isolate_model_state();
    let d = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(d.path()).args(["init", "-q"]).status().unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let mut backend = NativeBackend::with_model(d.path(),
        Arc::new(BlockingModel(entered.clone())), ExecutionOptions::default()).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {
        text: ":chat hello".into(), model: "local".into(),
        effort: None, skills: vec![], images: vec![],
    }).unwrap();
    let start = Instant::now();
    while !entered.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(1) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(entered.load(Ordering::SeqCst));
    backend.command(AgentCommand::Interrupt).unwrap();
    let deadline = Instant::now() + Duration::from_millis(600);
    let mut interrupted = false;
    while Instant::now() < deadline {
        if let Some(event) = backend.poll().unwrap() {
            if matches!(event.update, AgentUpdate::TurnCompleted { status, .. } if status == "interrupted") {
                interrupted = true;
                break;
            }
        } else { std::thread::sleep(Duration::from_millis(5)); }
    }
    assert!(interrupted, "interrupt waited for a blocked provider response");
}
fn backend() -> (tempfile::TempDir, NativeBackend) {
    let d = tempfile::tempdir().unwrap();
    Command::new("git")
        .current_dir(d.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    fs::write(d.path().join("file.txt"), "old\n").unwrap();
    let replies = [
        ("read", json!({"path":"file.txt"})),
        ("edit", json!({"path":"file.txt","old":"old","new":"new"})),
        ("finish", json!({"summary":"Native proposal"})),
    ]
    .into_iter()
    .map(|(name, args)| Response {
        parts: vec![Part::ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: args,
        }],
        usage: Usage {
            input: Some(10),
            cached_input: Some(0),
            output: Some(2),
        },
        stop: Stop::ToolUse,
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    });
    let backend = NativeBackend::with_model(
        d.path(),
        Arc::new(ScriptedModel::new(replies)),
        ExecutionOptions {
            checks: Some(vec![if cfg!(windows) { "exit /b 0" } else { "true" }.into()]),
            ..Default::default()
        },
    )
    .unwrap();
    (d, backend)
}

#[test]
fn native_edit_submission_echoes_user_before_core_preparation_and_only_once() {
    isolate_model_state();
    let (_directory, mut backend) = backend();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {
        text: "change file".into(), model: "local".into(), effort: None,
        skills: vec![], images: vec![],
    }).unwrap();
    // Queued synchronously, even if Core is still indexing the repository.
    let mut immediate = vec![];
    while let Some(event) = backend.poll().unwrap() {
        immediate.push(event.update);
        if immediate.len() >= 3 { break; }
    }
    assert!(immediate.iter().any(|u| matches!(u,
        AgentUpdate::EntryUpdated(e) if e.kind == Kind::User && e.body == "change file"
    )), "missing immediate echo: {immediate:?}");
    let mut all = immediate;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(event) = backend.poll().unwrap() {
            if matches!(event.update, AgentUpdate::ApprovalRequested(_)) {
                all.push(event.update);
                break;
            }
            all.push(event.update);
        } else { std::thread::sleep(Duration::from_millis(5)); }
    }
    assert_eq!(all.iter().filter(|u| matches!(u,
        AgentUpdate::EntryUpdated(e) if e.kind == Kind::User && e.body == "change file"
    )).count(), 1, "duplicate user echo: {all:?}");
}

#[test]
fn native_simple_chat_completes_after_one_tool_free_call_and_persists() {
    isolate_model_state();
    let directory = tempfile::tempdir().unwrap();
    assert!(Command::new("git").args(["init", "-q"])
        .current_dir(directory.path()).status().unwrap().success());
    let model = Arc::new(ScriptedModel::new([Response {
        parts: vec![Part::Text("I am the selected model.".into())],
        usage: Usage { input: Some(15), cached_input: Some(0), output: Some(8) },
        stop: Stop::EndTurn,
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    }]));
    let mut backend = NativeBackend::with_model(directory.path(), model.clone(), ExecutionOptions::default()).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {
        text: ":chat 너는 무슨 모델이야?".into(), model: "local".into(),
        effort: None, skills: vec![], images: vec![],
    }).unwrap();
    let mut updates = vec![];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(event) = backend.poll().unwrap() {
            let finished = matches!(event.update, AgentUpdate::TurnCompleted { .. });
            updates.push(event.update);
            if finished { break; }
        } else { std::thread::sleep(Duration::from_millis(5)); }
    }
    assert!(updates.iter().any(|u| matches!(u, AgentUpdate::TurnCompleted { status, .. } if status == "completed")), "{updates:?}");
    assert!(updates.iter().any(|u| matches!(u, AgentUpdate::EntryUpdated(entry) if entry.kind == Kind::User && entry.body == "너는 무슨 모델이야?")));
    assert!(updates.iter().any(|u| matches!(u, AgentUpdate::EntryUpdated(entry) if entry.kind == Kind::Assistant && entry.body == "I am the selected model.")));
    let requests = model.requests();
    assert_eq!(requests.len(), 1, "chat must make exactly one model call");
    assert!(requests[0].tools.is_empty(), "chat must not load coding tools");
    assert!(!requests[0].messages.iter().any(|m| m.text().contains("workspace graph")));
    assert_eq!(loom_core::sessions::Sessions::open(directory.path()).unwrap().get("native").unwrap().turns.len(), 1);
    let mut restored = NativeBackend::with_model(directory.path(), model, ExecutionOptions::default()).unwrap();
    assert!(std::iter::from_fn(|| restored.poll().unwrap()).any(|e| matches!(e.update, AgentUpdate::SessionLoaded { snapshot, .. } if snapshot.turns.as_ref().is_some_and(|t| t.iter().any(|t| t.entries.iter().any(|e| e.body == "I am the selected model."))))));
}

#[test]
fn native_restored_unresolved_task_is_failed_instead_of_interrupted() {
    isolate_model_state();
    let d = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(d.path()).args(["init", "-q"]).status().unwrap();
    let model = Arc::new(ScriptedModel::new([Response {
        parts: vec![Part::ToolCall {
            id: "finish".into(), name: "finish".into(),
            arguments: json!({"summary":"Could not finish the requested task", "unresolved":true}),
        }],
        usage: Usage::default(), stop: Stop::ToolUse,
        model: "scripted".into(), latency: Duration::ZERO, route: None,
        gateway_connection: None, gateway_upstream_ms: None,
    }]));
    let mut backend = NativeBackend::with_model(d.path(), model.clone(), ExecutionOptions {
        max_attempts: 1, ..ExecutionOptions::default()
    }).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {
        text: "attempt a task".into(), model: "local".into(),
        effort: None, skills: vec![], images: vec![],
    }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut failed = false;
    while Instant::now() < deadline {
        if let Some(event) = backend.poll().unwrap() {
            if let AgentUpdate::TurnCompleted { status, .. } = event.update {
                failed = status == "failed";
                break;
            }
        } else { std::thread::sleep(Duration::from_millis(5)); }
    }
    assert!(failed, "the original unresolved task should fail");
    backend.shutdown().unwrap();
    let mut restored = NativeBackend::with_model(d.path(), model, ExecutionOptions::default()).unwrap();
    let loaded = std::iter::from_fn(|| restored.poll().unwrap()).find_map(|event| {
        if let AgentUpdate::SessionLoaded { snapshot, .. } = event.update { Some(snapshot) } else { None }
    }).unwrap();
    let turn = loaded.turns.unwrap().into_iter().find(|turn| {
        turn.entries.iter().any(|entry| entry.body == "attempt a task")
    }).expect("restored task missing");
    assert_eq!(turn.status, "failed", "restoring a failed Native task must preserve its status");
}

#[test]
fn greeting_uses_normal_worker_without_keyword_routing() {
    isolate_model_state();
    let d = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(d.path()).args(["init", "-q"]).status().unwrap();
    let model = Arc::new(ScriptedModel::new([Response {
        parts: vec![Part::ToolCall { id: "finish".into(), name: "finish".into(),
            arguments: json!({"summary":"no edit requested","unresolved":true}) }],
        usage: Usage::default(), stop: Stop::ToolUse,
        model: "scripted".into(), latency: Duration::ZERO, route: None,
        gateway_connection: None, gateway_upstream_ms: None,
    }]));
    let mut backend = NativeBackend::with_model(d.path(), model.clone(), ExecutionOptions {
        max_attempts: 1, ..ExecutionOptions::default()
    }).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {
        text: "안녕".into(), model: "local".into(), effort: None,
        skills: vec![], images: vec![],
    }).unwrap();
    let start = Instant::now();
    let mut updates = vec![];
    while start.elapsed() < Duration::from_secs(8) {
        if let Some(event) = backend.poll().unwrap() {
            let done = matches!(event.update, AgentUpdate::TurnCompleted { .. });
            updates.push(event.update);
            if done { break; }
        } else { std::thread::sleep(Duration::from_millis(5)); }
    }
    assert!(updates.iter().any(|e| matches!(e, AgentUpdate::TurnCompleted { .. })), "{updates:?}");
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.iter().any(|tool| tool.name == "finish"), "keyword shortcut still active");
}

#[test]
fn native_explicit_edit_submissions_require_a_patch() {
    isolate_model_state();
    let directory = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(directory.path()).args(["init", "-q"])
        .status().unwrap();
    let model = Arc::new(ScriptedModel::new([Response {
        parts: vec![Part::Text("I changed the file.".into())],
        usage: Usage::default(), stop: Stop::EndTurn,
        model: "scripted".into(), latency: Duration::ZERO, route: None,
        gateway_connection: None, gateway_upstream_ms: None,
    }]));
    let mut backend = NativeBackend::with_model(directory.path(), model,
        ExecutionOptions { max_attempts: 1, ..Default::default() }).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {
        text: ":edit change code".into(), model: "local".into(), effort: None,
        skills: vec![], images: vec![],
    }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut finished = None;
    while Instant::now() < deadline {
        if let Some(event) = backend.poll().unwrap() {
            if let AgentUpdate::TurnCompleted { status, .. } = event.update {
                finished = Some(status);
                break;
            }
        } else { std::thread::sleep(Duration::from_millis(5)); }
    }
    assert_ne!(finished.as_deref(), Some("completed"), "prose without a patch was accepted");
    assert!(finished.is_some());
}

#[test]
fn native_plain_tool_question_completes_without_a_patch_or_failure_report() {
    isolate_model_state();
    let directory = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(directory.path()).args(["init", "-q"]).status().unwrap();
    let answer = "현재 제공된 도구는 read, outline, find, edit, write, check, finish 등입니다. 추가 그룹은 enable_tools로 활성화합니다.";
    let model = Arc::new(ScriptedModel::new([Response {
        parts: vec![Part::Text(answer.into())], usage: Usage::default(), stop: Stop::EndTurn,
        model: "scripted".into(), latency: Duration::ZERO, route: None,
        gateway_connection: None, gateway_upstream_ms: None,
    }]));
    let mut backend = NativeBackend::with_model(directory.path(), model.clone(),
        ExecutionOptions { max_attempts: 1, ..Default::default() }).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {
        text: "너에게 주어진 도구가 뭔지 말해봐".into(), model: "local".into(), effort: None,
        skills: vec![], images: vec![],
    }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut updates = Vec::new();
    while Instant::now() < deadline {
        if let Some(event) = backend.poll().unwrap() {
            let done = matches!(event.update, AgentUpdate::TurnCompleted { .. });
            updates.push(event.update);
            if done { break; }
        } else { std::thread::sleep(Duration::from_millis(5)); }
    }
    assert!(updates.iter().any(|event| matches!(event, AgentUpdate::TurnCompleted { status, .. } if status == "completed")), "{updates:?}");
    assert!(!updates.iter().any(|event| matches!(event, AgentUpdate::EntryUpdated(entry) if entry.body.contains("Core 실행 보고") || entry.body.contains("No file changes"))), "{updates:?}");
    assert_eq!(model.requests().len(), 1, "no extra classifier call or retry");
    let sessions = loom_core::sessions::Sessions::open(directory.path()).unwrap();
    assert_eq!(sessions.get("native").unwrap().turns.len(), 1);
}

#[test]
fn native_keeps_batched_tool_results_and_shows_core_progress_and_stop_report() {
    isolate_model_state();
    let directory = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(directory.path()).args(["init","-q"]).status().unwrap();
    fs::write(directory.path().join("first.txt"), "first evidence\n").unwrap();
    fs::write(directory.path().join("second.txt"), "second evidence\n").unwrap();
    let model = Arc::new(ScriptedModel::new([Response {
        parts: vec![Part::ToolCall { id: "first".into(), name: "read".into(), arguments: json!({"path":"first.txt"}) },
            Part::ToolCall { id: "second".into(), name: "read".into(), arguments: json!({"path":"second.txt"}) }],
        usage: Usage::default(), stop: Stop::ToolUse, model: "scripted".into(), latency: Duration::ZERO,
        route: None, gateway_connection: None, gateway_upstream_ms: None,
    }]));
    let mut backend = NativeBackend::with_model(directory.path(), model, ExecutionOptions {
        max_attempts: 1, budget: loom_core::worker::Budget { steps: 1, seconds: 30, tokens: 10000 },
        ..Default::default()
    }).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit { text: "fix files".into(), model: "local".into(),
        effort: None, skills: vec![], images: vec![] }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut entries = std::collections::BTreeMap::new();
    let mut ended = false;
    while Instant::now() < deadline {
        if let Some(event) = backend.poll().unwrap() {
            match event.update {
                AgentUpdate::EntryUpdated(entry) => { entries.insert(entry.id.clone(), entry); }
                AgentUpdate::TurnCompleted { .. } => { ended = true; break; }
                _ => {}
            }
        } else { std::thread::sleep(Duration::from_millis(5)); }
    }
    assert!(ended);
    let tools: Vec<_> = entries.values().filter(|e|e.kind == Kind::Tool).collect();
    assert_eq!(tools.len(), 2, "batched reads overwrote one another: {entries:?}");
    assert!(tools.iter().any(|e|e.title.contains("first.txt") && e.body.contains("first evidence")));
    assert!(tools.iter().any(|e|e.title.contains("second.txt") && e.body.contains("second evidence")));
    assert!(entries.values().any(|e|e.kind == Kind::Assistant && e.title == "Core 진행"));
    assert!(entries.values().any(|e|e.kind == Kind::Assistant && e.body.contains("남은 작업") && e.body.contains("원본 반영: 없음")));
}

#[test]
fn native_inspect_prefix_preserves_readonly_intent_without_leaking_to_next_edit() {
    isolate_model_state();
    let d = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(d.path()).args(["init", "-q"])
        .status().unwrap();
    let model = Arc::new(ScriptedModel::new([
        Response { parts: vec![Part::Text("inspected".into())],
            usage: Usage::default(), stop: Stop::EndTurn, model: "scripted".into(),
            latency: Duration::ZERO, route: None, gateway_connection: None, gateway_upstream_ms: None },
        Response { parts: vec![Part::Text("edited".into())],
            usage: Usage::default(), stop: Stop::EndTurn, model: "scripted".into(),
            latency: Duration::ZERO, route: None, gateway_connection: None, gateway_upstream_ms: None },
    ]));
    let mut backend = NativeBackend::with_model(d.path(), model,
        ExecutionOptions { max_attempts: 1, ..Default::default() }).unwrap();
    while backend.poll().unwrap().is_some() {}
    for (goal, expect) in [ (":inspect check this code", "check this code"), (":edit change code", "change code") ] {
        backend.command(AgentCommand::Submit { text: goal.into(), model: "local".into(),
            effort: None, skills: vec![], images: vec![] }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut done = None;
        while Instant::now() < deadline {
            if let Some(event) = backend.poll().unwrap() {
                if let AgentUpdate::TurnCompleted { status, .. } = event.update {
                    done = Some(status); break;
                }
            } else { std::thread::sleep(Duration::from_millis(5)); }
        }
        assert!(done.is_some(), "{goal} did not finish");
        if expect == "change code" {
            assert_ne!(done.as_deref(), Some("completed"), "inspect mode leaked into edit");
        }
    }
}
#[test]
fn native_ui_requires_approval_and_only_shows_completion_after_verified_application() {
    isolate_model_state();
    let (d, mut backend) = backend();
    let mut app = App::new(false);
    while let Some(event) = backend.poll().unwrap() {
        app.apply_event(event);
    }
    backend
        .command(AgentCommand::Submit {
            text: "change file".into(),
            model: "local".into(),
            effort: None,
            skills: vec![],
            images: vec![],
        })
        .unwrap();
    let start = Instant::now();
    let mut approval = None;
    let mut completed = false;
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(event) = backend.poll().unwrap() {
            match &event.update {
                AgentUpdate::TaskUpdated(task) if task.status == TaskStatus::Running => assert!(
                    task.worker.is_some(),
                    "Running Native task lost its allocated tier"
                ),
                AgentUpdate::ApprovalRequested(a) => approval = Some(a.clone()),
                AgentUpdate::TurnCompleted { .. } => completed = true,
                _ => {}
            }
            app.apply_event(event);
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
        if approval.is_some() {
            break;
        }
    }
    assert!(!completed);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
    let approval = approval.unwrap();
    for width in [40, 80, 140] {
        let screen = app.render(width, 30);
        assert!(
            (0..screen.height)
                .map(|y| screen.plain_line(y))
                .collect::<Vec<_>>()
                .join("\n")
                .contains("승인")
        );
    }
    backend
        .command(AgentCommand::Reply {
            request_id: approval.id,
            choice: "accept".into(),
        })
        .unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(event) = backend.poll().unwrap() {
            if let AgentUpdate::TurnCompleted { status, .. } = &event.update {
                assert_eq!(status, "completed");
                completed = true;
            }
            app.apply_event(event);
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
        if completed {
            break;
        }
    }
    assert!(completed);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "new\n"
    );
    backend.shutdown().unwrap();
}

#[test]
fn native_web_permission_is_requested_and_only_enables_the_current_task() {
    isolate_model_state();
    use std::io::{Read, Write};
    let d = tempfile::tempdir().unwrap();
    Command::new("git").current_dir(d.path()).args(["init", "-q"]).status().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/page", listener.local_addr().unwrap());
    let replies = [
        ("web_fetch", json!({"url":url})),
        ("finish", json!({"summary":"web page inspected"})),
    ].into_iter().map(|(name, arguments)| Response {
        parts: vec![Part::ToolCall { id: name.into(), name: name.into(), arguments }],
        usage: Usage::default(), stop: Stop::ToolUse,
        model: "scripted".into(), latency: Duration::ZERO, route: None,
        gateway_connection: None, gateway_upstream_ms: None,
    });
    let mut backend = NativeBackend::with_model(
        d.path(), Arc::new(ScriptedModel::new(replies)), ExecutionOptions {
            policy: loom_core::effects::ExecutionPolicy {
                sandbox: loom_core::effects::SandboxMode::WorkspaceWrite,
                network: false,
            },
            ..Default::default()
        },
    ).unwrap();
    while backend.poll().unwrap().is_some() {}
    backend.command(AgentCommand::Submit {
        text: ":inspect fetch a page".into(), model: "local".into(), effort: None,
        skills: vec![], images: vec![],
    }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let approval = loop {
        assert!(Instant::now() < deadline, "web permission approval never arrived");
        if let Some(event) = backend.poll().unwrap() {
            if let AgentUpdate::ApprovalRequested(a) = event.update {
                assert!(a.title.contains("웹 접근"), "{a:?}");
                assert!(a.summary.contains("web_fetch"));
                assert!(a.summary.contains(&url));
                break a;
            }
        } else { std::thread::sleep(Duration::from_millis(10)); }
    };
    assert!(backend.command(AgentCommand::Reply {
        request_id: "wrong-web-id".into(), choice: "accept".into(),
    }).is_err());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0; 2048];
        let n = stream.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("GET /page"));
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 18\r\nConnection: close\r\n\r\npermission granted").unwrap();
    });
    backend.command(AgentCommand::Reply {
        request_id: approval.id, choice: "accept".into(),
    }).unwrap();
    let mut fetched = false;
    let mut completed = false;
    while Instant::now() < deadline {
        if let Some(event) = backend.poll().unwrap() {
            match event.update {
                AgentUpdate::EntryUpdated(entry) if entry.title.starts_with("web_fetch · ") && entry.status == "completed" => {
                    fetched = entry.body.contains("permission granted");
                }
                AgentUpdate::TurnCompleted { status, .. } => {
                    assert_eq!(status, "completed");
                    completed = true;
                    break;
                }
                _ => {}
            }
        } else { std::thread::sleep(Duration::from_millis(10)); }
    }
    assert!(fetched && completed, "web tool did not resume after consent");
    server.join().unwrap();
    backend.shutdown().unwrap();
}

#[test]
fn native_reject_and_interrupt_preserve_original_and_report_interrupted() {
    isolate_model_state();
    for interrupt in [false, true] {
        let (d, mut backend) = backend();
        while backend.poll().unwrap().is_some() {}
        backend
            .command(AgentCommand::Submit {
                text: "change file".into(),
                model: "local".into(),
                effort: None,
                skills: vec![],
                images: vec![],
            })
            .unwrap();
        if interrupt {
            backend.command(AgentCommand::Interrupt).unwrap();
        }
        let start = Instant::now();
        let mut completed = false;
        let mut stop_report = false;
        while start.elapsed() < Duration::from_secs(10) {
            if let Some(event) = backend.poll().unwrap() {
                match event.update {
                    AgentUpdate::ApprovalRequested(approval) => {
                        assert!(
                            backend
                                .command(AgentCommand::Reply {
                                    request_id: "wrong".into(),
                                    choice: "accept".into()
                                })
                                .is_err()
                        );
                        backend
                            .command(AgentCommand::Reply {
                                request_id: approval.id,
                                choice: "reject".into(),
                            })
                            .unwrap();
                    }
                    AgentUpdate::EntryUpdated(entry) if entry.body.contains("Core 실행 보고") => {
                        assert!(entry.body.contains("원본 반영: 없음"));
                        assert!(entry.body.contains("남은 작업"));
                        stop_report = true;
                    }
                    AgentUpdate::TurnCompleted { status, .. } => {
                        assert_eq!(status, "interrupted");
                        completed = true;
                        break;
                    }
                    _ => {}
                }
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(completed);
        if interrupt {
            assert!(stop_report, "Native cancellation silently dropped the Core report");
        }
        assert_eq!(
            fs::read_to_string(d.path().join("file.txt")).unwrap(),
            "old\n"
        );
        backend.shutdown().unwrap();
        assert!(backend.command(AgentCommand::RefreshModels).is_err());
    }
}

#[test]
fn readonly_native_cannot_accept_a_persisted_edit_proposal() {
    isolate_model_state();
    let (d, mut writer) = backend();
    while writer.poll().unwrap().is_some() {}
    writer
        .command(AgentCommand::Submit {
            text: "change file".into(),
            model: "local".into(),
            effort: None,
            skills: vec![],
            images: vec![],
        })
        .unwrap();
    let start = Instant::now();
    let mut proposed = false;
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(event) = writer.poll().unwrap() {
            if matches!(event.update, AgentUpdate::ApprovalRequested(_)) {
                proposed = true;
                break;
            }
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(proposed);
    // Keep the writer alive so its shutdown cancellation does not reject the proposal.
    let mut reader = NativeBackend::with_model(
        d.path(),
        Arc::new(ScriptedModel::new([])),
        ExecutionOptions {
            readonly: true,
            ..Default::default()
        },
    )
    .unwrap();
    let mut approval = None;
    while let Some(event) = reader.poll().unwrap() {
        if let AgentUpdate::ApprovalRequested(a) = event.update {
            approval = Some(a.id);
        }
    }
    assert!(
        reader
            .command(AgentCommand::Reply {
                request_id: approval.unwrap(),
                choice: "accept".into()
            })
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
    reader.shutdown().unwrap();
    writer.shutdown().unwrap();
}

#[test]
fn native_sessions_goals_skills_compaction_and_permissions_are_connected() {
    isolate_model_state();
    let (d, mut backend) = backend();
    while backend.poll().unwrap().is_some() {}
    backend
        .command(AgentCommand::Goal(GoalAction::Set(
            "remember project goal".into(),
        )))
        .unwrap();
    assert!(std::iter::from_fn(||backend.poll().unwrap()).any(|event|matches!(event.update,AgentUpdate::GoalUpdated(Some(goal)) if goal.objective=="remember project goal")));
    backend
        .command(AgentCommand::Goal(GoalAction::Pause))
        .unwrap();
    backend
        .command(AgentCommand::Goal(GoalAction::Resume))
        .unwrap();
    backend.command(AgentCommand::Compact).unwrap();
    backend
        .command(AgentCommand::UpdatePermission {
            kind: "sandbox".into(),
            value: "readOnly".into(),
        })
        .unwrap();
    backend.command(AgentCommand::RefreshSkills).unwrap();
    backend
        .command(AgentCommand::OpenSession {
            id: None,
            model: "local".into(),
        })
        .unwrap();
    backend.command(AgentCommand::RefreshSessions).unwrap();
    let events = std::iter::from_fn(|| backend.poll().unwrap()).collect::<Vec<_>>();
    assert!(events.iter().any(|event|matches!(&event.update,AgentUpdate::SessionsLoaded{sessions,..} if sessions.len()==2)));
    assert_eq!(
        loom_core::sessions::Sessions::open(d.path())
            .unwrap()
            .list()
            .unwrap()
            .len(),
        2
    );
    backend.shutdown().unwrap();
}

#[test]
fn native_images_effort_followups_and_question_answers_reach_the_model() {
    isolate_model_state();
    let d = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(d.path())
        .status()
        .unwrap();
    fn response(name: &str, args: serde_json::Value) -> Response {
        Response {
            parts: vec![Part::ToolCall {
                id: name.into(),
                name: name.into(),
                arguments: args,
            }],
            usage: Usage::default(),
            stop: Stop::ToolUse,
            model: "scripted".into(),
            latency: Duration::ZERO,
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        }
    }
    let model = Arc::new(ScriptedModel::new([
        response(
            "request_user_input",
            json!({"questions":[{"id":"q","question":"which word?","options":["cobalt","amber"]}]}),
        ),
        response("finish", json!({"summary":"cobalt remembered"})),
        response("finish", json!({"summary":"answered follow-up"})),
    ]));
    let image_path = d.path().join("input.png");
    image::save_buffer_with_format(
        &image_path,
        &[255, 0, 0, 255],
        1,
        1,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .unwrap();
    let mut backend = NativeBackend::with_model(
        d.path(),
        model.clone(),
        ExecutionOptions {
            readonly: true,
            ..Default::default()
        },
    )
    .unwrap();
    while backend.poll().unwrap().is_some() {}
    backend
        .command(AgentCommand::Submit {
            text: "remember this image".into(),
            model: "local".into(),
            effort: Some("high".into()),
            skills: vec![],
            images: vec![loom_tui::attachment::ImageAttachment::from_path(&image_path).unwrap()],
        })
        .unwrap();
    let start = Instant::now();
    let mut answered = false;
    let mut completed = false;
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(event) = backend.poll().unwrap() {
            match event.update {
                AgentUpdate::InputRequested(request) => {
                    backend
                        .command(AgentCommand::Answer {
                            request_id: request.id,
                            answers: vec![QuestionAnswer {
                                question_id: "q".into(),
                                value: AnswerValue::Option(0),
                            }],
                        })
                        .unwrap();
                    answered = true;
                }
                AgentUpdate::TurnCompleted { status, .. } => {
                    assert_eq!(status, "completed");
                    completed = true;
                    break;
                }
                _ => {}
            }
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(answered && completed);
    assert_eq!(
        model.requests()[0].reasoning_effort.as_deref(),
        Some("high")
    );
    assert!(
        model.requests()[0]
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .any(|p| matches!(p,Part::Image{mime_type,..} if mime_type=="image/png"))
    );
    backend
        .command(AgentCommand::Submit {
            text: "what was the word?".into(),
            model: "local".into(),
            effort: None,
            skills: vec![],
            images: vec![],
        })
        .unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(event) = backend.poll().unwrap() {
            if matches!(event.update, AgentUpdate::TurnCompleted { .. }) {
                break;
            }
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(
        model
            .requests()
            .last()
            .unwrap()
            .messages
            .iter()
            .any(|m| m.text().contains("cobalt remembered"))
    );
    backend.shutdown().unwrap();
}
