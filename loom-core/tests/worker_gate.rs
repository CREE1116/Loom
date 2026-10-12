mod isolation;
use loom_core::{
    base::Revision,
    effects::Overlay,
    models::{Part, Response, ScriptedModel, Stop, Usage},
    worker::{Budget, ToolSet, WorkerStatus, WorkerTask, run},
};
use serde_json::json;
use std::{fs, process::Command, sync::atomic::AtomicBool, time::Duration};

fn task(steps: usize) -> (tempfile::TempDir, WorkerTask) {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .current_dir(directory.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    fs::write(
        directory.path().join("lib.rs"),
        "pub fn value() -> u32 { 1 }\n",
    )
    .unwrap();
    let overlay = Overlay::create(
        directory.path(),
        &Revision::capture(directory.path()).unwrap(),
    )
    .unwrap();
    (
        directory,
        WorkerTask {
            allowed_writes: None,
            goal: "change value to 2".into(),
            guidance: String::new(),
            images: vec![],
            skills: vec![],
            reasoning_effort: None,
            model: None,
            context_limit: 96_000,
            plan: vec![],
            depth: 0,
            defer_validation_to_core: false,
            control: Default::default(),
            policy: loom_core::effects::ExecutionPolicy {
                sandbox: loom_core::effects::SandboxMode::FullAccess,
                network: true,
            },
            overlay,
            context: None,
            supplied: vec![],
            handoff: None,
            artifacts: vec![],
            tools: ToolSet::Edit,
            tool_profile: Default::default(),
            budget: Budget {
                steps,
                seconds: 30,
                tokens: 100_000,
            },
            need: Default::default(),
            checks: vec![],
        },
    )
}
fn call(name: &str, args: serde_json::Value) -> Response {
    Response {
        parts: vec![Part::ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: args,
        }],
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
    }
}

#[test]
fn file_patterns_and_filtered_search_are_available_to_read_only_workers_without_enabling_tools() {
    let (_directory, mut task) = task(4);
    task.tools = ToolSet::ReadOnly;
    task.goal = "Find Rust files and the VALUE symbol without editing".into();
    let model = ScriptedModel::new([
        call("glob", json!({"pattern":"**/*.rs"})),
        call("find", json!({"pattern":"VALUE", "include":"*.rs", "case_sensitive":false})),
        call("finish", json!({"summary":"Found value in lib.rs", "answer":true})),
    ]);
    let mut results = Vec::new();
    let report = run(&mut task, &model, &AtomicBool::new(false), |event| {
        if matches!(event["tool"].as_str(), Some("glob" | "find")) && event["result"].is_string() {
            results.push(event["result"].as_str().unwrap().to_owned());
        }
    }).unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0].contains("lib.rs"), "{}", results[0]);
    assert!(results[1].contains("lib.rs:1") && results[1].contains("value"), "{}", results[1]);
    assert!(report.patch.files.is_empty());
    let requests = model.requests();
    assert!(requests[0].tools.iter().any(|tool| tool.name == "glob"));
}

#[test]
fn graph_child_check_is_deferred_and_never_reported_as_passed() {
    let (_directory, mut task) = task(2);
    task.defer_validation_to_core = true;
    task.checks = vec![if cfg!(windows) {
        "echo ran > check-ran"
    } else {
        "touch check-ran"
    }
    .into()];
    let model = ScriptedModel::new([
        call("check", json!({})),
        call(
            "finish",
            json!({"summary":"scope proposal","unresolved":true}),
        ),
    ]);
    let mut results = vec![];
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "check" {
            results.push(event);
        }
    })
    .unwrap();
    assert!(
        !task.overlay.dir().join("check-ran").exists(),
        "child ran the parent check before merge"
    );
    assert!(
        model
            .requests()
            .iter()
            .all(|request| !request.tools.iter().any(|tool| tool.name == "check"))
    );
    assert_eq!(results.len(), 1);
    assert!(
        results[0]["result_summary"]
            .as_str()
            .unwrap()
            .contains("not yet validated")
    );
    let result: serde_json::Value =
        serde_json::from_str(results[0]["result"].as_str().unwrap()).unwrap();
    assert_eq!(result["status"], "deferred_to_core");
    assert!(
        result["passed"].is_null(),
        "deferral is not validation success"
    );
}

#[test]
fn empty_compiled_context_preserves_local_anchor_orientation() {
    let (_directory, mut task) = task(1);
    task.context = Some(String::new());
    let model = ScriptedModel::new([call(
        "finish",
        json!({"summary":"need to inspect further","unresolved":true}),
    )]);
    run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let request = model.requests().remove(0);
    assert!(
        request.messages.iter().any(|message| {
            message.text().contains("Likely relevant code:") && message.text().contains("lib.rs")
        }),
        "empty context disabled the graph's local anchor hint"
    );
}

#[test]
fn web_fetch_without_network_requests_consent_and_denial_never_fetches() {
    let (_directory, mut task) = task(3);
    task.policy.network = false;
    let model = ScriptedModel::new([
        call("web_fetch", json!({"url":"https://example.org/private"})),
        call(
            "finish",
            json!({"summary":"network permission denied","unresolved":true}),
        ),
    ]);
    let control = task.control.clone();
    let mut requests = vec![];
    let result = run(&mut task, &model, &AtomicBool::new(false), |event| {
        if let Some(request) = event.get("network_approval_request") {
            requests.push(request.clone());
            control.answer(request["id"].as_str().unwrap().into(), json!(false));
        }
    })
    .unwrap();
    assert_eq!(
        requests.len(),
        1,
        "a denied web request must ask exactly once"
    );
    assert_eq!(requests[0]["tool"], "web_fetch");
    assert_eq!(requests[0]["target"], "https://example.org/private");
    let model_requests = model.requests();
    assert!(model_requests[1].messages.last().unwrap().parts.iter().any(
        |p| matches!(p, Part::ToolResult{is_error:true,output,..} if output.contains("denied"))
    ));
    assert_eq!(result.status, WorkerStatus::Unresolved);
}
#[test]
fn read_edit_finish_produces_a_patch_without_accepting_or_changing_the_workspace() {
    let (directory, mut task) = task(5);
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs"})),
        call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
        call("finish", json!({"summary":"proposed"})),
    ]);
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.status, WorkerStatus::Proposed);
    assert_eq!(result.patch.files.len(), 1);
    assert_eq!(result.input_tokens, 30);
    assert!(
        fs::read_to_string(directory.path().join("lib.rs"))
            .unwrap()
            .contains("{ 1 }")
    );
    assert!(
        fs::read_to_string(task.overlay.dir().join("lib.rs"))
            .unwrap()
            .contains("{ 2 }")
    );
}
#[test]
fn budget_and_cancellation_are_unresolved_not_proposed() {
    let (_directory, mut task) = task(1);
    let model = ScriptedModel::new([call("read", json!({"path":"lib.rs"}))]);
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.status, WorkerStatus::Unresolved);
    let result = run(&mut task, &model, &AtomicBool::new(true), |_| {}).unwrap();
    assert_eq!(result.status, WorkerStatus::Unresolved);
}

#[test]
fn explicit_unresolved_finish_stops_without_proposing_or_repeating_a_call() {
    let (_directory, mut task) = task(5);
    let model = ScriptedModel::new([call(
        "finish",
        json!({"summary":"missing information","unresolved":true}),
    )]);
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.status, WorkerStatus::Unresolved);
    assert_eq!(result.summary, "missing information");
    assert_eq!(model.requests().len(), 1);
}

#[test]
fn a_read_overlapping_supplied_anchor_body_counts_as_a_reread() {
    let (_directory, mut task) = task(1);
    task.supplied = vec![loom_core::worker::ReadSpan {
        symbol: "lib.rs::value".into(),
        path: "lib.rs".into(),
        start: 1,
        end: 1,
    }];
    let model = ScriptedModel::new([call("read", json!({"path":"lib.rs"}))]);
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.rereads, 1);
}

#[test]
fn supplied_body_allows_edit_without_resending_the_same_read() {
    let (_directory, mut task) = task(5);
    task.context = Some("lib.rs::value\npub fn value() -> u32 { 1 }".into());
    task.supplied = vec![loom_core::worker::ReadSpan {
        symbol: "lib.rs::value".into(),
        path: "lib.rs".into(),
        start: 1,
        end: 1,
    }];
    let model = ScriptedModel::new([
        call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
        call("finish", json!({"summary":"updated supplied body"})),
    ]);
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.patch.files.len(), 1);
    assert_eq!(result.rereads, 0);
}

#[test]
fn remote_input_counts_do_not_treat_missing_provider_usage_as_zero() {
    use loom_core::models::{CostClass, Tier, router::Candidate};
    for input in [Some(10), None] {
        let (_directory, mut task) = task(1);
        let mut reply = call("read", json!({"path":"lib.rs"}));
        reply.usage.input = input;
        let model = ScriptedModel::with_candidate(
            Candidate {
                endpoint: "remote".into(),
                model: "scripted".into(),
                tier: Tier::Local,
                cost: CostClass::Free,
                price: Some((0.0, 0.0)),
                tools: true,
                context: None,
            },
            [Ok(reply)],
        );
        let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(result.remote_calls, 1);
        assert_eq!(result.remote_input_tokens, input);
    }
}

#[test]
fn finish_is_terminal_even_when_the_model_batches_a_later_edit() {
    let (_directory, mut task) = task(2);
    let mut response = call("finish", json!({"summary":"done"}));
    response.parts.push(Part::ToolCall {
        id: "later-write".into(),
        name: "write".into(),
        arguments: json!({"path":"later.txt","content":"must not run"}),
    });
    let model = ScriptedModel::new([response]);
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.status, WorkerStatus::Proposed);
    assert!(result.patch.files.is_empty());
    assert!(!task.overlay.dir().join("later.txt").exists());
}

#[test]
fn plan_and_live_terminal_tools_work_in_the_worker_loop() {
    let (_directory, mut task) = task(6);
    let model = ScriptedModel::new([
        call(
            "update_plan",
            json!({"plan":[{"step":"inspect output","status":"in_progress"}]}),
        ),
        call(
            "exec_command",
            json!({"command":"printf terminal-evidence","yield_ms":1000}),
        ),
        call("finish", json!({"summary":"inspected","unresolved":true})),
    ]);
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.plan[0].step, "inspect output");
    #[cfg(unix)]
    assert!(model.requests()[2].messages.last().unwrap().parts.iter().any(|p|matches!(p,Part::ToolResult{output,is_error:false,..} if output.contains("terminal-evidence"))));
    #[cfg(not(unix))]
    assert!(model.requests()[2].messages.last().unwrap().parts.iter().any(|p|matches!(p,Part::ToolResult{output,is_error:true,..} if output.contains("native PTY execution is only supported on Unix"))));
}
#[test]
fn parallel_child_can_propose_and_merge_without_writing_original() {
    let (directory, mut task) = task(10);
    struct Port {
        parent: ScriptedModel,
        child: ScriptedModel,
    }
    impl loom_core::models::ModelPort for Port {
        fn complete(
            &self,
            need: loom_core::models::Need,
            request: &loom_core::models::Request,
        ) -> Result<loom_core::models::Served, loom_core::models::ProviderError> {
            if request
                .messages
                .iter()
                .any(|m| m.text() == "child change value")
            {
                loom_core::models::ModelPort::complete(&self.child, need, request)
            } else {
                loom_core::models::ModelPort::complete(&self.parent, need, request)
            }
        }
    }
    let model = Port {
        parent: ScriptedModel::new([
            call(
                "spawn_agent",
                json!({"goal":"child change value","write_files":["lib.rs"],"read_files":["shared.rs"],"anchors":["lib.rs::value"]}),
            ),
            call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 3 }"})),
            call(
                "spawn_agent",
                json!({"goal":"other child","write_files":["lib.rs"]}),
            ),
            call("wait_agent", json!({"id":"child-1","timeout_ms":5000})),
            call("merge_agent", json!({"id":"child-1"})),
            call("finish", json!({"summary":"parent proposal"})),
        ]),
        child: ScriptedModel::new([
            call(
                "write",
                json!({"path":"unexpected.rs","content":"fn unexpected() {}"}),
            ),
            call("read", json!({"path":"lib.rs"})),
            call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
            call("finish", json!({"summary":"child proposal"})),
        ]),
    };
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.status, WorkerStatus::Proposed);
    assert_eq!(result.patch.files.len(), 1);
    assert!(
        model.child.requests()[0]
            .messages
            .iter()
            .any(|m| m.text().contains("Loom graph slice") && m.text().contains("lib.rs::value"))
    );
    assert!(model.parent.requests()[2].messages.last().unwrap().parts.iter()
        .any(|p| matches!(p, Part::ToolResult{is_error:true,output,..} if output.contains("WRITE_SCOPE_CONFLICT"))),
        "parent must not edit a child's leased file");
    assert!(model.parent.requests()[3].messages.last().unwrap().parts.iter()
        .any(|p| matches!(p, Part::ToolResult{is_error:true,output,..} if output.contains("WRITE_SCOPE_CONFLICT"))),
        "second child must not reserve an outstanding write scope");
    assert!(model.child.requests()[1].messages.last().unwrap().parts.iter()
        .any(|p| matches!(p, Part::ToolResult{is_error:true,output,..} if output.contains("WRITE_SCOPE_CONFLICT"))),
        "child must not write outside its declared scope");
    assert!(!task.overlay.dir().join("unexpected.rs").exists());
    assert!(
        fs::read_to_string(directory.path().join("lib.rs"))
            .unwrap()
            .contains("{ 1 }")
    );
}

#[test]
fn only_core_tools_are_listed_until_a_group_is_enabled() {
    let (_directory, mut task) = task(4);
    let model = ScriptedModel::new([
        call("enable_tools", json!({"groups":["terminal"]})),
        call(
            "finish",
            json!({"summary":"nothing to do","unresolved":true}),
        ),
    ]);
    run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let names = |i: usize| -> Vec<String> {
        model.requests()[i]
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect()
    };
    let first = names(0);
    for core in ["find", "read", "edit", "check", "finish", "enable_tools"] {
        assert!(first.iter().any(|n| n == core), "{core} missing: {first:?}");
    }
    for hidden in [
        "map",
        "ast_search",
        "refs",
        "plan_split",
        "exec_command",
        "spawn_agent",
        "update_plan",
        "rename_symbol",
    ] {
        assert!(
            !first.iter().any(|n| n == hidden),
            "{hidden} listed up front: {first:?}"
        );
    }
    let second = names(1);
    assert!(
        second.iter().any(|n| n == "exec_command") && second.iter().any(|n| n == "write_stdin")
    );
    assert!(
        !second.iter().any(|n| n == "spawn_agent"),
        "only the enabled group appears"
    );
}

#[test]
fn graph_tools_are_available_immediately_if_no_code_anchor_matches() {
    let (_directory, mut unknown) = task(1);
    unknown.goal = "zzqxy1234".into();
    let model = ScriptedModel::new([call(
        "finish",
        json!({"summary":"unknown target","unresolved":true}),
    )]);
    run(&mut unknown, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let requests = model.requests();
    let names: Vec<_> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    for expected in ["map", "ast_search", "refs", "plan_split"] {
        assert!(names.contains(&expected), "{expected} missing: {names:?}");
    }
    let schema_bytes = |tools: &[loom_core::models::ToolSpec]| -> usize {
        tools
            .iter()
            .map(|tool| {
                tool.name.len() + tool.description.len() + tool.parameters.to_string().len()
            })
            .sum()
    };
    let unknown_bytes = schema_bytes(&model.requests()[0].tools);
    let (_directory, mut anchored) = task(1);
    let anchored_model = ScriptedModel::new([call(
        "finish",
        json!({"summary":"found target","unresolved":true}),
    )]);
    run(
        &mut anchored,
        &anchored_model,
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    let anchored_bytes = schema_bytes(&anchored_model.requests()[0].tools);
    assert!(
        anchored_bytes + 400 < unknown_bytes,
        "known targets should send fewer schema bytes: {anchored_bytes} vs {unknown_bytes}"
    );
    println!(
        "first-request tool schemas: anchored={anchored_bytes} bytes, unknown={unknown_bytes} bytes"
    );
}

#[test]
fn multiword_find_uses_explicit_goal_directory_and_reports_term_intersections() {
    let (_directory, mut task) = task(3);
    let dir = task.overlay.dir().join("loom-tui/src");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("main.rs"),
        "let session = current();\nprintln!(\"{id}\");\n",
    )
    .unwrap();
    fs::write(
        task.overlay.dir().join("other.rs"),
        "let session = 1; let id = 2;\n",
    )
    .unwrap();
    task.goal = "Find session id in loom-tui".into();
    let model = ScriptedModel::new([
        call("find", json!({"pattern":"session id"})),
        call("find", json!({"pattern":"session id","path":"."})),
        call("finish", json!({"summary":"located"})),
    ]);
    let mut results = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "find" {
            results.push(event["result"].as_str().unwrap().to_owned());
        }
    })
    .unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results[0].contains("loom-tui/src/main.rs"),
        "{}",
        results[0]
    );
    assert!(!results[0].contains("other.rs"), "{}", results[0]);
    assert!(results[0].contains("no exact match"), "{}", results[0]);
    assert!(
        results[1].contains("other.rs"),
        "explicit root path should override the goal scope: {}",
        results[1]
    );
}

#[test]
fn small_worker_stops_after_eight_model_calls_without_fake_finish() {
    let (_directory, mut task) = task(30);
    task.need.min_tier = loom_core::models::Tier::Small;
    let model = ScriptedModel::with_candidate(
        loom_core::models::router::Candidate {
            endpoint: "scripted".into(),
            model: "small".into(),
            tier: loom_core::models::Tier::Small,
            cost: loom_core::models::CostClass::Free,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        },
        (1..=10).map(|i| {
            Ok(call(
                "update_plan",
                json!({"plan":[{"step":format!("distinct step {i}"),"status":"in_progress"}]}),
            ))
        }),
    );
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(model.requests().len(), 8);
    assert_eq!(result.status, WorkerStatus::Unresolved);
    assert_eq!(result.summary, "model-call budget reached");
}

#[test]
fn identical_repeated_exploration_triggers_stagnation_without_requerying_tools() {
    let (_directory, mut task) = task(20);
    let model = ScriptedModel::new((0..7).map(|_| call("outline", json!({"path":"lib.rs"}))));
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(model.requests().len(), 4);
    assert_eq!(result.summary, "exploration stalled");
    assert_eq!(result.status, WorkerStatus::Unresolved);
}

#[test]
fn equivalent_results_with_different_query_arguments_do_not_reset_stagnation() {
    let (_directory, mut task) = task(15);
    // Both spellings resolve to the same file and return identical evidence.
    let model = ScriptedModel::new((0..8).map(|i| {
        call(
            "outline",
            json!({"path": if i % 2 == 0 { "lib.rs" } else { "./lib.rs" }}),
        )
    }));
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.summary, "exploration stalled");
    assert_eq!(
        model.requests().len(),
        4,
        "equivalent results are not new evidence"
    );
}

#[test]
fn distinct_negative_searches_are_not_treated_as_equivalent_evidence() {
    let (_directory, mut task) = task(8);
    for i in 0..4 {
        let folder = task.overlay.dir().join(format!("folder{i}"));
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("empty.rs"), "pub fn unrelated() {}\n").unwrap();
    }
    // "no matches" is new negative evidence for each distinct search scope.
    let model = ScriptedModel::new(
        (0..4)
            .map(|i| {
                call(
                    "find",
                    json!({"pattern":"zzzz_no_match", "path":format!("folder{i}")}),
                )
            })
            .chain([call(
                "finish",
                json!({"summary":"investigated","unresolved":true}),
            )]),
    );
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(result.summary, "investigated");
    assert_eq!(model.requests().len(), 5);
}

#[test]
fn a_successful_edit_invalidates_cached_reads_and_evidence() {
    let (_directory, mut task) = task(5);
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs"})),
        call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
        call("read", json!({"path":"lib.rs"})),
        call("finish", json!({"summary":"done"})),
    ]);
    let mut reads = Vec::new();
    let result = run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event.get("tool") == Some(&json!("read")) {
            reads.push(event);
        }
    })
    .unwrap();
    assert_eq!(result.status, WorkerStatus::Proposed);
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["cached"], false);
    assert_eq!(reads[1]["cached"], false);
    assert!(reads[1]["result"].as_str().unwrap().contains("{ 2 }"));
}

#[test]
fn unchanged_checks_keep_exploration_evidence_and_stop_repeated_validation() {
    let (_directory, mut task) = task(12);
    task.checks = vec![if cfg!(windows) { "exit /b 0" } else { "true" }.into()];
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs"})),
        call("check", json!({})),
        call("outline", json!({"path":"lib.rs"})),
        call("check", json!({})),
        call("find", json!({"pattern":"value"})),
        call("check", json!({})),
        call("read", json!({"path":"lib.rs"})),
        call("check", json!({})),
        call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
    ]);
    let mut checks = Vec::new();
    let result = run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event.get("tool") == Some(&json!("check")) {
            checks.push(event);
        }
    })
    .unwrap();
    assert_eq!(
        result.summary,
        "validation stalled: no file changes since the first check"
    );
    assert_eq!(
        model.requests().len(),
        8,
        "unchanged checks must not consume the edit turn"
    );
    assert_eq!(checks.len(), 4);
    for repeated in &checks[1..] {
        let text = repeated["result"].as_str().unwrap();
        assert!(
            text.len() < 350,
            "cached validation re-sent a full log: {text}"
        );
        assert!(text.contains("unchanged"));
    }
    let requests = model.requests();
    let reads: Vec<_> = requests
        .iter()
        .flat_map(|r| &r.messages)
        .flat_map(|m| &m.parts)
        .filter_map(|p| match p {
            Part::ToolResult { output, .. } if output.contains("Same read call") => Some(output),
            _ => None,
        })
        .collect();
    assert!(
        !reads.is_empty(),
        "check invalidated the identical-read cache"
    );
}

#[test]
fn an_edit_invalidates_the_check_cache_and_verifies_the_new_patch() {
    let (_directory, mut task) = task(6);
    task.checks = vec![
        if cfg!(windows) {
            "findstr /c:\"pub fn value() -> u32 { 2 }\" lib.rs >nul"
        } else {
            "test \"$(cat lib.rs)\" = 'pub fn value() -> u32 { 2 }'"
        }
        .into(),
    ];
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs"})),
        call("check", json!({})),
        call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
        call("check", json!({})),
        call("finish", json!({"summary":"changed and checked"})),
    ]);
    let mut checks = Vec::new();
    let result = run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event.get("tool") == Some(&json!("check")) {
            checks.push(event);
        }
    })
    .unwrap();
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0]["error"], true);
    assert_eq!(checks[1]["error"], false);
    assert!(!checks[1]["result"].as_str().unwrap().contains("unchanged"));
    assert_eq!(result.status, WorkerStatus::Proposed);
    assert!(result.patch.diff.windows(5).any(|w| w == b"{ 2 }"));
}

#[test]
fn compaction_preserves_all_initial_instructions_and_goal() {
    let (_directory, mut task) = task(10);
    task.goal = "The original change value to 2 requirement must survive".into();
    task.guidance = "Preserve the user's formatting constraints".into();
    task.context = Some("Original compiled context from the repository".into());
    task.handoff = Some("Previous worker identified a failing test".into());
    task.context_limit = 8_000;
    let mut code = String::new();
    for line in 0..400 {
        code.push_str(&format!(
            "// generated line {line}: abcdefghijklmnopqrstuvwxyz0123456789\n"
        ));
    }
    fs::write(task.overlay.dir().join("lib.rs"), code).unwrap();
    let model = ScriptedModel::new(
        (0..6)
            .map(|i| {
                call(
                    "read",
                    json!({"path":"lib.rs","start":i * 50 + 1,"end":i * 50 + 50}),
                )
            })
            .chain([call(
                "finish",
                json!({"summary":"inspected","unresolved":true}),
            )]),
    );
    run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let requests = model.requests();
    let last = &requests.last().unwrap().messages;
    for required in [
        "The original change value to 2 requirement must survive",
        "Preserve the user's formatting constraints",
        "Original compiled context from the repository",
        "Previous worker identified a failing test",
    ] {
        assert!(
            last.iter().any(|m| m.text().contains(required)),
            "compaction dropped {required:?}"
        );
    }
    assert!(
        last.iter()
            .any(|m| m.text().contains("Full conversation archive")),
        "the evicted evidence must remain retrievable"
    );
}

#[test]
fn compaction_reduces_repeated_request_payload_on_the_same_scripted_workload() {
    fn measure(context_limit: usize) -> (usize, usize) {
        let (_directory, mut task) = task(8);
        task.context_limit = context_limit;
        let mut code = String::new();
        for line in 0..400 {
            code.push_str(&format!(
                "// generated line {line}: abcdefghijklmnopqrstuvwxyz0123456789\n"
            ));
        }
        fs::write(task.overlay.dir().join("lib.rs"), code).unwrap();
        let model = ScriptedModel::new(
            (0..6)
                .map(|i| {
                    call(
                        "read",
                        json!({"path":"lib.rs","start":i * 50 + 1,"end":i * 50 + 50}),
                    )
                })
                .chain([call(
                    "finish",
                    json!({"summary":"inspected","unresolved":true}),
                )]),
        );
        let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(result.status, WorkerStatus::Unresolved);
        let sizes: Vec<usize> = model
            .requests()
            .iter()
            .map(|request| serde_json::to_vec(&request.messages).unwrap().len())
            .collect();
        (*sizes.last().unwrap(), sizes.iter().sum())
    }

    let (last_unbounded, total_unbounded) = measure(usize::MAX);
    let (last_compacted, total_compacted) = measure(8_000);
    eprintln!(
        "scripted message JSON bytes: last {last_unbounded} -> {last_compacted}; total {total_unbounded} -> {total_compacted}"
    );
    assert!(last_compacted < last_unbounded);
    assert!(total_compacted < total_unbounded);
}

#[test]
fn compaction_can_evict_a_single_oversized_completed_tool_turn() {
    let (_directory, mut task) = task(5);
    task.context_limit = 8_000;
    let code = (0..390)
        .map(|i| format!("// {i}: abcdefghijklmnopqrstuvwxyz0123456789\n"))
        .collect::<String>();
    fs::write(task.overlay.dir().join("lib.rs"), code).unwrap();
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs","start":1,"end":390})),
        call("finish", json!({"summary":"inspected","unresolved":true})),
    ]);
    run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let second = &model.requests()[1].messages;
    let bytes = serde_json::to_vec(second).unwrap().len();
    assert!(
        bytes <= 8_000,
        "the last tool output still exceeds the context limit: {bytes}"
    );
    assert!(
        second
            .iter()
            .any(|m| m.text().contains("Full conversation archive"))
    );
}

#[test]
fn repeated_reads_retain_a_retrievable_artifact_after_compaction() {
    use loom_core::base::ArtifactStore;
    let (_directory, mut task) = task(10);
    task.context_limit = 8_000;
    let code = (0..390)
        .map(|i| format!("// {i}: abcdefghijklmnopqrstuvwxyz0123456789\n"))
        .collect::<String>();
    fs::write(task.overlay.dir().join("lib.rs"), code).unwrap();
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs","start":1,"end":250})),
        call("read", json!({"path":"lib.rs","start":251,"end":390})),
        call("read", json!({"path":"lib.rs","start":1,"end":250})),
        call("finish", json!({"summary":"done","unresolved":true})),
    ]);
    let mut first = None;
    let mut repeated = None;
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "read" && event["cached"] == false && first.is_none() {
            first = event["artifact"].as_object().cloned();
        }
        if event["tool"] == "read" && event["cached"] == true {
            repeated = Some(event["result"].as_str().unwrap().to_owned());
        }
    })
    .unwrap();
    let original = first.expect("original read artifact");
    let id = original["id"].as_str().unwrap();
    assert!(
        repeated.unwrap().contains(id),
        "a cached read needs a recoverable source"
    );
    let reference: loom_core::base::ArtifactRef = serde_json::from_value(json!(original)).unwrap();
    let saved = task.overlay.artifacts().get(&reference, None).unwrap();
    assert!(String::from_utf8(saved).unwrap().contains("// 0:"));
}

#[test]
fn multiple_compactions_archive_individually_readable_messages() {
    use loom_core::base::ArtifactStore;
    let (directory, mut task) = task(12);
    task.context_limit = 8_000;
    fs::write(
        task.overlay.dir().join("lib.rs"),
        (0..390)
            .map(|i| format!("// {i}: abcdefghijklmnopqrstuvwxyz0123456789\n"))
            .collect::<String>(),
    )
    .unwrap();
    let model = ScriptedModel::new(
        (0..6)
            .map(|i| {
                call(
                    "read",
                    json!({
                        "path":"lib.rs", "start":i * 55 + 1, "end":i * 55 + 55
                    }),
                )
            })
            .chain([call("finish", json!({"summary":"done","unresolved":true}))]),
    );
    run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let requests = model.requests();
    let last = &requests.last().unwrap().messages;
    let summary = last
        .iter()
        .find(|m| m.text().starts_with("Earlier work summary:"))
        .expect("latest compaction summary")
        .text();
    let id = summary
        .split("Full conversation archive (read_artifact): ")
        .nth(1)
        .expect("archived artifact id")
        .trim();
    let store = task.overlay.artifacts();
    let archive = std::fs::read(directory.path().join(".loom/artifacts").join(id)).unwrap();
    let records: Vec<serde_json::Value> = archive
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("one message per archive line"))
        .collect();
    assert!(records.len() >= 2);
    assert!(
        records
            .iter()
            .any(|v| v.to_string().contains("Earlier work summary:")),
        "later archives must retain the earlier archive chain"
    );
    let reference = store.put(&archive).unwrap();
    assert_eq!(reference.id, id);
}

#[test]
fn compaction_preserves_early_source_evidence_and_the_active_plan() {
    let (_directory, mut task) = task(12);
    task.context_limit = 8_000;
    fs::write(
        task.overlay.dir().join("target.rs"),
        "pub fn root_cause() { /* expired_token */ }\n",
    )
    .unwrap();
    let mut replies = vec![call("find", json!({"pattern":"expired_token"}))];
    replies.push(call(
        "update_plan",
        json!({"plan":[{"step":"fix expired token at the located source","status":"in_progress"}]}),
    ));
    for index in 0..7 {
        let path = format!("noise{index}.rs");
        fs::write(
            task.overlay.dir().join(&path),
            (0..100)
                .map(|n| format!("// unrelated line {n}: abcdefghijklmnopqrstuvwxyz0123456789\n"))
                .collect::<String>(),
        )
        .unwrap();
        replies.push(call("read", json!({"path":path,"start":1,"end":100})));
    }
    replies.push(call(
        "finish",
        json!({"summary":"need implementation","unresolved":true}),
    ));
    let model = ScriptedModel::new(replies);
    run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let requests = model.requests();
    let last = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        last.contains("target.rs:1") && last.contains("expired_token"),
        "compaction lost the original root-cause evidence: {last}"
    );
    assert!(
        last.contains("fix expired token at the located source"),
        "active plan was lost"
    );
}

#[test]
fn each_tool_call_has_a_distinct_start_and_result_even_in_one_batch() {
    let (_directory, mut task) = task(1);
    let mut reply = call("read", json!({"path":"lib.rs"}));
    reply.parts.push(Part::ToolCall {
        id: "another-read".into(),
        name: "read".into(),
        arguments: json!({"path":"lib.rs","start":1,"end":1}),
    });
    let model = ScriptedModel::new([reply]);
    let mut events = vec![];
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        events.push(event)
    })
    .unwrap();
    let starts: Vec<_> = events
        .iter()
        .filter_map(|e| e.get("tool_started"))
        .collect();
    let results: Vec<_> = events.iter().filter(|e| e["tool"] == "read").collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(results.len(), 2);
    assert_ne!(starts[0]["operation_id"], starts[1]["operation_id"]);
    for i in 0..2 {
        assert_eq!(starts[i]["operation_id"], results[i]["operation_id"]);
        assert!(
            results[i]["description"]
                .as_str()
                .unwrap()
                .contains("lib.rs")
        );
        let start = events
            .iter()
            .position(|e| e.get("tool_started") == Some(starts[i]))
            .unwrap();
        let end = events.iter().position(|e| e == results[i]).unwrap();
        assert!(start < end);
    }
    assert!(events.iter().any(|e| {
        e["progress"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("lib.rs"))
    }));
}

#[test]
fn nearing_limits_requests_a_conclusion_before_the_last_model_turn() {
    let (_directory, mut task) = task(4);
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs"})),
        call("outline", json!({"path":"lib.rs"})),
        call("find", json!({"pattern":"value"})),
        call("finish", json!({"summary":"not changed","unresolved":true})),
    ]);
    run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let requests = model.requests();
    assert!(
        requests[2]
            .messages
            .iter()
            .any(|m| m.text().contains("Conclude the current attempt"))
    );
}

#[test]
fn checks_follow_the_project_of_each_changed_file() {
    use loom_core::worker::{default_checks, effective_checks};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // A tooling package.json at the root (npm's stub test) and a Rust crate below it.
    std::fs::write(
        root.join("package.json"),
        r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("tool/src")).unwrap();
    std::fs::write(root.join("tool/Cargo.toml"), "[package]\nname='t'\n").unwrap();
    let defaults = default_checks(root);
    assert!(
        defaults.is_empty(),
        "a stub npm test is no validation: {defaults:?}"
    );
    let touched = [std::path::PathBuf::from("tool/src/lib.rs")].into();
    let checks = effective_checks(root, &defaults, &touched);
    assert_eq!(
        checks,
        [
            "cd 'tool' && cargo build --quiet",
            "cd 'tool' && cargo test --quiet -- --test-threads=1"
        ]
    );
    let explicit = vec!["make test".to_owned()];
    assert_eq!(
        effective_checks(root, &explicit, &touched),
        explicit,
        "user checks are kept"
    );
}

#[test]
fn html_escaped_edit_arguments_are_applied_unescaped() {
    let (_directory, mut task) = task(5);
    let original = std::fs::read_to_string(task.overlay.dir().join("lib.rs")).unwrap();
    let line = original.lines().next().unwrap().to_owned();
    std::fs::write(
        task.overlay.dir().join("lib.rs"),
        format!("// a && b\n{original}"),
    )
    .unwrap();
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs"})),
        call(
            "edit",
            json!({"path":"lib.rs","old":"// a &amp;&amp; b","new":"// a &amp;&amp; c"}),
        ),
        call("finish", json!({"summary":"edited"})),
    ]);
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let text = std::fs::read_to_string(task.overlay.dir().join("lib.rs")).unwrap();
    assert!(text.starts_with("// a && c\n"), "{text}");
    assert!(text.contains(&line));
    assert_eq!(result.status, WorkerStatus::Proposed);
}

#[cfg(unix)]
#[test]
fn starting_a_running_command_again_points_at_the_live_session() {
    let (_directory, mut task) = task(4);
    let model = ScriptedModel::new([
        call("exec_command", json!({"command":"sleep 3","yield_ms":50})),
        call("exec_command", json!({"command":"sleep 3","yield_ms":50})),
        call("finish", json!({"summary":"done","unresolved":true})),
    ]);
    run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let first = model.requests()[1].messages.last().unwrap().clone();
    let second = model.requests()[2].messages.last().unwrap().clone();
    let text = |m: &loom_core::models::Message| {
        m.parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolResult { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect::<String>()
    };
    assert!(text(&first).contains("still running"), "{}", text(&first));
    assert!(
        text(&second).contains("already running as session"),
        "{}",
        text(&second)
    );
}

#[test]
fn worker_checks_share_one_deadline_and_do_not_start_late_commands() {
    let (_directory, mut task) = task(2);
    task.budget.seconds = if cfg!(windows) { 2 } else { 1 };
    task.checks = if cfg!(windows) {
        vec![
            "echo first-check".into(),
            "ping -n 5 127.0.0.1 >nul".into(),
            "echo should-not-run > late-check.txt".into(),
        ]
    } else {
        vec![
            "true".into(),
            "sleep 2".into(),
            "printf should-not-run > late-check.txt".into(),
        ]
    };
    let model = ScriptedModel::new([
        call("check", json!({})),
        call("finish", json!({"summary":"checked"})),
    ]);
    let mut checks = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "check" {
            checks.push(event);
        }
    })
    .unwrap();
    assert!(
        !task.overlay.dir().join("late-check.txt").exists(),
        "a check started after the shared worker deadline"
    );
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0]["error"], true);
    let report: serde_json::Value =
        serde_json::from_str(checks[0]["result"].as_str().unwrap()).unwrap();
    assert_eq!(report["commands"].as_array().unwrap().len(), 2);
    assert_eq!(report["commands"][1]["status"], "TimedOut");
    assert!(
        checks[0]["result_summary"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
}

#[cfg(unix)]
#[test]
fn model_wait_is_deducted_from_a_late_terminal_command_deadline() {
    struct SlowModel(ScriptedModel);
    impl loom_core::models::ModelPort for SlowModel {
        fn complete(
            &self,
            need: loom_core::models::Need,
            request: &loom_core::models::Request,
        ) -> Result<loom_core::models::Served, loom_core::models::ProviderError> {
            std::thread::sleep(Duration::from_millis(600));
            loom_core::models::ModelPort::complete(&self.0, need, request)
        }
    }
    let (_directory, mut task) = task(1);
    task.budget.seconds = 1;
    let model = SlowModel(ScriptedModel::new([call(
        "exec_command",
        json!({
            "command":"sleep 0.6; printf should-not-run > late-command.txt", "yield_ms":10000, "timeout_seconds":30
        }),
    )]));
    let mut commands = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "exec_command" {
            commands.push(event);
        }
    })
    .unwrap();
    assert!(
        !task.overlay.dir().join("late-command.txt").exists(),
        "command received a fresh timeout after the model used most of it"
    );
    assert_eq!(commands.len(), 1);
    let report: serde_json::Value =
        serde_json::from_str(commands[0]["result"].as_str().unwrap()).unwrap();
    // A bounded PTY poll may return just before its supervisor reports the kill.
    // That is a live session, not a false failed exit; the late effect must still
    // be prevented and the worker must reap the session when the attempt ends.
    if report["exit_code"].is_null() {
        assert!(report["session_id"].as_u64().is_some());
    } else {
        assert_eq!(commands[0]["error"], true);
    }
}

#[cfg(unix)]
#[test]
fn failed_terminal_exit_is_reported_as_error_with_its_exit_code_and_output() {
    let (_directory, mut task) = task(2);
    let model = ScriptedModel::new([
        call(
            "exec_command",
            json!({"command":"printf 'specific failure\\n'; exit 7"}),
        ),
        call(
            "finish",
            json!({"summary":"command failed","unresolved":true}),
        ),
    ]);
    let mut commands = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "exec_command" {
            commands.push(event);
        }
    })
    .unwrap();
    assert_eq!(
        commands[0]["error"], true,
        "nonzero command exit was recorded as success"
    );
    let summary = commands[0]["result_summary"].as_str().unwrap();
    assert!(
        summary.contains('7') && summary.contains("specific failure"),
        "{summary}"
    );
    let request = model.requests().remove(1);
    assert!(request.messages.iter().flat_map(|message| &message.parts).any(|part|
        matches!(part, Part::ToolResult { is_error: true, output, .. } if output.contains("specific failure"))));
}

#[cfg(unix)]
#[test]
fn large_terminal_output_stays_valid_bounded_json_with_a_complete_raw_artifact() {
    use loom_core::base::ArtifactStore;
    let (_directory, mut task) = task(2);
    let command = "i=0; while [ $i -lt 600 ]; do printf 'abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789\\n'; i=$((i + 1)); done; printf 'TAIL-EVIDENCE\\n'";
    let model = ScriptedModel::new([
        call("exec_command", json!({"command":command})),
        call("finish", json!({"summary":"inspected","unresolved":true})),
    ]);
    let mut commands = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "exec_command" {
            commands.push(event);
        }
    })
    .unwrap();
    let text = commands[0]["result"].as_str().unwrap();
    let report: serde_json::Value =
        serde_json::from_str(text).expect("terminal result was clipped into invalid JSON");
    assert!(
        text.len() <= 12000,
        "model-facing terminal output exceeds its cap"
    );
    assert_eq!(report["output_truncated"], true);
    let artifact: loom_core::base::ArtifactRef =
        serde_json::from_value(report["artifact"].clone()).unwrap();
    let raw = task.overlay.artifacts().get(&artifact, None).unwrap();
    assert!(raw.len() > 32000);
    assert!(String::from_utf8_lossy(&raw).contains("TAIL-EVIDENCE"));
    assert_eq!(report["exit_code"], 0);
}

#[test]
fn transient_retry_backoff_stops_at_the_worker_deadline() {
    struct TimeoutModel;
    impl loom_core::models::ModelPort for TimeoutModel {
        fn complete(
            &self,
            _need: loom_core::models::Need,
            _request: &loom_core::models::Request,
        ) -> Result<loom_core::models::Served, loom_core::models::ProviderError> {
            std::thread::sleep(Duration::from_secs(1));
            Err(loom_core::models::ProviderError::new(
                loom_core::models::ErrorKind::Transient,
                "simulated read timeout",
            ))
        }
    }
    let (_directory, mut task) = task(4);
    task.budget.seconds = 1;
    let started = std::time::Instant::now();
    let result = run(&mut task, &TimeoutModel, &AtomicBool::new(false), |_| {}).unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(1600),
        "a retry slept after the worker deadline: {:?}",
        started.elapsed()
    );
    assert_eq!(result.steps, 1);
    assert!(result.activity_report.contains("simulated read timeout"));
}

#[test]
fn waiting_for_a_child_observes_parent_cancellation() {
    use std::sync::atomic::Ordering;
    struct Port {
        parent: ScriptedModel,
        entered: AtomicBool,
    }
    impl loom_core::models::ModelPort for Port {
        fn complete(
            &self,
            need: loom_core::models::Need,
            request: &loom_core::models::Request,
        ) -> Result<loom_core::models::Served, loom_core::models::ProviderError> {
            if request
                .messages
                .iter()
                .any(|message| message.text() == "waiting child")
            {
                self.entered.store(true, Ordering::SeqCst);
                let started = std::time::Instant::now();
                while !request.cancel.as_ref().unwrap().load(Ordering::SeqCst)
                    && started.elapsed() < Duration::from_secs(2)
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                return Err(loom_core::models::ProviderError::new(
                    loom_core::models::ErrorKind::Cancelled,
                    "child stopped",
                ));
            }
            loom_core::models::ModelPort::complete(&self.parent, need, request)
        }
    }
    let (_directory, mut task) = task(4);
    let cancel = AtomicBool::new(false);
    let model = Port {
        entered: AtomicBool::new(false),
        parent: ScriptedModel::new([
            call(
                "spawn_agent",
                json!({"goal":"waiting child","read_only":true}),
            ),
            call("wait_agent", json!({"id":"child-1","timeout_ms":30000})),
        ]),
    };
    let mut interrupted_at = None;
    let result = run(&mut task, &model, &cancel, |event| {
        if event["tool_started"]["tool"] == "wait_agent" {
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !model.entered.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(model.entered.load(Ordering::SeqCst));
            interrupted_at = Some(std::time::Instant::now());
            cancel.store(true, Ordering::SeqCst);
        }
    })
    .unwrap();
    assert!(
        interrupted_at.unwrap().elapsed() < Duration::from_millis(600),
        "wait_agent ignored the parent interrupt"
    );
    assert_eq!(result.summary, "cancelled");
    assert_eq!(result.status, WorkerStatus::Unresolved);
}

#[test]
fn editing_one_file_does_not_resend_an_unchanged_other_file() {
    let (_directory, mut task) = task(5);
    let helper = (0..200)
        .map(|n| format!("helper line {n}: abcdefghijklmnopqrstuvwxyz\n"))
        .collect::<String>();
    fs::write(task.overlay.dir().join("helper.txt"), &helper).unwrap();
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs"})),
        call("read", json!({"path":"helper.txt"})),
        call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
        call("read", json!({"path":"helper.txt"})),
        call("finish", json!({"summary":"edited"})),
    ]);
    let mut reads = Vec::new();
    let result = run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "read" && event["args"]["path"] == "helper.txt" {
            reads.push(event);
        }
    })
    .unwrap();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[0]["cached"], false);
    #[cfg(unix)]
    {
        assert_eq!(
            reads[1]["cached"], true,
            "an unrelated edit invalidated unchanged source evidence"
        );
        let before = reads[0]["result"].as_str().unwrap().len();
        let after = reads[1]["result"].as_str().unwrap().len();
        eprintln!("unchanged helper result bytes: {before} -> {after}");
        assert!(after < 350 && before > 7000);
    }
    #[cfg(not(unix))]
    assert_eq!(reads[1]["cached"], false);
    assert_eq!(
        fs::read_to_string(task.overlay.dir().join("helper.txt")).unwrap(),
        helper
    );
    assert_eq!(result.status, WorkerStatus::Proposed);
    assert!(
        result
            .patch
            .files
            .iter()
            .any(|file| file.path.to_str() == Some("lib.rs"))
    );
}

#[cfg(unix)]
#[test]
fn editing_a_file_invalidates_cached_reads_through_its_symlink_alias() {
    let (directory, mut task) = task(5);
    std::os::unix::fs::symlink("lib.rs", directory.path().join("alias.rs")).unwrap();
    task.overlay = Overlay::create(
        directory.path(),
        &Revision::capture(directory.path()).unwrap(),
    )
    .unwrap();
    let model = ScriptedModel::new([
        call("read", json!({"path":"alias.rs"})),
        call("read", json!({"path":"lib.rs"})),
        call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
        call("read", json!({"path":"alias.rs"})),
        call("finish", json!({"summary":"edited"})),
    ]);
    let mut reads = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "read" && event["args"]["path"] == "alias.rs" {
            reads.push(event);
        }
    })
    .unwrap();
    assert_eq!(reads.len(), 2);
    assert_eq!(reads[1]["cached"], false);
    assert!(reads[1]["result"].as_str().unwrap().contains("{ 2 }"));
}

#[cfg(unix)]
#[test]
fn arbitrary_commands_invalidate_cached_source_evidence() {
    let (_directory, mut task) = task(4);
    fs::write(task.overlay.dir().join("helper.txt"), "old helper\n").unwrap();
    let model = ScriptedModel::new([
        call("read", json!({"path":"helper.txt"})),
        call(
            "exec_command",
            json!({"command":"printf 'new helper\\n' > helper.txt"}),
        ),
        call("read", json!({"path":"helper.txt"})),
        call("finish", json!({"summary":"edited"})),
    ]);
    let mut reads = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "read" {
            reads.push(event);
        }
    })
    .unwrap();
    assert_eq!(reads[1]["cached"], false);
    assert!(reads[1]["result"].as_str().unwrap().contains("new helper"));
}

#[cfg(unix)]
#[test]
fn editing_a_file_invalidates_cached_reads_through_its_hardlink_alias() {
    let (_directory, mut task) = task(5);
    fs::hard_link(
        task.overlay.dir().join("lib.rs"),
        task.overlay.dir().join("alias.rs"),
    )
    .unwrap();
    let model = ScriptedModel::new([
        call("read", json!({"path":"alias.rs"})),
        call("read", json!({"path":"lib.rs"})),
        call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
        call("read", json!({"path":"alias.rs"})),
        call("finish", json!({"summary":"edited"})),
    ]);
    let mut reads = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "read" && event["args"]["path"] == "alias.rs" {
            reads.push(event);
        }
    })
    .unwrap();
    assert_eq!(
        reads[1]["cached"], false,
        "file effects include hardlink aliases"
    );
    assert!(reads[1]["result"].as_str().unwrap().contains("{ 2 }"));
}

#[test]
fn invalid_search_regex_is_an_error_with_guidance_and_literal_retry_can_succeed() {
    let (_directory, mut task) = task(3);
    let model = ScriptedModel::new([
        call("find", json!({"pattern":"value("})),
        call("find", json!({"pattern":"value(","literal":true})),
        call(
            "finish",
            json!({"summary":"found the source","unresolved":true}),
        ),
    ]);
    let mut searches = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "find" {
            searches.push(event);
        }
    })
    .unwrap();
    assert_eq!(searches.len(), 2);
    assert_eq!(
        searches[0]["error"], true,
        "a broken search was presented as no matches"
    );
    let error = searches[0]["result"].as_str().unwrap();
    assert!(
        error.contains("regex") && error.contains("literal=true"),
        "{error}"
    );
    assert_eq!(searches[1]["error"], false);
    assert!(searches[1]["result"].as_str().unwrap().contains("lib.rs"));
}

#[test]
fn an_unknown_tool_does_not_invalidate_observed_source() {
    let (_directory, mut task) = task(4);
    let model = ScriptedModel::new([
        call("read", json!({"path":"lib.rs"})),
        call("repo_browser.find", json!({"pattern":"value"})),
        call("read", json!({"path":"lib.rs"})),
        call(
            "finish",
            json!({"summary":"still unresolved","unresolved":true}),
        ),
    ]);
    let mut reads = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "read" {
            reads.push(event);
        }
    })
    .unwrap();
    assert_eq!(
        reads[1]["cached"], true,
        "a rejected unknown call cannot mutate the copy"
    );
}

#[test]
fn core_recovery_guidance_can_turn_repeated_reads_into_a_real_patch() {
    struct Port {
        reads: ScriptedModel,
        repair: ScriptedModel,
    }
    impl loom_core::models::ModelPort for Port {
        fn complete(
            &self,
            need: loom_core::models::Need,
            request: &loom_core::models::Request,
        ) -> Result<loom_core::models::Served, loom_core::models::ProviderError> {
            let model = if request.messages.iter().any(|message| {
                message
                    .text()
                    .contains("Repeated tool calls produced no new evidence")
            }) {
                &self.repair
            } else {
                &self.reads
            };
            loom_core::models::ModelPort::complete(model, need, request)
        }
    }
    let (_directory, mut task) = task(6);
    let model = Port {
        reads: ScriptedModel::new((0..6).map(|_| call("read", json!({"path":"lib.rs"})))),
        repair: ScriptedModel::new([
            call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
            call("finish", json!({"summary":"fixed"})),
        ]),
    };
    let mut progress = Vec::new();
    let result = run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["progress"]["phase"] == "recovery" {
            progress.push(event);
        }
    })
    .unwrap();
    assert_eq!(
        result.status,
        WorkerStatus::Proposed,
        "repeated reads ended before Core supplied corrective guidance"
    );
    assert_eq!(result.steps, 5);
    assert_eq!(
        progress.len(),
        1,
        "recovery must be bounded, not another loop"
    );
    assert!(
        fs::read_to_string(task.overlay.dir().join("lib.rs"))
            .unwrap()
            .contains("{ 2 }")
    );
}

#[test]
fn repeated_unknown_tool_errors_receive_one_warning_and_stop() {
    let (_directory, mut task) = task(8);
    let model =
        ScriptedModel::new((0..8).map(|_| call("repo_browser.find", json!({"pattern":"value"}))));
    let mut warnings = 0;
    let result = run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["progress"]["phase"] == "recovery" {
            warnings += 1;
        }
    })
    .unwrap();
    assert_eq!(result.status, WorkerStatus::Unresolved);
    assert_eq!(result.summary, "exploration stalled");
    assert_eq!(
        result.steps, 4,
        "unknown tools cannot consume all eight calls"
    );
    assert_eq!(warnings, 1);
}

#[test]
fn rejected_tool_generation_recovers_once_without_retrying_other_bad_requests() {
    use loom_core::models::{ErrorKind, ModelPort, ProviderError, Request, Served};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Port {
        calls: AtomicUsize,
        code: &'static str,
        persistent: bool,
        script: ScriptedModel,
    }
    impl ModelPort for Port {
        fn complete(
            &self,
            need: loom_core::models::Need,
            request: &Request,
        ) -> Result<Served, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 1 || (call > 1 && self.persistent) {
                return Err(ProviderError::new(
                    ErrorKind::BadRequest,
                    format!(
                        "HTTP 400: {}",
                        json!({"error":{"code":self.code,"message":"rejected generation"}})
                    ),
                ));
            }
            if call > 1 {
                assert!(request.messages.iter().any(|message| {
                    message
                        .text()
                        .contains("The provider rejected your generated tool call")
                }));
            }
            self.script.complete(need, request)
        }
    }
    for (code, persistent, expected_steps, proposed) in [
        ("tool_use_failed", false, 4, true),
        ("invalid_request_error", false, 2, false),
        ("tool_use_failed", true, 3, false),
    ] {
        let (_directory, mut task) = task(6);
        let model = Port {
            calls: AtomicUsize::new(0),
            code,
            persistent,
            script: ScriptedModel::new([
                call("read", json!({"path":"lib.rs"})),
                call("edit", json!({"path":"lib.rs","old":"{ 1 }","new":"{ 2 }"})),
                call("finish", json!({"summary":"fixed"})),
            ]),
        };
        let mut warnings = 0;
        let result = run(&mut task, &model, &AtomicBool::new(false), |event| {
            if event["progress"]["phase"] == "tool_recovery" {
                warnings += 1;
            }
        })
        .unwrap();
        assert_eq!(
            result.steps, expected_steps,
            "{code}, persistent={persistent}"
        );
        assert_eq!(result.status == WorkerStatus::Proposed, proposed);
        assert_eq!(warnings, usize::from(code == "tool_use_failed"));
    }
}

#[cfg(unix)]
#[test]
fn source_reads_are_not_reused_while_a_terminal_can_still_change_them() {
    let (_directory, mut task) = task(4);
    let overlay = task.overlay.dir().to_path_buf();
    fs::write(overlay.join("helper.rs"), "old evidence\n").unwrap();
    let model = ScriptedModel::new([
        call(
            "exec_command",
            json!({"command":"while [ ! -f release-command ]; do sleep 0.01; done; printf 'new evidence\n' > helper.rs; touch command-updated","yield_ms":1}),
        ),
        call("read", json!({"path":"helper.rs"})),
        call("read", json!({"path":"helper.rs"})),
        call("finish", json!({"summary":"observed","unresolved":true})),
    ]);
    let mut reads = Vec::new();
    run(&mut task, &model, &AtomicBool::new(false), |event| {
        if event["tool"] == "read" {
            reads.push(event);
            if reads.len() == 1 {
                fs::write(overlay.join("release-command"), "go").unwrap();
                let start = std::time::Instant::now();
                while !overlay.join("command-updated").exists() {
                    assert!(start.elapsed() < Duration::from_secs(5));
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    })
    .unwrap();
    assert!(
        reads[0]["result"]
            .as_str()
            .unwrap()
            .contains("old evidence")
    );
    assert_eq!(
        reads[1]["cached"], false,
        "the running terminal can invalidate source without a new tool call"
    );
    assert!(
        reads[1]["result"]
            .as_str()
            .unwrap()
            .contains("new evidence")
    );
}

#[test]
fn children_inherit_the_selected_parent_model_and_share_its_call_limit() {
    use loom_core::models::{ModelPort, Need, ProviderError, Request, Served};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    struct Port {
        parent: ScriptedModel,
        calls: AtomicUsize,
        child_models: Mutex<Vec<String>>,
    }
    impl ModelPort for Port {
        fn complete(&self, need: Need, request: &Request) -> Result<Served, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if request
                .messages
                .iter()
                .any(|message| message.text().starts_with("budget child"))
            {
                self.child_models
                    .lock()
                    .unwrap()
                    .push(request.model.clone());
                return ScriptedModel::new([call("read", json!({"path":"lib.rs"}))])
                    .complete(need, request);
            }
            self.parent.complete(need, request)
        }
    }
    let (_directory, mut task) = task(5);
    task.model = Some("selected-parent-model".into());
    let model = Port {
        calls: AtomicUsize::new(0),
        child_models: Mutex::new(vec![]),
        parent: ScriptedModel::new([
            call(
                "spawn_agent",
                json!({"goal":"budget child 1","read_only":true}),
            ),
            call(
                "spawn_agent",
                json!({"goal":"budget child 2","read_only":true}),
            ),
            call("wait_agent", json!({"id":"child-1"})),
            call("wait_agent", json!({"id":"child-2"})),
            call("finish", json!({"summary":"done","answer":true})),
        ]),
    };
    let result = run(&mut task, &model, &AtomicBool::new(false), |_| {}).unwrap();
    let children = model.child_models.lock().unwrap();
    assert!(!children.is_empty());
    assert!(
        children
            .iter()
            .all(|model| model == "selected-parent-model"),
        "{children:?}"
    );
    assert!(
        model.calls.load(Ordering::SeqCst) <= 5,
        "each child received a fresh call budget"
    );
    assert_eq!(
        result.steps,
        model.calls.load(Ordering::SeqCst),
        "count all actual parent and child calls"
    );
    assert_eq!(result.input_tokens, result.steps as u64 * 10);
}
