use anyhow::{Result, ensure};
use loom_tui::{
    agent::*,
    backend::{
        version_core::{Settings, Version},
        versions::VersionBackend,
    },
};
use loom_core::models::{self as m, ModelPort};
use serde_json::json;
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

fn fixture() -> Result<tempfile::TempDir> {
    let root = tempfile::tempdir()?;
    ensure!(std::process::Command::new("git").args(["init", "-q"]).arg(root.path())
        .status()?.success(), "git init failed");
    fs::create_dir_all(root.path().join("src"))?;
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
    )?;
    fs::write(
        root.path().join("src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )?;
    Ok(root)
}
fn script(_version: Version) -> Arc<m::ScriptedModel> {
    let tools = vec![
        m::Part::ToolCall {
            id: "read".into(),
            name: "open".into(),
            arguments: json!({"target":"src/lib.rs"}),
        },
        m::Part::ToolCall {
            id: "edit".into(),
            name: "change".into(),
            arguments: json!({"op":"edit","target":"src/lib.rs","old":"{ 1 }","new":"{ 2 }"}),
        },
        m::Part::ToolCall {
            id: "finish".into(),
            name: "finish".into(),
            arguments: json!({"summary":"value corrected"}),
        },
    ];
    Arc::new(m::ScriptedModel::new([m::Response {
        parts: tools,
        usage: m::Usage {
            input: Some(31),
            output: Some(5),
            cached_input: None,
        },
        stop: m::Stop::ToolUse,
        model: "fixture-model".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    }]))
}
fn command() -> AgentCommand {
    AgentCommand::Submit {
        text: ":edit fix value in src/lib.rs".into(),
        model: "fixed-model".into(),
        effort: None,
        images: vec![],
        skills: vec![],
    }
}
fn wait(
    backend: &mut VersionBackend,
    predicate: impl Fn(&AgentUpdate) -> bool,
) -> Result<AgentEvent> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = backend.poll()? {
            if predicate(&event.update) {
                return Ok(event);
            }
            // The real TUI drains queued events in batches. Sleeping after every
            // existing event adds artificial latency to interruption acknowledgements.
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
        ensure!(
            Instant::now() < until,
            "backend timed out waiting for expected event"
        );
    }
}
fn settings(state: &tempfile::TempDir) -> Settings {
    Settings {
        state: Some(state.path().to_owned()),
        checks: vec![if cfg!(windows) {
            "findstr /c:\"{ 2 }\" src\\lib.rs >nul"
        } else {
            "grep -q '{ 2 }' src/lib.rs"
        }
        .into()],
        steps: 2,
        attempts: 1,
        ..Default::default()
    }
}

#[test]
fn loom_projects_diff_into_chat_and_restores_it() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let options = settings(&state);
    let mut backend = VersionBackend::with_model_and_session(
        Version::Loom,
        root.path(),
        script(Version::Loom),
        options.clone(),
        Some("new".into()),
    )?;
    let event = wait(&mut backend, |u| {
        matches!(u, AgentUpdate::SessionLoaded { .. })
    })?;
    let AgentUpdate::SessionLoaded { snapshot, .. } = event.update else {
        unreachable!()
    };
    let session_id = snapshot.id;

    backend.command(command())?;
    let event = wait(
        &mut backend,
        |u| matches!(u, AgentUpdate::EntryUpdated(entry) if entry.kind == Kind::Change),
    )?;
    let AgentUpdate::EntryUpdated(change) = event.update else {
        unreachable!()
    };
    assert!(change.body.contains("src/lib.rs"), "{}", change.body);
    assert!(change.body.contains("{ 1 }") && change.body.contains("{ 2 }"));

    let event = wait(&mut backend, |u| {
        matches!(u, AgentUpdate::ApprovalRequested(_))
    })?;
    let AgentUpdate::ApprovalRequested(approval) = event.update else {
        unreachable!()
    };
    backend.command(AgentCommand::Reply {
        request_id: approval.id,
        choice: "accept".into(),
    })?;
    wait(
        &mut backend,
        |u| matches!(u, AgentUpdate::TurnCompleted { status, .. } if status == "completed"),
    )?;
    backend.shutdown()?;

    let mut restored = VersionBackend::with_model_and_session(
        Version::Loom,
        root.path(),
        Arc::new(m::ScriptedModel::new([])),
        options,
        Some(session_id),
    )?;
    let event = wait(&mut restored, |u| {
        matches!(u, AgentUpdate::SessionLoaded { .. })
    })?;
    let AgentUpdate::SessionLoaded { snapshot, .. } = event.update else {
        unreachable!()
    };
    let restored_change = snapshot
        .turns
        .unwrap_or_default()
        .into_iter()
        .flat_map(|turn| turn.entries)
        .find(|entry| entry.kind == Kind::Change)
        .expect("restored Loom turn should include its diff entry");
    assert!(restored_change.body.contains("src/lib.rs"));
    restored.shutdown()?;
    Ok(())
}

#[test]
fn version_backends_preserve_source_until_matching_approval_and_restore_pending() -> Result<()> {
    for version in [Version::Loom] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let model = script(version);
        let options = settings(&state);
        let mut backend =
            VersionBackend::with_model(version, root.path(), model.clone(), options.clone())?;
        backend.command(command())?;
        let event = wait(&mut backend, |u| {
            matches!(u, AgentUpdate::ApprovalRequested(_))
        })?;
        let AgentUpdate::ApprovalRequested(approval) = event.update else {
            unreachable!()
        };
        assert_eq!(approval.choices.len(), 2);
        assert_eq!(
            fs::read_to_string(root.path().join("src/lib.rs"))?,
            "pub fn value() -> u32 { 1 }\n"
        );
        assert_eq!(model.requests()[0].model, "fixed-model");
        assert!(
            backend
                .command(AgentCommand::Reply {
                    request_id: "wrong".into(),
                    choice: "accept".into()
                })
                .is_err()
        );
        backend.shutdown()?;
        let mut restored = VersionBackend::with_model(
            version,
            root.path(),
            Arc::new(m::ScriptedModel::new([])),
            options,
        )?;
        let event = wait(&mut restored, |u| {
            matches!(u, AgentUpdate::ApprovalRequested(_))
        })?;
        let AgentUpdate::ApprovalRequested(pending) = event.update else {
            unreachable!()
        };
        assert_eq!(pending.id, approval.id);
        restored.command(AgentCommand::Reply {
            request_id: pending.id,
            choice: "accept".into(),
        })?;
        wait(
            &mut restored,
            |u| matches!(u,AgentUpdate::TurnCompleted{status,..} if status=="completed"),
        )?;
        assert_eq!(
            fs::read_to_string(root.path().join("src/lib.rs"))?,
            "pub fn value() -> u32 { 2 }\n"
        );
        restored.shutdown()?;
    }
    Ok(())
}

#[test]
fn stale_approval_keeps_new_user_bytes_and_can_still_be_rejected() -> Result<()> {
    for version in [Version::Loom] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let mut backend =
            VersionBackend::with_model(version, root.path(), script(version), settings(&state))?;
        backend.command(command())?;
        let event = wait(&mut backend, |u| {
            matches!(u, AgentUpdate::ApprovalRequested(_))
        })?;
        let AgentUpdate::ApprovalRequested(approval) = event.update else {
            unreachable!()
        };
        fs::write(root.path().join("src/lib.rs"), "pub fn user_changed() {}\n")?;
        backend.command(AgentCommand::Reply {
            request_id: approval.id.clone(),
            choice: "accept".into(),
        })?;
        wait(&mut backend, |u| {
            matches!(u, AgentUpdate::ApprovalReplyFailed { .. })
        })?;
        assert_eq!(
            fs::read_to_string(root.path().join("src/lib.rs"))?,
            "pub fn user_changed() {}\n"
        );
        backend.command(AgentCommand::Reply {
            request_id: approval.id,
            choice: "reject".into(),
        })?;
        wait(&mut backend, |u| {
            matches!(u, AgentUpdate::ApprovalResolved(_))
        })?;
        backend.shutdown()?;
    }
    Ok(())
}

struct Blocking {
    entered: Arc<AtomicUsize>,
}
impl ModelPort for Blocking {
    fn complete(
        &self,
        _: m::Need,
        r: &m::Request,
    ) -> std::result::Result<m::Served, m::ProviderError> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(3);
        while !r.cancel.as_ref().unwrap().load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(m::ProviderError::new(
            m::ErrorKind::Cancelled,
            "cancelled fixture request",
        ))
    }
}

struct StalledStream {
    entered: Arc<AtomicUsize>,
}
impl ModelPort for StalledStream {
    fn complete(
        &self,
        _: m::Need,
        _: &m::Request,
    ) -> std::result::Result<m::Served, m::ProviderError> {
        unreachable!("Loom should use the streaming model path")
    }

    fn complete_stream(
        &self,
        _: m::Need,
        _: &m::Request,
        _: &mut dyn FnMut(m::StreamEvent),
    ) -> std::result::Result<m::Served, m::ProviderError> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_secs(1));
        Err(m::ProviderError::new(
            m::ErrorKind::Cancelled,
            "stalled fixture request eventually returned",
        ))
    }
}

#[test]
fn loom_interrupt_does_not_wait_for_a_stalled_stream_read() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let entered = Arc::new(AtomicUsize::new(0));
    let mut backend = VersionBackend::with_model(
        Version::Loom,
        root.path(),
        Arc::new(StalledStream {
            entered: entered.clone(),
        }),
        settings(&state),
    )?;
    backend.command(command())?;
    let until = Instant::now() + Duration::from_secs(3);
    while entered.load(Ordering::SeqCst) == 0 {
        let _ = backend.poll()?;
        ensure!(
            Instant::now() < until,
            "streaming model request did not start"
        );
        std::thread::sleep(Duration::from_millis(3));
    }

    let interrupted_at = Instant::now();
    backend.command(AgentCommand::Interrupt)?;
    wait(
        &mut backend,
        |u| matches!(u,AgentUpdate::TurnCompleted{status,..} if status=="interrupted"),
    )?;
    assert!(
        interrupted_at.elapsed() < Duration::from_millis(250),
        "Loom interrupt waited {:?} for a stalled stream read",
        interrupted_at.elapsed()
    );
    backend.shutdown()?;
    Ok(())
}

#[test]
fn interrupt_is_nonblocking_and_preserves_source_across_versions() -> Result<()> {
    for version in [Version::Loom] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let entered = Arc::new(AtomicUsize::new(0));
        let mut backend = VersionBackend::with_model(
            version,
            root.path(),
            Arc::new(Blocking {
                entered: entered.clone(),
            }),
            settings(&state),
        )?;
        backend.command(command())?;
        let until = Instant::now() + Duration::from_secs(3);
        while entered.load(Ordering::SeqCst) == 0 {
            let _ = backend.poll()?;
            ensure!(Instant::now() < until, "model request did not start");
            std::thread::sleep(Duration::from_millis(3));
        }
        let before = Instant::now();
        backend.command(AgentCommand::Interrupt)?;
        assert!(before.elapsed() < Duration::from_millis(100));
        wait(
            &mut backend,
            |u| matches!(u,AgentUpdate::TurnCompleted{status,..} if status=="interrupted"),
        )?;
        assert_eq!(entered.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read_to_string(root.path().join("src/lib.rs"))?,
            "pub fn value() -> u32 { 1 }\n"
        );
        backend.shutdown()?;
    }
    Ok(())
}

#[test]
fn readonly_backend_rejects_edit_before_model() -> Result<()> {
    for version in [Version::Loom] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let model = script(version);
        let mut options = settings(&state);
        options.readonly = true;
        let mut backend = VersionBackend::with_model(version, root.path(), model.clone(), options)?;
        assert!(backend.command(command()).is_err());
        assert!(model.requests().is_empty());
        backend.shutdown()?;
    }
    Ok(())
}

#[test]
fn loom_projects_native_ast_operations_with_readable_names() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let response = |name: &str, arguments| m::Response {
        parts: vec![m::Part::ToolCall {
            id: name.into(),
            name: name.into(),
            arguments,
        }],
        usage: Default::default(),
        stop: m::Stop::ToolUse,
        model: "fixture-model".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    };
    let model = Arc::new(m::ScriptedModel::new([
        response("find", json!({"query":"value","kind":"symbol"})),
        response("open", json!({"target":"src/lib.rs::value"})),
        response(
            "finish",
            json!({"summary":"value is defined in src/lib.rs"}),
        ),
    ]));
    let mut options = settings(&state);
    options.steps = 1;
    let mut backend = VersionBackend::with_model(Version::Loom, root.path(), model, options)?;
    backend.update_execution_budget(4, 300);
    backend.command(AgentCommand::Submit {
        text: ":inspect inspect value".into(),
        model: "fixture-model".into(),
        effort: None,
        images: vec![],
        skills: vec![],
    })?;

    let search = wait(&mut backend, |u| {
        matches!(u, AgentUpdate::EntryUpdated(entry)
            if entry.kind == Kind::Tool
                && entry.title == "find · value"
                && entry.status == "inProgress")
    })?;
    let AgentUpdate::EntryUpdated(search) = search.update else {
        unreachable!()
    };
    assert!(!search.title.contains("AST"), "{}", search.title);
    assert!(!search.body.contains("\"args\""), "{}", search.body);
    let trace_id = search.id.clone();
    let completed_search = wait(&mut backend, |u| {
        matches!(u, AgentUpdate::EntryUpdated(entry)
            if entry.id == trace_id && entry.status == "completed"
                && entry.body.contains("검색 · value") && entry.body.contains("count=1"))
    })?;
    let AgentUpdate::EntryUpdated(completed_search) = completed_search.update else {
        unreachable!()
    };
    assert!(!completed_search.body.contains("operation_id"), "{}", completed_search.body);

    let source = wait(&mut backend, |u| {
        matches!(u, AgentUpdate::EntryUpdated(entry)
            if entry.kind == Kind::Tool
                && entry.title == "open · src/lib.rs::value"
                && entry.status == "inProgress")
    })?;
    let AgentUpdate::EntryUpdated(source) = source.update else {
        unreachable!()
    };
    assert!(!source.title.contains("AST"), "{}", source.title);
    assert!(!source.body.contains("\"source\""), "{}", source.body);
    wait(&mut backend, |u| {
        matches!(u, AgentUpdate::TurnCompleted { .. })
    })?;
    backend.shutdown()?;
    Ok(())
}

#[test]
fn loom_warms_ast_index_without_model_or_busy_noise() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let model = Arc::new(m::ScriptedModel::new([]));
    let mut backend =
        VersionBackend::with_model(Version::Loom, root.path(), model.clone(), settings(&state))?;
    let _ = wait(&mut backend, |u| {
        matches!(u, AgentUpdate::SessionLoaded { .. })
    })?;
    let until = Instant::now() + Duration::from_millis(250);
    while Instant::now() < until {
        if let Some(event) = backend.poll()? {
            if let AgentUpdate::Notice(text) = event.update {
                assert!(
                    !text.contains("AST 인덱스 준비됨"),
                    "successful AST warmup should stay quiet: {text}"
                );
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        model.requests().is_empty(),
        "AST warmup must not call a model"
    );
    backend.shutdown()?;
    Ok(())
}

#[test]
fn loom_session_survives_backend_restart() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let options = settings(&state);
    let model = Arc::new(m::ScriptedModel::new([]));
    let mut backend =
        VersionBackend::with_model(Version::Loom, root.path(), model.clone(), options.clone())?;
    let loaded = wait(&mut backend, |u| {
        matches!(u, AgentUpdate::SessionLoaded { .. })
    })?;
    let AgentUpdate::SessionLoaded { snapshot, .. } = loaded.update else {
        unreachable!()
    };
    let initial = snapshot.id;
    backend.command(AgentCommand::OpenSession {
        id: None,
        model: "auto".into(),
    })?;
    let created = wait(&mut backend, |u| {
        matches!(u, AgentUpdate::SessionLoaded { .. })
    })?;
    let AgentUpdate::SessionLoaded { snapshot, .. } = created.update else {
        unreachable!()
    };
    let created_id = snapshot.id;
    assert_ne!(created_id, initial);
    backend.shutdown()?;

    let mut restored = VersionBackend::with_model_and_session(
        Version::Loom,
        root.path(),
        Arc::new(m::ScriptedModel::new([])),
        options,
        Some(created_id.clone()),
    )?;
    let loaded = wait(&mut restored, |u| {
        matches!(u, AgentUpdate::SessionLoaded { .. })
    })?;
    let AgentUpdate::SessionLoaded { snapshot, .. } = loaded.update else {
        unreachable!()
    };
    assert_eq!(snapshot.id, created_id);
    restored.shutdown()?;
    Ok(())
}

#[test]
#[cfg(unix)]
fn followup_history_loading_does_not_block_terminal_commands() -> Result<()> {
    use std::{io::Write, os::unix::ffi::OsStrExt};
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let response = m::Response { parts: vec![m::Part::Text("remember this".into())], usage: Default::default(), stop: m::Stop::EndTurn,
        model: "fixture-model".into(), latency: Duration::ZERO, route: None, gateway_connection: None, gateway_upstream_ms: None };
    let model = Arc::new(m::ScriptedModel::new([response.clone(), response]));
    let mut backend = VersionBackend::with_model(Version::Loom, root.path(), model, settings(&state))?;
    let submit = || AgentCommand::Submit { text: "hello".into(), model: "fixture-model".into(), effort: None, images: vec![], skills: vec![] };
    backend.command(submit())?;
    wait(&mut backend, |event| matches!(event, AgentUpdate::TurnCompleted { .. }))?;
    let log = fs::read_dir(root.path().join(".loom/tasks"))?.filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.to_string_lossy().ends_with(".events.jsonl")).unwrap();
    let contents = fs::read(&log)?;
    fs::remove_file(&log)?;
    let fifo = std::ffi::CString::new(log.as_os_str().as_bytes())?;
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        let mut file = fs::OpenOptions::new().write(true).open(log).unwrap();
        file.write_all(&contents).unwrap();
    });
    let started = Instant::now();
    backend.command(submit())?;
    assert!(started.elapsed() < Duration::from_millis(250), "history blocked input for {:?}", started.elapsed());
    wait(&mut backend, |event| matches!(event, AgentUpdate::TurnCompleted { .. }))?;
    writer.join().unwrap();
    backend.shutdown()?;
    Ok(())
}

#[test]
fn loom_followup_receives_prior_session_goal_answer_and_tool_activity() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let response = |parts| m::Response {
        parts,
        usage: Default::default(),
        stop: m::Stop::ToolUse,
        model: "fixture-model".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    };
    let model = Arc::new(m::ScriptedModel::new([
        response(vec![
            m::Part::ToolCall {
                id: "read-first".into(),
                name: "open".into(),
                arguments: json!({"target":"src/lib.rs"}),
            },
            m::Part::ToolCall {
                id: "finish-first".into(),
                name: "finish".into(),
                arguments: json!({"summary":"value is currently one"}),
            },
        ]),
        response(vec![m::Part::ToolCall {
            id: "finish-followup".into(),
            name: "finish".into(),
            arguments: json!({"summary":"continued from the previous turn"}),
        }]),
    ]));
    let mut backend =
        VersionBackend::with_model(Version::Loom, root.path(), model.clone(), settings(&state))?;
    backend.command(AgentCommand::Submit {
        text: ":inspect inspect value and remember the finding".into(),
        model: "fixture-model".into(),
        effort: None,
        images: vec![],
        skills: vec![],
    })?;
    wait(&mut backend, |u| {
        matches!(u, AgentUpdate::TurnCompleted { .. })
    })?;

    backend.command(AgentCommand::Submit {
        text: "계속해봐".into(),
        model: "fixture-model".into(),
        effort: None,
        images: vec![],
        skills: vec![],
    })?;
    wait(&mut backend, |u| {
        matches!(u, AgentUpdate::TurnCompleted { .. })
    })?;

    let requests = model.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    let followup = serde_json::to_string(&requests[1].messages)?;
    assert!(
        followup.contains("inspect value and remember the finding"),
        "prior user goal missing from follow-up context: {followup}"
    );
    assert!(
        followup.contains("value is currently one"),
        "prior assistant outcome missing from follow-up context: {followup}"
    );
    assert!(
        followup.contains("src/lib.rs") && followup.contains("\\\"tool\\\":\\\"open\\\""),
        "prior observed tool activity missing from follow-up context: {followup}"
    );
    backend.shutdown()?;
    Ok(())
}

#[test]
fn session_control_allows_new_switch_and_list_without_blocking() -> Result<()> {
    for version in [Version::Loom] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let model = script(version);
        let mut backend =
            VersionBackend::with_model(version, root.path(), model, settings(&state))?;

        // 1. Initial SessionLoaded must be global so app.thread can be set
        let loaded = wait(&mut backend, |u| {
            matches!(u, AgentUpdate::SessionLoaded { .. })
        })?;
        assert!(
            loaded.session_id.is_none(),
            "SessionLoaded must be global to set initial app.thread"
        );
        let AgentUpdate::SessionLoaded { snapshot, .. } = loaded.update else {
            unreachable!()
        };
        let initial_id = snapshot.id.clone();

        // 2. RefreshSessions loads the session list
        backend.command(AgentCommand::RefreshSessions)?;
        let sessions_evt = wait(&mut backend, |u| {
            matches!(u, AgentUpdate::SessionsLoaded { .. })
        })?;
        assert!(sessions_evt.session_id.is_none());
        let AgentUpdate::SessionsLoaded { sessions, .. } = sessions_evt.update else {
            unreachable!()
        };
        assert!(sessions.iter().any(|s| s.id == initial_id));

        // 3. OpenSession(None) creates a new session (/new)
        backend.command(AgentCommand::OpenSession {
            id: None,
            model: "auto".into(),
        })?;
        let new_loaded = wait(&mut backend, |u| {
            matches!(u, AgentUpdate::SessionLoaded { .. })
        })?;
        assert!(new_loaded.session_id.is_none());
        let AgentUpdate::SessionLoaded {
            snapshot: new_snap, ..
        } = new_loaded.update
        else {
            unreachable!()
        };
        assert_ne!(new_snap.id, initial_id, "New session must have distinct id");
        assert_eq!(new_snap.title.as_deref(), Some("새 대화"));
        assert_eq!(new_snap.turns.as_ref().map(|t| t.len()), Some(0));

        // 4. OpenSession(Some(initial_id)) switches back
        backend.command(AgentCommand::OpenSession {
            id: Some(initial_id.clone()),
            model: "auto".into(),
        })?;
        let switched = wait(&mut backend, |u| {
            matches!(u, AgentUpdate::SessionLoaded { .. })
        })?;
        let AgentUpdate::SessionLoaded {
            snapshot: switched_snap,
            ..
        } = switched.update
        else {
            unreachable!()
        };
        assert_eq!(switched_snap.id, initial_id);

        backend.shutdown()?;
    }
    Ok(())
}

#[test]
fn interrupt_during_approval_resolves_the_turn_without_applying() -> Result<()> {
    for version in [Version::Loom] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let marker = tempfile::tempdir()?;
        let pause = marker.path().join("pause");
        let mut options = settings(&state);
        options.checks = vec![if cfg!(windows) {
            format!(
                "if exist \"{}\" (ping -n 4 127.0.0.1 >nul) & findstr /c:\"{{ 2 }}\" src\\lib.rs >nul",
                pause.display()
            )
        } else {
            format!(
                "if test -f '{}'; then sleep 3; fi; grep -q '{{ 2 }}' src/lib.rs",
                pause.display()
            )
        }];
        let mut backend =
            VersionBackend::with_model(version, root.path(), script(version), options)?;
        backend.command(command())?;
        let event = wait(&mut backend, |u| {
            matches!(u, AgentUpdate::ApprovalRequested(_))
        })?;
        let AgentUpdate::ApprovalRequested(approval) = event.update else {
            unreachable!()
        };
        fs::write(pause, "pause approval")?;
        backend.command(AgentCommand::Reply {
            request_id: approval.id,
            choice: "accept".into(),
        })?;
        std::thread::sleep(Duration::from_millis(150));
        backend.command(AgentCommand::Interrupt)?;
        wait(
            &mut backend,
            |u| matches!(u,AgentUpdate::TurnCompleted{status,..} if status=="interrupted"),
        )?;
        assert_eq!(
            fs::read_to_string(root.path().join("src/lib.rs"))?,
            "pub fn value() -> u32 { 1 }\n"
        );
        backend.shutdown()?;
    }
    Ok(())
}

#[test]
fn conversational_outcome_and_reported_usage_survive_restart() -> Result<()> {
    use loom_tui::backend::version_core::{self, Intent};
    for version in [Version::Loom] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let options = Settings {
            model: "fixture-model".into(),
            ..settings(&state)
        };
        let model = Arc::new(m::ScriptedModel::new([m::Response {
            parts: vec![m::Part::Text("안녕하세요".into())],
            usage: m::Usage {
                input: Some(31),
                output: Some(5),
                cached_input: None,
            },
            stop: m::Stop::EndTurn,
            model: "fixture".into(),
            latency: Duration::ZERO,
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        }]));
        let outcome = version_core::execute(
            version,
            root.path(),
            "안녕",
            Intent::Answer,
            options.clone(),
            model,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(|_| {}),
        )?;
        let restored = version_core::history(version, root.path(), &options)?;
        let chat = restored
            .iter()
            .find(|o| o.id == outcome.id)
            .expect("completed conversation was lost on restart");
        assert_eq!(chat.input, Some(31));
        assert_eq!(chat.cached, None);
        assert_eq!(chat.summary, "안녕하세요");
        let other = fixture()?;
        assert!(
            !version_core::history(version, other.path(), &options)?
                .iter()
                .any(|o| o.id == outcome.id)
        );
    }
    Ok(())
}

#[test]
fn initial_permission_display_matches_enforced_policy() -> Result<()> {
    use loom_core::effects::{ExecutionPolicy, SandboxMode};
    for version in [Version::Loom] {
        for (sandbox, readonly, expected) in [
            (SandboxMode::ReadOnly, false, "readOnly"),
            (SandboxMode::FullAccess, false, "dangerFullAccess"),
            (SandboxMode::WorkspaceWrite, true, "readOnly"),
        ] {
            let root = fixture()?;
            let state = tempfile::tempdir()?;
            let options = Settings {
                readonly,
                policy: ExecutionPolicy {
                    sandbox,
                    network: false,
                },
                ..settings(&state)
            };
            let mut backend = VersionBackend::with_model(
                version,
                root.path(),
                Arc::new(m::ScriptedModel::new([])),
                options,
            )?;
            let event = wait(&mut backend, |u| {
                matches!(u, AgentUpdate::SessionLoaded { .. })
            })?;
            let AgentUpdate::SessionLoaded { permissions, .. } = event.update else {
                unreachable!()
            };
            assert_eq!(permissions.sandbox.as_deref(), Some(expected));
            if expected == "readOnly" {
                assert!(backend.command(command()).is_err());
            }
            backend.shutdown()?;
        }
    }
    Ok(())
}

#[test]
fn compact_replaces_earlier_turns_with_a_model_summary() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let response = |parts, stop| m::Response {
        parts,
        usage: Default::default(),
        stop,
        model: "fixture-model".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    };
    let model = Arc::new(m::ScriptedModel::new([
        response(vec![m::Part::ToolCall {
            id: "finish-first".into(),
            name: "finish".into(),
            arguments: json!({"summary":"FIRST_TURN_RECEIPT"}),
        }], m::Stop::ToolUse),
        response(vec![m::Part::Text("COMPACT_SUMMARY: the value is one".into())], m::Stop::EndTurn),
        response(vec![m::Part::ToolCall {
            id: "finish-next".into(),
            name: "finish".into(),
            arguments: json!({"summary":"continued"}),
        }], m::Stop::ToolUse),
    ]));
    let mut backend =
        VersionBackend::with_model(Version::Loom, root.path(), model.clone(), settings(&state))?;
    let submit = |text: &str| AgentCommand::Submit {
        text: text.into(),
        model: "fixture-model".into(),
        effort: None,
        images: vec![],
        skills: vec![],
    };
    backend.command(submit(":inspect FIRST_GOAL inspect value"))?;
    wait(&mut backend, |u| matches!(u, AgentUpdate::TurnCompleted { .. }))?;
    backend.command(AgentCommand::Compact)?;
    wait(&mut backend, |u| matches!(u, AgentUpdate::Notice(text) if text.contains("요약했습니다")))?;
    backend.command(submit(":inspect 계속해봐"))?;
    wait(&mut backend, |u| matches!(u, AgentUpdate::TurnCompleted { .. }))?;
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    let next = serde_json::to_string(&requests[2].messages)?;
    assert!(next.contains("COMPACT_SUMMARY"), "summary must open the history: {next}");
    assert!(!next.contains("FIRST_TURN_RECEIPT"), "compacted turns are not resent: {next}");
    backend.shutdown()?;
    Ok(())
}

#[test]
fn long_history_is_compacted_automatically_before_the_next_request() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let response = |parts, stop| m::Response {
        parts,
        usage: Default::default(),
        stop,
        model: "fixture-model".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    };
    // An answer far larger than the history a request may carry.
    let long = format!("FIRST_TURN_RECEIPT {}", "detail ".repeat(6000));
    // A small window: the first turn alone is more history than the next request may carry.
    let model = Arc::new(m::ScriptedModel::with_context("fixture-model", 12_000, [
        response(vec![m::Part::ToolCall {
            id: "finish-first".into(),
            name: "finish".into(),
            arguments: json!({"summary": long}),
        }], m::Stop::ToolUse),
        response(vec![m::Part::Text("AUTO_SUMMARY: the value is one".into())], m::Stop::EndTurn),
        response(vec![m::Part::ToolCall {
            id: "finish-next".into(),
            name: "finish".into(),
            arguments: json!({"summary":"continued"}),
        }], m::Stop::ToolUse),
    ]));
    let mut backend =
        VersionBackend::with_model(Version::Loom, root.path(), model.clone(), settings(&state))?;
    let submit = |text: &str| AgentCommand::Submit {
        text: text.into(),
        model: "fixture-model".into(),
        effort: None,
        images: vec![],
        skills: vec![],
    };
    backend.command(submit(":inspect FIRST_GOAL inspect value"))?;
    wait(&mut backend, |u| matches!(u, AgentUpdate::TurnCompleted { .. }))?;
    backend.command(submit(":inspect SECOND_GOAL 계속해봐"))?;
    wait(&mut backend, |u| matches!(u, AgentUpdate::CompressionState { running: true, .. }))?;
    wait(&mut backend, |u| matches!(u, AgentUpdate::Notice(text) if text.contains("자동으로 요약")))?;
    wait(&mut backend, |u| matches!(u, AgentUpdate::TurnCompleted { .. }))?;
    let requests = model.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    let summary_request = serde_json::to_string(&requests[1].messages)?;
    assert!(summary_request.contains("FIRST_TURN_RECEIPT"), "the summary reads the earlier turn");
    let next = serde_json::to_string(&requests[2].messages)?;
    assert!(next.contains("AUTO_SUMMARY"), "summary must open the history: {next}");
    assert!(!next.contains("FIRST_TURN_RECEIPT"), "folded turns are not resent");
    assert!(next.contains("SECOND_GOAL"));
    backend.shutdown()?;
    Ok(())
}

#[test]
fn rewind_restores_code_and_conversation_to_an_earlier_reply() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let response = |parts: Vec<m::Part>| m::Response {
        parts,
        usage: Default::default(),
        stop: m::Stop::ToolUse,
        model: "fixture-model".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    };
    let edit = |old: &str, new: &str, id: &str| {
        response(vec![
            m::Part::ToolCall { id: format!("open-{id}"), name: "open".into(), arguments: json!({"target":"src/lib.rs"}) },
            m::Part::ToolCall {
                id: format!("edit-{id}"),
                name: "change".into(),
                arguments: json!({"op":"edit","target":"src/lib.rs","old":old,"new":new}),
            },
            m::Part::ToolCall { id: format!("finish-{id}"), name: "finish".into(), arguments: json!({"summary":format!("value {new}")}) },
        ])
    };
    let model = Arc::new(m::ScriptedModel::new([edit("{ 1 }", "{ 2 }", "a"), edit("{ 2 }", "{ 3 }", "b")]));
    let options = Settings { checks: vec![if cfg!(windows) { "exit /b 0" } else { "true" }.into()], ..settings(&state) };
    let mut backend = VersionBackend::with_model(Version::Loom, root.path(), model, options)?;
    let mut turns = Vec::new();
    for text in ["change the value to two", "change the value to three"] {
        backend.command(AgentCommand::Submit {
            text: text.into(),
            model: "fixture-model".into(),
            effort: None,
            images: vec![],
            skills: vec![],
        })?;
        let event = wait(&mut backend, |u| matches!(u, AgentUpdate::TurnStarted { id: Some(_) }))?;
        let AgentUpdate::TurnStarted { id: Some(turn) } = event.update else { unreachable!() };
        turns.push(turn);
        let event = wait(&mut backend, |u| matches!(u, AgentUpdate::ApprovalRequested(_)))?;
        let AgentUpdate::ApprovalRequested(approval) = event.update else { unreachable!() };
        backend.command(AgentCommand::Reply { request_id: approval.id, choice: "accept".into() })?;
        wait(&mut backend, |u| matches!(u, AgentUpdate::TurnCompleted { status, .. } if status == "completed"))?;
        // The checkpoint follows completion from the approval worker.
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            while backend.poll()?.is_some() {}
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(fs::read_to_string(root.path().join("src/lib.rs"))?.contains("{ 3 }"));
    backend.command(AgentCommand::PreviewRewind { turn_id: turns[0].clone() })?;
    let event = wait(&mut backend, |u| matches!(u, AgentUpdate::RewindPreview { .. }))?;
    let AgentUpdate::RewindPreview { files, blocked, .. } = event.update else { unreachable!() };
    assert_eq!(blocked, None);
    assert_eq!(files, 1);
    backend.command(AgentCommand::Rewind { turn_id: turns[0].clone(), model: "fixture-model".into() })?;
    wait(&mut backend, |u| matches!(u, AgentUpdate::Notice(text) if text.contains("되돌렸습니다")))?;
    assert!(fs::read_to_string(root.path().join("src/lib.rs"))?.contains("{ 2 }"));
    backend.shutdown()?;
    Ok(())
}
