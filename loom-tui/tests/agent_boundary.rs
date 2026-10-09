use loom_tui::{
    agent::{AgentBackend, AgentCommand, AgentEvent, AgentUpdate},
    app::App,
    core::{backend::CoreBackend, repository::RepositoryExplorer},
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

#[test]
#[cfg(unix)]
fn completed_turn_does_not_block_ui_on_slow_git_checkpoint() {
    use loom_tui::agent::{Permissions, SessionSnapshot};
    use std::{collections::VecDeque, process::Command};
    struct Script(VecDeque<AgentEvent>);
    impl AgentBackend for Script {
        fn command(&mut self, _: AgentCommand) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> anyhow::Result<Option<AgentEvent>> {
            Ok(self.0.pop_front())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "filter.loom-slow.clean", "sleep 1; cat"],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(directory.path())
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(
        directory.path().join(".gitattributes"),
        "slow.txt filter=loom-slow\n",
    )
    .unwrap();
    fs::write(directory.path().join("slow.txt"), "source\n").unwrap();
    let script = Script(VecDeque::from([
        AgentEvent::global(AgentUpdate::SessionLoaded {
            snapshot: SessionSnapshot {
                id: "main".into(),
                title: None,
                status: None,
                turns: None,
            },
            switching: true,
            model: None,
            permissions: Permissions::default(),
        }),
        AgentEvent::scoped(
            "main",
            AgentUpdate::TurnCompleted {
                id: Some("turn".into()),
                status: "completed".into(),
                error: None,
            },
        ),
        AgentEvent::global(AgentUpdate::Notice("input remains responsive".into())),
    ]));
    let mut core = CoreBackend::new(Box::new(script), directory.path(), true).unwrap();
    core.poll().unwrap();
    let start = Instant::now();
    assert!(matches!(
        core.poll().unwrap().unwrap().update,
        AgentUpdate::TurnCompleted { .. }
    ));
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "checkpoint blocked the input loop for {:?}",
        start.elapsed()
    );
    assert!(matches!(
        core.poll().unwrap().unwrap().update,
        AgentUpdate::Notice(_)
    ));
    let checkpoint = directory.path().join(".custom-tui/checkpoints.json");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !checkpoint.exists() && Instant::now() < deadline {
        core.poll().unwrap();
        thread::sleep(Duration::from_millis(10));
    }
    let saved: Value = serde_json::from_slice(&fs::read(checkpoint).unwrap()).unwrap();
    assert_eq!(saved["main"][0]["turn"], "turn");
    core.shutdown().unwrap();
}


#[test]
fn session_projection_keeps_previous_page_on_invalid_refresh() {
    use loom_tui::backend::codec;
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
        loom_tui::agent::TaskStatus::Completed
    );
    assert_eq!(
        app.tasks[0].worker,
        Some(loom_tui::agent::WorkerKind::Local)
    );
    assert!(app.tasks[0].elapsed_ms.is_some());
    assert!(app.tasks[0].outputs[0].starts_with("repo://"));
}

#[test]
fn silent_work_animates_and_stops_after_completion_or_approval() {
    use loom_tui::agent::{Approval, Choice};
    use loom_tui::engine::Action;
    let mut app = App::new(false);
    app.busy = true;
    app.status = "실행 중".into();
    let status = |app: &mut App| {
        let canvas = app.render(80, 24);
        let model = canvas
            .hits
            .iter()
            .find(|hit| hit.y > 2 && hit.action == Action::Command("/model".into()))
            .unwrap();
        canvas.cells[usize::from(model.y) * usize::from(canvas.width) + 2]
            .text
            .clone()
    };
    let first = status(&mut app);
    let now = Instant::now() + Duration::from_millis(120);
    assert!(app.tick(now));
    let next = status(&mut app);
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
    let waiting = app.render(80, 24);
    assert!(!app.tick(now + Duration::from_millis(240)));
    let next = app.render(80, 24);
    for y in 0..waiting.height {
        assert_eq!(waiting.plain_line(y), next.plain_line(y));
    }
    app.apply_event(AgentEvent::global(AgentUpdate::ApprovalResolved(
        "request".into(),
    )));
    app.apply_event(AgentEvent::global(AgentUpdate::TurnCompleted {
        id: None,
        status: "completed".into(),
        error: None,
    }));
    app.tick(now + Duration::from_millis(360));
    assert_eq!(status(&mut app), "✓");
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
fn core_records_phase_cost_for_each_remote_turn() {
    use loom_tui::agent::{Entry, Kind, TokenUsage};
    use std::collections::VecDeque;
    struct Scripted(VecDeque<AgentUpdate>);
    impl AgentBackend for Scripted {
        fn command(&mut self, _: AgentCommand) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> anyhow::Result<Option<AgentEvent>> {
            Ok(self
                .0
                .pop_front()
                .map(|u| AgentEvent::scoped("thread-1", u)))
        }
    }
    let tool = |id: &str, title: &str, body: &str| {
        AgentUpdate::EntryUpdated(Entry {
            id: id.into(),
            kind: Kind::Tool,
            title: title.into(),
            body: body.into(),
            status: "completed".into(),
            expanded: false,
        })
    };
    let usage = |input| {
        AgentUpdate::TokenUsageUpdated(TokenUsage {
            input: Some(input),
            ..TokenUsage::default()
        })
    };
    let script = VecDeque::from(vec![
        AgentUpdate::TurnStarted {
            id: Some("turn-1".into()),
        },
        tool(
            "a",
            "/bin/zsh -lc \"sed -n '1,200p' src/lib.rs\"",
            &"x".repeat(900),
        ),
        tool("b", "/bin/zsh -lc 'cargo test'", &"y".repeat(90)),
        usage(12_000),
        AgentUpdate::TurnCompleted {
            id: Some("turn-1".into()),
            status: "completed".into(),
            error: None,
        },
    ]);
    let directory = tempfile::tempdir().unwrap();
    let mut core = CoreBackend::new(Box::new(Scripted(script)), directory.path(), true).unwrap();
    while core.poll().unwrap().is_some() {}
    let cost = core.last_turn_cost().expect("turn closed");
    assert_eq!(cost.session.as_deref(), Some("thread-1"));
    assert!(cost.bytes.orient > 900 && cost.bytes.validate > 90 && cost.bytes.edit == 0);
    assert_eq!(cost.tokens.input, Some(12_000));
    let log = fs::read_to_string(core.telemetry_path()).unwrap();
    let record: Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_eq!(record["turn"], "turn-1");
    assert_eq!(record["commands"]["Orient"], 1);
}

#[test]
fn core_serves_a_revision_pinned_code_graph() {
    struct NoRemote;
    impl AgentBackend for NoRemote {
        fn command(&mut self, _: AgentCommand) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll(&mut self) -> anyhow::Result<Option<AgentEvent>> {
            Ok(None)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("auth.rs"),
        "fn refresh() {}\nfn login() { refresh(); }\n",
    )
    .unwrap();
    let core = CoreBackend::new(Box::new(NoRemote), directory.path(), true).unwrap();
    let graph = core.code_graph().unwrap();
    let callers: Vec<_> = graph
        .referrers("auth.rs::refresh")
        .into_iter()
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(callers, ["auth.rs::login"]);
    assert!(
        Arc::ptr_eq(&graph, &core.code_graph().unwrap()),
        "unchanged revision reuses the graph"
    );
}
