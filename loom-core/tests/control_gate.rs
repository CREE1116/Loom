mod isolation;
use loom_core::{
    control::{Core, ExecutionOptions, TaskIntent, TaskStatus},
    models::{Part, Response, ScriptedModel, Stop, Usage},
};
use serde_json::json;
use std::{fs, process::Command, sync::atomic::AtomicBool, time::Duration};
fn check_command(unix: &str, windows: &str) -> String {
    if cfg!(windows) { windows } else { unix }.to_owned()
}

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    Command::new("git")
        .current_dir(d.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    fs::write(d.path().join("file.txt"), "old\n").unwrap();
    d
}
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
fn model() -> ScriptedModel {
    ScriptedModel::new([
        response("read", json!({"path":"file.txt"})),
        response("edit", json!({"path":"file.txt","old":"old","new":"new"})),
        response("finish", json!({"summary":"ready"})),
    ])
}

#[test]
fn core_automatically_dispatches_independent_graph_groups_and_validates_the_merged_patch() {
    use loom_core::models::{ModelPort, Need, ProviderError, Request, Served};
    struct Port {
        left: ScriptedModel,
        right: ScriptedModel,
        arrivals: std::sync::atomic::AtomicUsize,
    }
    impl ModelPort for Port {
        fn complete(&self, need: Need, request: &Request) -> Result<Served, ProviderError> {
            assert_eq!(
                request.model, "pinned-model",
                "graph worker lost the selected model"
            );
            assert!(
                request
                    .system
                    .as_deref()
                    .is_some_and(|system| system.contains("Owned write files:")),
                "scope was only a user-message hint, missing from worker system contract"
            );
            assert!(
                !request.tools.iter().any(|tool| tool.name == "check"),
                "automatic children must not test the unmerged whole task"
            );
            if let Some(enable) = request
                .tools
                .iter()
                .find(|tool| tool.name == "enable_tools")
            {
                let groups = enable.parameters["properties"]["groups"]["items"]["enum"]
                    .as_array()
                    .unwrap();
                assert!(
                    !groups
                        .iter()
                        .any(|group| group == "agents" || group == "terminal"),
                    "automatic child was offered unsupported delegation or unbounded commands"
                );
            }
            self.arrivals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while self.arrivals.load(std::sync::atomic::Ordering::SeqCst) < 2
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(
                self.arrivals.load(std::sync::atomic::Ordering::SeqCst) >= 2,
                "independent workers were serialized"
            );
            if request
                .messages
                .iter()
                .any(|message| message.text().contains("Owned write files: left.rs"))
            {
                self.left.complete(need, request)
            } else if request
                .messages
                .iter()
                .any(|message| message.text().contains("Owned write files: right.rs"))
            {
                self.right.complete(need, request)
            } else {
                Err(ProviderError::new(
                    loom_core::models::ErrorKind::BadRequest,
                    "Core did not dispatch a graph-scoped task",
                ))
            }
        }
    }
    let d = repo();
    fs::write(
        d.path().join("left.rs"),
        "pub fn left_value() -> u32 { 1 }\n",
    )
    .unwrap();
    fs::write(
        d.path().join("right.rs"),
        "pub fn right_value() -> u32 { 1 }\n",
    )
    .unwrap();
    let port = Port {
        arrivals: std::sync::atomic::AtomicUsize::new(0),
        left: ScriptedModel::new([
            response("read", json!({"path":"left.rs"})),
            response(
                "edit",
                json!({"path":"left.rs","old":"{ 1 }","new":"{ 2 }"}),
            ),
            response("finish", json!({"summary":"left proposal"})),
        ]),
        right: ScriptedModel::new([
            response("read", json!({"path":"right.rs"})),
            response(
                "edit",
                json!({"path":"right.rs","old":"{ 1 }","new":"{ 2 }"}),
            ),
            response("finish", json!({"summary":"right proposal"})),
        ]),
    };
    let mut core = Core::open(d.path()).unwrap();
    let mut children = std::collections::BTreeSet::new();
    let task = core
        .execute(
            "Change left_value and right_value to return 2",
            ExecutionOptions {
                intent: TaskIntent::Edit,
                approve: false,
                model: Some("pinned-model".into()),
                max_attempts: 1,
                checks: Some(vec![
                    "rg -F '{ 2 }' left.rs && rg -F '{ 2 }' right.rs".into(),
                ]),
                ..Default::default()
            },
            &port,
            &AtomicBool::new(false),
            |event| {
                if let loom_core::control::CoreEvent::WorkerEvent { event, .. } = event
                    && let Some(id) = event["child"].as_str()
                {
                    children.insert(id.to_owned());
                }
            },
        )
        .unwrap();
    assert_eq!(format!("{:?}", task.decision.mode), "Split");
    assert_eq!(
        task.status,
        TaskStatus::AwaitingApproval,
        "{}",
        task.summary
    );
    assert_eq!(children.len(), 2);
    assert_eq!(task.attempts[0].steps, 6, "count all child model calls");
    assert_eq!(
        task.attempts[0].validation.len(),
        1,
        "validate once after merge"
    );
    assert!(
        fs::read_to_string(d.path().join("left.rs"))
            .unwrap()
            .contains("{ 1 }")
    );
    core.approve(&task.id, &AtomicBool::new(false), |_| {})
        .unwrap();
    assert!(
        fs::read_to_string(d.path().join("left.rs"))
            .unwrap()
            .contains("{ 2 }")
    );
    assert!(
        fs::read_to_string(d.path().join("right.rs"))
            .unwrap()
            .contains("{ 2 }")
    );
}
#[test]
fn automatic_graph_workers_cannot_apply_a_partial_or_out_of_scope_result() {
    use loom_core::models::{ModelPort, Need, ProviderError, Request, Served};
    struct Port {
        left: ScriptedModel,
        right: ScriptedModel,
    }
    impl ModelPort for Port {
        fn complete(&self, need: Need, request: &Request) -> Result<Served, ProviderError> {
            if request
                .messages
                .iter()
                .any(|m| m.text().contains("Owned write files: left.rs"))
            {
                self.left.complete(need, request)
            } else {
                self.right.complete(need, request)
            }
        }
    }
    let d = repo();
    fs::write(d.path().join("left.rs"), "fn left_value() { }\n").unwrap();
    fs::write(d.path().join("right.rs"), "fn right_value() { }\n").unwrap();
    let port = Port {
        left: ScriptedModel::new([
            response("read", json!({"path":"right.rs"})),
            response(
                "edit",
                json!({"path":"right.rs","old":"{ }","new":"{ panic!() }"}),
            ),
            response(
                "finish",
                json!({"summary":"left worker blocked by scope","unresolved":true}),
            ),
        ]),
        right: ScriptedModel::new([
            response("read", json!({"path":"right.rs"})),
            response(
                "edit",
                json!({"path":"right.rs","old":"{ }","new":"{ println!(\"ok\"); }"}),
            ),
            response("finish", json!({"summary":"right proposed"})),
        ]),
    };
    let mut errors = vec![];
    let task = Core::open(d.path())
        .unwrap()
        .execute(
            "Change left_value and right_value",
            ExecutionOptions {
                intent: TaskIntent::Edit,
                max_attempts: 1,
                checks: Some(vec![check_command("true", "exit /b 0")]),
                ..Default::default()
            },
            &port,
            &AtomicBool::new(false),
            |event| {
                if let loom_core::control::CoreEvent::WorkerEvent { event, .. } = event
                    && event["event"]["error"] == true
                {
                    errors.push(event["event"]["result"].to_string());
                }
            },
        )
        .unwrap();
    assert_eq!(format!("{:?}", task.decision.mode), "Split");
    assert_ne!(task.status, TaskStatus::Accepted);
    assert!(
        task.attempts[0].validation.is_empty(),
        "partial worker success must not proceed to validation/application"
    );
    assert!(
        errors.iter().any(|error| error.contains("WRITE_SCOPE")),
        "missing scope denial: {errors:?}"
    );
    assert_eq!(
        fs::read_to_string(d.path().join("right.rs")).unwrap(),
        "fn right_value() { }\n"
    );
    assert!(task.summary.contains("graph workers"), "{}", task.summary);
}

#[test]
fn automatic_graph_worker_panic_reports_failure_without_waiting_out_the_execution_window() {
    use loom_core::models::{ModelPort, Need, ProviderError, Request, Served};
    struct Port;
    impl ModelPort for Port {
        fn complete(&self, _need: Need, _request: &Request) -> Result<Served, ProviderError> {
            panic!("simulated child panic");
        }
    }
    let d = repo();
    fs::write(d.path().join("left.rs"), "fn left_value() {}\n").unwrap();
    fs::write(d.path().join("right.rs"), "fn right_value() {}\n").unwrap();
    let started = std::time::Instant::now();
    let task = Core::open(d.path())
        .unwrap()
        .execute(
            "Change left_value and right_value",
            ExecutionOptions {
                intent: TaskIntent::Edit,
                max_attempts: 1,
                budget: loom_core::worker::Budget {
                    steps: 8,
                    seconds: 30,
                    tokens: 10000,
                },
                checks: Some(vec![check_command("true", "exit /b 0")]),
                ..Default::default()
            },
            &Port,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "scheduler polled dead handles until the deadline"
    );
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert!(
        task.summary.contains("without a result") || task.summary.contains("panicked"),
        "{}",
        task.summary
    );
    assert!(
        task.summary.contains("Core 실행 보고"),
        "dead child must not suppress the stop report"
    );
}

#[test]
fn validated_proposal_waits_for_approval_and_replays_after_restart() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                checks: Some(vec![check_command("test \"$(cat file.txt)\" = new", "findstr /x /c:new file.txt >nul")]),
                approve: false,
                ..Default::default()
            },
            &model(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::AwaitingApproval);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
    drop(core);
    let mut core = Core::open(d.path()).unwrap();
    let accepted = core
        .approve(&task.id, &AtomicBool::new(false), |_| {})
        .unwrap();
    assert_eq!(accepted.status, TaskStatus::Accepted);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        Core::open(d.path()).unwrap().state.tasks[&task.id].status,
        TaskStatus::Accepted
    );
}

#[test]
fn approved_patch_contains_the_exact_validated_tree_including_generated_artifacts() {
    use loom_core::base::Revision;
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                checks: Some(vec![
                    check_command("printf 'generated\\n' > generated.txt; test \"$(cat file.txt)\" = new", r#"powershell -NoProfile -Command "[IO.File]::WriteAllText('generated.txt', 'generated'+[char]10); if ((Get-Content -Raw file.txt).Trim() -ne 'new') { exit 1 }""#),
                ]),
                approve: false,
                ..Default::default()
            },
            &model(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::AwaitingApproval);
    assert!(!d.path().join("generated.txt").exists());
    fs::write(d.path().join("user.txt"), "user edit\n").unwrap();
    let task = core
        .approve(&task.id, &AtomicBool::new(false), |_| {})
        .unwrap();
    assert_eq!(task.status, TaskStatus::Accepted, "{}", task.summary);
    assert_eq!(
        fs::read_to_string(d.path().join("generated.txt")).unwrap(),
        "generated\n"
    );
    assert_eq!(
        fs::read_to_string(d.path().join("user.txt")).unwrap(),
        "user edit\n"
    );
    assert_eq!(
        core.state.revision,
        Some(Revision::capture(d.path()).unwrap())
    );
}
#[test]
fn false_finish_is_rejected_by_core_validation() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                checks: Some(vec!["exit 1".into()]),
                max_attempts: 1,
                approve: true,
                ..Default::default()
            },
            &model(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::ValidationFailed);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn finish_without_a_patch_cannot_complete_an_edit_task() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                checks: Some(vec![check_command("true", "exit /b 0")]),
                max_attempts: 1,
                ..Default::default()
            },
            &ScriptedModel::new([response("finish", json!({"summary":"all done"}))]),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn finish_can_explicitly_return_a_conversational_answer_without_edits() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([response(
        "finish",
        json!({"summary": "안녕하세요!", "answer": true}),
    )]);
    let task = core
        .execute(
            "안녕",
            ExecutionOptions {
                checks: Some(vec![check_command("false", "exit /b 1")]),
                max_attempts: 1,
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Accepted, "{}", task.summary);
    assert_eq!(task.summary, "안녕하세요!");
    assert_eq!(task.attempts.len(), 1);
    assert!(task.attempts[0].validation.is_empty());
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn finish_cannot_claim_answer_after_attempting_an_edit() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response("read", json!({"path": "file.txt"})),
        response(
            "edit",
            json!({"path": "file.txt", "old": "absent", "new": "new"}),
        ),
        response("finish", json!({"summary": "done", "answer": true})),
    ]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                max_attempts: 1,
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn finish_without_changes_does_not_restart_a_new_worker_or_call_map() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response("finish", json!({"summary":"nothing changed"})),
        response("map", json!({"path":"."})),
    ]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                max_attempts: 8,
                checks: Some(vec![check_command("true", "exit /b 0")]),
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert_eq!(task.attempts.len(), 1, "finish must end the Core turn");
    assert_eq!(model.requests().len(), 1, "no request after finish");
    assert_eq!(model.remaining(), 1, "map after finish must stay untouched");
}

#[test]
fn single_attempt_failed_validation_after_finish_does_not_restart_exploration() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response("read", json!({"path":"file.txt"})),
        response("edit", json!({"path":"file.txt","old":"old","new":"bad"})),
        response("finish", json!({"summary":"proposed"})),
        response("map", json!({"path":"."})),
    ]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                max_attempts: 1,
                checks: Some(vec!["exit 1".into()]),
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::ValidationFailed);
    assert_eq!(
        task.attempts.len(),
        1,
        "Core must respect the explicit one-attempt limit"
    );
    assert_eq!(model.requests().len(), 3);
    assert_eq!(model.remaining(), 1);
}

#[test]
fn core_repairs_a_finished_proposal_from_validation_evidence_without_user_resume() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response("read", json!({"path":"file.txt"})),
        response("edit", json!({"path":"file.txt","old":"old","new":"bad"})),
        response("finish", json!({"summary":"first proposal"})),
        response("read", json!({"path":"file.txt"})),
        response("edit", json!({"path":"file.txt","old":"bad","new":"new"})),
        response("finish", json!({"summary":"repaired proposal"})),
    ]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                max_attempts: 2,
                intent: TaskIntent::Edit,
                checks: Some(vec![check_command("test \"$(cat file.txt)\" = new", "findstr /x /c:new file.txt >nul")]),
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Accepted, "{}", task.summary);
    assert_eq!(task.attempts.len(), 2);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "new\n"
    );
    let repair = model.requests()[3]
        .messages
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(repair.contains("prior_observations") && repair.contains("file.txt"));
    assert!(repair.contains("proposal_restored") && repair.contains("validation"));
}

#[test]
fn identical_failed_patch_is_not_revalidated_or_retried() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response("read", json!({"path":"file.txt"})),
        response("edit", json!({"path":"file.txt","old":"old","new":"bad"})),
        response("finish", json!({"summary":"first proposal"})),
        response("finish", json!({"summary":"same proposal"})),
        response("map", json!({"path":"."})),
    ]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                max_attempts: 4,
                checks: Some(vec!["exit 1".into()]),
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::ValidationFailed);
    assert_eq!(task.attempts.len(), 2);
    assert!(task.summary.contains("unchanged"));
    assert_eq!(model.requests().len(), 4);
    assert_eq!(model.remaining(), 1);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn evidence_index_survives_multiple_worker_replacements_without_remote_replay() {
    let d = repo();
    let options = ExecutionOptions {
        max_attempts: 1,
        readonly: true,
        ..Default::default()
    };
    let mut core = Core::open(d.path()).unwrap();
    let first = core
        .execute(
            "locate old text",
            options.clone(),
            &ScriptedModel::new([
                response("find", json!({"pattern":"old"})),
                response(
                    "finish",
                    json!({"summary":"need more investigation","unresolved":true}),
                ),
            ]),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    drop(core);
    let mut core = Core::open(d.path()).unwrap();
    for _ in 0..2 {
        // Later resumes can escalate; the fixture must serve every allowed tier.
        let model = ScriptedModel::with_candidate(
            loom_core::models::router::Candidate {
                endpoint: "scripted".into(),
                model: "scripted".into(),
                tier: loom_core::models::Tier::Flagship,
                cost: loom_core::models::CostClass::Local,
                price: Some((0.0, 0.0)),
                tools: true,
                context: None,
            },
            [Ok(response(
                "finish",
                json!({"summary":"waiting","unresolved":true}),
            ))],
        );
        let task = core
            .resume(
                &first.id,
                options.clone(),
                &model,
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        let prompt = model.requests()[0]
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt.contains("file.txt:1") && prompt.contains("old"));
        assert!(
            !prompt.contains("\"observations\""),
            "the full index leaked into the remote prompt"
        );
        use loom_core::base::ArtifactStore;
        let result: serde_json::Value = serde_json::from_slice(
            &core
                .artifacts()
                .get(&task.attempts.last().unwrap().result, None)
                .unwrap(),
        )
        .unwrap();
        assert!(result["work_memory"].to_string().contains("file.txt:1"));
        assert_eq!(
            model.requests().len(),
            1,
            "memory transfer should not require a model call"
        );
    }
}

#[test]
fn unresolved_finish_does_not_trigger_another_tier_attempt() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(
            "finish",
            json!({"summary":"cannot locate it","unresolved":true}),
        ),
        response("map", json!({"path":"."})),
    ]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                max_attempts: 8,
                checks: Some(vec![check_command("true", "exit /b 0")]),
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert_eq!(task.attempts.len(), 1);
    assert_eq!(model.requests().len(), 1);
    assert_eq!(model.remaining(), 1);
}

#[test]
fn total_token_budget_is_shared_by_all_core_attempts() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let mut first = response("map", json!({"path":"."}));
    first.usage = Usage {
        input: Some(6),
        cached_input: Some(0),
        output: Some(3),
    };
    let model = ScriptedModel::new([first, response("map", json!({"path":"."}))]);
    let task = core
        .execute(
            "inspect the repository",
            ExecutionOptions {
                budget: loom_core::worker::Budget {
                    steps: 4,
                    seconds: 30,
                    tokens: 9,
                },
                max_attempts: 8,
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert_eq!(
        task.attempts.len(),
        1,
        "token exhaustion must not restart the worker"
    );
    assert_eq!(model.requests().len(), 1);
    assert!(task.summary.contains("total model token budget exhausted"));
}

#[test]
fn zero_total_time_budget_starts_no_worker() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([response("map", json!({"path":"."}))]);
    let task = core
        .execute(
            "inspect the repository",
            ExecutionOptions {
                budget: loom_core::worker::Budget {
                    steps: 4,
                    seconds: 0,
                    tokens: 100,
                },
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert!(task.attempts.is_empty());
    assert!(model.requests().is_empty());
    assert!(
        task.summary
            .contains("total execution time budget exhausted")
    );
}

#[test]
fn exhausted_run_reports_actual_work_remaining_work_and_unapplied_patch() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response("read", json!({"path":"file.txt"})),
        response("edit", json!({"path":"file.txt","old":"old","new":"new"})),
    ]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                intent: TaskIntent::Edit,
                max_attempts: 1,
                budget: loom_core::worker::Budget {
                    steps: 2,
                    seconds: 30,
                    tokens: 10000,
                },
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    for expected in [
        "진행 결과",
        "file.txt",
        "원본 반영: 없음",
        "남은 작업",
        ".loom/artifacts/",
    ] {
        assert!(
            task.summary.contains(expected),
            "missing {expected}: {}",
            task.summary
        );
    }
    assert_eq!(
        model.requests().len(),
        2,
        "reporting must not spend another model call"
    );
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn cancelling_after_edit_reports_the_partial_work_without_applying_it() {
    use loom_core::control::CoreEvent;
    use std::sync::atomic::Ordering;
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let model = model();
    let task = core
        .execute(
            "change file",
            ExecutionOptions::default(),
            &model,
            &cancel,
            |event| {
                if let CoreEvent::WorkerEvent { event, .. } = event
                    && event["tool"] == "edit"
                {
                    cancel.store(true, Ordering::SeqCst);
                }
            },
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    for expected in [
        "작업을 중단",
        "file.txt",
        "원본 반영: 없음",
        "남은 작업",
        "모델 요청 2/",
    ] {
        assert!(
            task.summary.contains(expected),
            "missing {expected}: {}",
            task.summary
        );
    }
    assert_eq!(model.requests().len(), 2);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
    let reopened = Core::open(d.path()).unwrap();
    assert_eq!(reopened.state.tasks[&task.id].summary, task.summary);
}

#[test]
fn last_turn_provider_timeout_is_reported_without_an_extra_summarizer_call() {
    use loom_core::models::{CostClass, ErrorKind, ProviderError, Tier, router::Candidate};
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::with_candidate(
        Candidate {
            endpoint: "scripted".into(),
            model: "scripted".into(),
            tier: Tier::Local,
            cost: CostClass::Local,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        },
        [Err(ProviderError::new(
            ErrorKind::Transient,
            "gateway response timed out",
        ))],
    );
    let mut messages = Vec::new();
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                max_attempts: 1,
                budget: loom_core::worker::Budget {
                    steps: 1,
                    seconds: 30,
                    tokens: 10000,
                },
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |event| {
                if let loom_core::control::CoreEvent::WorkerEvent { event, .. } = event
                    && let Some(message) = event["progress"]["message"].as_str()
                {
                    messages.push(message.to_owned());
                }
            },
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    for expected in [
        "gateway response timed out",
        "실행 완료된 도구 기록 없음",
        "격리 사본 수정 제안: 없음",
        "남은 작업",
    ] {
        assert!(
            task.summary.contains(expected),
            "missing {expected}: {}",
            task.summary
        );
    }
    assert_eq!(
        messages.len(),
        1,
        "provider failure should emit only its actual progress message: {messages:?}"
    );
    assert!(messages[0].contains("No model response was received"));
    assert_eq!(model.requests().len(), 1);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn a_new_worker_cannot_spend_the_time_reserved_for_independent_validation() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response("read", json!({"path":"file.txt"})),
        response("finish", json!({"summary":"second worker must not start"})),
    ]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                intent: TaskIntent::Edit,
                max_attempts: 3,
                budget: loom_core::worker::Budget {
                    steps: 1,
                    seconds: 10,
                    tokens: 10000,
                },
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |event| {
                if let loom_core::control::CoreEvent::WorkerEvent { event, .. } = event
                    && event["tool"] == "read"
                {
                    // The only worker spends its 9s execution window, leaving Core's 1s.
                    std::thread::sleep(Duration::from_secs(9));
                }
            },
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert_eq!(
        model.requests().len(),
        1,
        "a retry consumed the validation reserve"
    );
    assert!(task.summary.contains("누적 실행: 시도 1회 · 모델 요청 1회"));
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn stop_report_totals_every_attempt_instead_of_only_the_last_retry() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let mut first = response("read", json!({"path":"file.txt"}));
    first.usage = Usage {
        input: Some(11),
        output: Some(3),
        cached_input: Some(0),
    };
    let mut second = response("read", json!({"path":"file.txt"}));
    second.usage = Usage {
        input: Some(7),
        output: Some(2),
        cached_input: Some(0),
    };
    let model = ScriptedModel::new([first, second]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                intent: TaskIntent::Edit,
                max_attempts: 2,
                budget: loom_core::worker::Budget {
                    steps: 1,
                    seconds: 30,
                    tokens: 10000,
                },
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert!(
        task.summary
            .contains("누적 실행: 시도 2회 · 모델 요청 2회 · 토큰 23"),
        "{}",
        task.summary
    );
    assert_eq!(model.requests().len(), 2);
}

#[test]
fn explicit_edit_intent_cannot_succeed_with_only_a_prose_reply() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([Response {
        parts: vec![Part::Text("I made the changes".into())],
        usage: Usage::default(),
        stop: Stop::EndTurn,
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    }]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                intent: TaskIntent::Edit,
                max_attempts: 1,
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Unresolved);
    assert_eq!(task.intent, TaskIntent::Edit);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
    assert!(model.requests()[0].messages.iter().any(|m| {
        m.text()
            .contains("Required outcome: modify the requested files")
    }));
}

#[test]
fn validation_respects_the_shared_deadline() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                checks: Some(vec![check_command("sleep 3", "ping -n 4 127.0.0.1 >nul"), "exit 87".into()]),
                budget: loom_core::worker::Budget {
                    steps: 5,
                    seconds: 1,
                    tokens: 10000,
                },
                max_attempts: 1,
                approve: false,
                ..Default::default()
            },
            &model(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::ValidationFailed);
    assert_eq!(task.attempts[0].validation.len(), 1);
    assert_eq!(
        task.attempts[0].validation[0].status,
        loom_core::effects::RunStatus::TimedOut
    );
    assert!(
        task.summary
            .contains("validation exceeded the shared execution deadline")
    );
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn retry_receives_observed_scope_misses_instead_of_only_the_stale_plan() {
    let d = repo();
    let options = ExecutionOptions {
        checks: Some(vec![check_command("test \"$(cat file.txt)\" = expected", "findstr /x /c:expected file.txt >nul")]),
        max_attempts: 1,
        approve: false,
        ..Default::default()
    };
    let mut core = Core::open(d.path()).unwrap();
    let failed = core
        .execute(
            "change file",
            options.clone(),
            &model(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(failed.status, TaskStatus::ValidationFailed);
    assert!(
        failed
            .scope
            .as_ref()
            .unwrap()
            .events
            .iter()
            .any(|event| event.contains("file.txt"))
    );

    let retry = ScriptedModel::new([response(
        "finish",
        json!({"summary":"need more evidence", "unresolved":true}),
    )]);
    core.resume(&failed.id, options, &retry, &AtomicBool::new(false), |_| {})
        .unwrap();
    let request = retry.requests().remove(0);
    let handoff = request
        .messages
        .iter()
        .map(|message| message.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        handoff.contains("scope_feedback"),
        "handoff omitted observed scope: {handoff}"
    );
    assert!(handoff.contains("file.txt"));
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn supplied_context_and_reread_measurement_survive_restart() {
    let d = repo();
    fs::write(d.path().join("lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let options = ExecutionOptions {
        budget: loom_core::worker::Budget {
            steps: 1,
            seconds: 30,
            tokens: 100_000,
        },
        max_attempts: 1,
        use_context: true,
        ..Default::default()
    };
    let task = core
        .execute(
            "fix value",
            options.clone(),
            &ScriptedModel::new([response("read", json!({"path":"lib.rs"}))]),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert!(task.context.is_some());
    assert!(task.slice_bytes > 0);
    assert_eq!(task.attempts[0].rereads, 1);
    let model = ScriptedModel::new([response("read", json!({"path":"lib.rs"}))]);
    let task = Core::open(d.path())
        .unwrap()
        .resume(
            &task.id,
            ExecutionOptions {
                use_context: false,
                ..options
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.attempts.last().unwrap().rereads, 1);
    assert!(
        model.requests()[0].messages[0]
            .text()
            .contains("pub fn value()")
    );
}

#[test]
fn readonly_proposals_accept_findings_without_a_patch_or_build() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "explain file",
            ExecutionOptions {
                readonly: true,
                ..Default::default()
            },
            &ScriptedModel::new([
                response("read", json!({"path":"file.txt"})),
                response("finish", json!({"summary":"file contains old"})),
            ]),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Accepted);
    assert_eq!(core.state.findings, ["file contains old"]);
    assert_eq!(
        Core::open(d.path()).unwrap().state.findings,
        core.state.findings
    );
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn retry_preserves_the_failed_proposal_and_can_read_previous_validation_evidence() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let first = ScriptedModel::new([
        response("read", json!({"path":"file.txt"})),
        response("edit", json!({"path":"file.txt","old":"old","new":"bad"})),
        response("finish", json!({"summary":"first proposal"})),
    ]);
    let options=ExecutionOptions{max_attempts:1,checks:Some(vec![check_command("if [ \"$(cat file.txt)\" != new ]; then printf 'specific-failure-evidence\\n'; exit 1; fi", "findstr /x /c:new file.txt >nul || (echo specific-failure-evidence& exit /b 1)")]),..Default::default()};
    let task = core
        .execute(
            "change file",
            options.clone(),
            &first,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::ValidationFailed);
    let log = &task.attempts[0].validation[0].artifact;
    let model = ScriptedModel::new([
        response("read_artifact", json!({"path":log.id,"start":1,"end":20})),
        response("read", json!({"path":"file.txt"})),
        response("edit", json!({"path":"file.txt","old":"bad","new":"new"})),
        response("finish", json!({"summary":"fixed previous proposal"})),
    ]);
    let task = Core::open(d.path())
        .unwrap()
        .resume(&task.id, options, &model, &AtomicBool::new(false), |_| {})
        .unwrap();
    assert_eq!(task.status, TaskStatus::Accepted, "{}", task.summary);
    let requests = model.requests();
    let evidence = requests[1].messages.last().unwrap();
    assert!(
        matches!(&evidence.parts[0],Part::ToolResult{is_error:false,output,..} if output.contains("specific-failure-evidence"))
    );
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "new\n"
    );
}

#[test]
fn an_active_task_cannot_be_resumed_by_another_core_instance() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let mut rejected = None;
    let options = ExecutionOptions {
        checks: Some(vec![check_command("true", "exit /b 0")]),
        ..Default::default()
    };
    core.execute(
        "change file",
        options.clone(),
        &model(),
        &AtomicBool::new(false),
        |event| {
            if let loom_core::control::CoreEvent::TaskSaved(task) = event
                && task.status == TaskStatus::Running
                && rejected.is_none()
            {
                rejected = Some(
                    Core::open(d.path())
                        .unwrap()
                        .resume(
                            &task.id,
                            options.clone(),
                            &ScriptedModel::new([]),
                            &AtomicBool::new(false),
                            |_| {},
                        )
                        .is_err(),
                );
            }
        },
    )
    .unwrap();
    assert_eq!(rejected, Some(true));
}
#[test]
fn three_way_application_preserves_nonconflicting_user_edits_and_refuses_conflicts() {
    let d = repo();
    fs::write(d.path().join("other.txt"), "user base\n").unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                checks: Some(vec![check_command("true", "exit /b 0")]),
                approve: false,
                ..Default::default()
            },
            &model(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    fs::write(d.path().join("other.txt"), "user changed\n").unwrap();
    let accepted = core
        .approve(&task.id, &AtomicBool::new(false), |_| {})
        .unwrap();
    assert_eq!(
        accepted.status,
        TaskStatus::Accepted,
        "{}",
        accepted.summary
    );
    assert_eq!(
        fs::read_to_string(d.path().join("other.txt")).unwrap(),
        "user changed\n"
    );
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                checks: Some(vec![check_command("true", "exit /b 0")]),
                approve: false,
                ..Default::default()
            },
            &model(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    fs::write(d.path().join("file.txt"), "user conflict\n").unwrap();
    assert_eq!(
        core.approve(&task.id, &AtomicBool::new(false), |_| {})
            .unwrap()
            .status,
        TaskStatus::ScopeConflict
    );
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "user conflict\n"
    );
}

#[test]
fn a_stale_client_cannot_reject_an_already_accepted_task() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                checks: Some(vec![check_command("true", "exit /b 0")]),
                approve: false,
                ..Default::default()
            },
            &model(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    let mut stale = Core::open(d.path()).unwrap();
    core.approve(&task.id, &AtomicBool::new(false), |_| {})
        .unwrap();
    assert!(stale.reject(&task.id, |_| {}).is_err());
    assert_eq!(
        Core::open(d.path()).unwrap().state.tasks[&task.id].status,
        TaskStatus::Accepted
    );
}

#[test]
fn cancellation_preserves_original_and_restart_does_not_apply_a_partial_patch() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            ExecutionOptions::default(),
            &model(),
            &AtomicBool::new(true),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
    assert_eq!(
        Core::open(d.path()).unwrap().state.tasks[&task.id].status,
        TaskStatus::Cancelled
    );
}

#[test]
fn escalation_is_monotonic_across_restart_and_handoff_contains_failure_evidence() {
    use loom_core::models::{CostClass, Tier, router::Candidate};
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let scripted = ScriptedModel::with_candidate(
        Candidate {
            endpoint: "scripted".into(),
            model: "scripted".into(),
            tier: Tier::Flagship,
            cost: CostClass::Free,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        },
        (0..3).map(|_| Ok(response("read", json!({"path":"file.txt"})))),
    );
    let options = ExecutionOptions {
        budget: loom_core::worker::Budget {
            steps: 1,
            seconds: 30,
            tokens: 100_000,
        },
        max_attempts: 3,
        ..Default::default()
    };
    let task = core
        .execute(
            "change file",
            options.clone(),
            &scripted,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(
        task.attempts.iter().map(|a| a.tier).collect::<Vec<_>>(),
        [Tier::Local, Tier::Local, Tier::Small]
    );
    let mut core = Core::open(d.path()).unwrap();
    let scripted = ScriptedModel::with_candidate(
        Candidate {
            endpoint: "scripted".into(),
            model: "scripted".into(),
            tier: Tier::Flagship,
            cost: CostClass::Free,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        },
        [Ok(response("read", json!({"path":"file.txt"})))],
    );
    let task = core
        .resume(
            &task.id,
            ExecutionOptions {
                max_attempts: 1,
                ..options
            },
            &scripted,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.attempts.last().unwrap().tier, Tier::Small);
    let request = scripted.requests().remove(0);
    assert!(format!("{:?}", request.messages).contains("budget exhausted"));
}

#[test]
fn resuming_a_scope_conflict_uses_the_current_workspace_as_its_base() {
    let d = repo();
    let options = ExecutionOptions {
        checks: Some(vec![check_command("true", "exit /b 0")]),
        approve: false,
        max_attempts: 1,
        ..Default::default()
    };
    let task = Core::open(d.path())
        .unwrap()
        .execute(
            "change file",
            options.clone(),
            &model(),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    fs::write(d.path().join("file.txt"), "user conflict\n").unwrap();
    let mut core = Core::open(d.path()).unwrap();
    assert_eq!(
        core.approve(&task.id, &AtomicBool::new(false), |_| {})
            .unwrap()
            .status,
        TaskStatus::ScopeConflict
    );
    let model = ScriptedModel::new([
        response("read", json!({"path":"file.txt"})),
        response(
            "edit",
            json!({"path":"file.txt","old":"user conflict","new":"resolved"}),
        ),
        response("finish", json!({"summary":"resolved current user edit"})),
    ]);
    let task = core
        .resume(
            &task.id,
            ExecutionOptions {
                approve: true,
                ..options
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Accepted, "{}", task.summary);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "resolved\n"
    );
}

#[test]
fn retry_can_read_logs_from_the_previous_workers_own_check() {
    use loom_core::base::ArtifactStore;
    let d = repo();
    let options = ExecutionOptions {
        checks: Some(vec![check_command("printf 'worker-check-evidence\\n'; exit 1", "echo worker-check-evidence& exit /b 1")]),
        max_attempts: 1,
        ..Default::default()
    };
    let mut core = Core::open(d.path()).unwrap();
    let task = core
        .execute(
            "change file",
            options.clone(),
            &ScriptedModel::new([
                response("check", json!({})),
                response(
                    "finish",
                    json!({"summary":"check failed","unresolved":true}),
                ),
            ]),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    let result: loom_core::worker::WorkerResult = serde_json::from_slice(
        &core
            .artifacts()
            .get(&task.attempts[0].result, None)
            .unwrap(),
    )
    .unwrap();
    let trace = core.artifacts().get(&result.trace, None).unwrap();
    let event = String::from_utf8(trace)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["tool"] == "check")
        .unwrap();
    let output: serde_json::Value =
        serde_json::from_str(event["result"].as_str().unwrap()).unwrap();
    let path = output["commands"][0]["artifact_path"].as_str().unwrap();
    let model = ScriptedModel::new([
        response("read_artifact", json!({"path":path,"start":1,"end":20})),
        response(
            "finish",
            json!({"summary":"investigated","unresolved":true}),
        ),
    ]);
    core.resume(&task.id, options, &model, &AtomicBool::new(false), |_| {})
        .unwrap();
    assert!(model.requests()[1].messages.last().unwrap().parts.iter().any(|part| matches!(part, Part::ToolResult{output,is_error:false,..} if output.contains("worker-check-evidence"))));
}

#[test]
fn a_prose_reply_ends_the_turn_without_exploring_validating_or_retrying() {
    // "안녕" once ran for hours: the reply was treated as a stalled edit task, so the
    // worker was pushed into the repository and Core retried and escalated.
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        Response {
            parts: vec![Part::Text("안녕하세요! 무엇을 도와드릴까요?".into())],
            usage: Usage::default(),
            stop: Stop::EndTurn,
            model: "scripted".into(),
            latency: Duration::ZERO,
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        },
        response("map", json!({"path":"."})),
    ]);
    let task = core
        .execute(
            "안녕",
            ExecutionOptions {
                checks: Some(vec![check_command("false", "exit /b 1")]),
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Accepted, "{}", task.summary);
    assert_eq!(task.summary, "안녕하세요! 무엇을 도와드릴까요?");
    assert_eq!(task.attempts.len(), 1, "no retry or escalation");
    assert_eq!(model.remaining(), 1, "one model call, no exploration");
    assert!(
        task.attempts[0].validation.is_empty(),
        "nothing to validate"
    );
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn structured_finish_text_after_edit_is_a_proposal_subject_to_independent_validation() {
    for (new, expected) in [
        ("new", TaskStatus::AwaitingApproval),
        ("wrong", TaskStatus::ValidationFailed),
    ] {
        let d = repo();
        let model = ScriptedModel::new([
            response("read", json!({"path":"file.txt"})),
            response("edit", json!({"path":"file.txt","old":"old","new":new})),
            Response {
                parts: vec![Part::Text(
                    json!({"summary":"change proposed","answer":true,"unresolved":false})
                        .to_string(),
                )],
                usage: Usage::default(),
                stop: Stop::EndTurn,
                model: "scripted".into(),
                latency: Duration::ZERO,
                route: None,
                gateway_connection: None,
                gateway_upstream_ms: None,
            },
        ]);
        let task = Core::open(d.path())
            .unwrap()
            .execute(
                "change file",
                ExecutionOptions {
                    intent: TaskIntent::Edit,
                    approve: false,
                    max_attempts: 1,
                    checks: Some(vec![check_command("test \"$(cat file.txt)\" = new", "findstr /x /c:new file.txt >nul")]),
                    ..Default::default()
                },
                &model,
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap();
        assert_eq!(task.status, expected, "{}", task.summary);
        assert_eq!(
            task.attempts[0].steps, 3,
            "do not ask the model to repeat an already structured finish"
        );
        assert_eq!(
            task.attempts[0].validation.len(),
            1,
            "answer=true after edit must not skip validation"
        );
        assert_eq!(
            fs::read_to_string(d.path().join("file.txt")).unwrap(),
            "old\n"
        );
    }
}

#[test]
fn prose_after_an_edit_is_not_an_answer() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response("read", json!({"path":"file.txt"})),
        response("edit", json!({"path":"file.txt","old":"old","new":"new"})),
        Response {
            parts: vec![Part::Text("done, I think".into())],
            usage: Usage::default(),
            stop: Stop::EndTurn,
            model: "scripted".into(),
            latency: Duration::ZERO,
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        },
        response("finish", json!({"summary":"changed"})),
    ]);
    let task = core
        .execute(
            "change file",
            ExecutionOptions {
                checks: Some(vec![check_command("true", "exit /b 0")]),
                max_attempts: 1,
                ..Default::default()
            },
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    assert_eq!(task.status, TaskStatus::Accepted, "{}", task.summary);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "new\n",
        "validated edit applied"
    );
}

#[test]
fn a_task_left_running_by_a_dead_process_is_not_executing() {
    let d = repo();
    // Simulate a process that saved Running and died: append the event directly.
    {
        let mut core = Core::open(d.path()).unwrap();
        let cancel = AtomicBool::new(false);
        let _ = core.execute(
            "change file",
            ExecutionOptions {
                max_attempts: 1,
                ..Default::default()
            },
            &ScriptedModel::new([response("finish", json!({"summary":"x"}))]),
            &cancel,
            |_| {},
        );
    }
    let log = d.path().join(".loom/events.jsonl");
    let mut lines: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    let running = lines
        .iter()
        .rev()
        .find(|l| l.contains("\"TaskSaved\"") && l.contains("\"Running\""))
        .expect("a Running event")
        .clone();
    let mut event: serde_json::Value = serde_json::from_str(&running).unwrap();
    event["sequence"] = json!(lines.len() as u64 + 1);
    lines.push(event.to_string());
    fs::write(&log, lines.join("\n") + "\n").unwrap();
    let core = Core::open(d.path()).unwrap();
    let task = core
        .state
        .tasks
        .values()
        .find(|t| t.goal == "change file")
        .unwrap();
    assert_eq!(
        task.status,
        TaskStatus::Running,
        "stored state is kept for resume"
    );
    assert!(!core.is_executing(&task.id), "but nobody executes it");
}
