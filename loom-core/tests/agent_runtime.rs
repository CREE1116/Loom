mod isolation;
use loom_core::{
    base::Revision,
    models::{
        self, Message, ModelPort, Need, Part, Request, Response, ScriptedModel, Served, Stop, Usage,
    },
};
use loom_core::agent::{
    Core, Intent, Options, Status,
    orchestration::{Plan, Unit},
    query::{Find, FindKind, Operator, Relation, Sets},
    world::WorldHub,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};
fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .arg(d.path())
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir(d.path().join("src")).unwrap();
    fs::write(d.path().join("src/a.rs"), "pub fn alpha() -> u32 { 1 }\n").unwrap();
    fs::write(d.path().join("src/b.rs"), "pub fn beta() -> u32 { 2 }\n").unwrap();
    fs::write(
        d.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    assert!(
        Command::new("git")
            .current_dir(d.path())
            .args(["add", "."])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .current_dir(d.path())
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "fixture"
            ])
            .status()
            .unwrap()
            .success()
    );
    d
}
fn response(calls: Vec<(&str, Value)>) -> Response {
    Response {
        parts: calls
            .into_iter()
            .enumerate()
            .map(|(i, (n, a))| Part::ToolCall {
                id: format!("c{i}"),
                name: n.into(),
                arguments: a,
            })
            .collect(),
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
fn options() -> Options {
    Options {
        checks: vec!["git diff --check".into()],
        seconds: 30,
        validation_reserve: 0,
        ..Default::default()
    }
}
fn cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
#[test]
fn default_token_budget_allows_progress_after_160k_and_explicit_budget_still_stops() {
    let d = repo();
    let mut first = response(vec![("open", json!({"target":"src/a.rs"}))]);
    first.usage.input = Some(164809);
    first.usage.output = Some(2317);
    first.usage.cached_input = Some(55424);
    for (tokens, expected) in [(Options::default().tokens, Status::Inspected), (160000, Status::Unresolved)] {
        let mut core = Core::open(d.path()).unwrap();
        let model = ScriptedModel::new([
            first.clone(),
            response(vec![("finish", json!({"summary":"source inspected"}))]),
        ]);
        let task = core.execute("inspect alpha", Intent::Inspect,
            Options { tokens, ..options() }, &model, cancel(), &|_| {}).unwrap();
        assert_eq!(task.status, expected, "{}", task.summary);
        assert_eq!(task.calls, if expected == Status::Inspected { 2 } else { 1 });
        assert!(task.usage.input.unwrap() >= 164809);
        if expected == Status::Unresolved {
            assert_eq!(task.summary, "observed token budget exhausted");
        }
    }
}

#[test]
fn task_compaction_preserves_attached_image_and_current_objective() {
    let d = repo();
    let image = Part::Image { mime_type:"image/png".into(), data:"base64-image-fixture".repeat(2000) };
    let model = ScriptedModel::new([
        response(vec![("open",json!({"target":"src/a.rs"}))]),
        response(vec![("open",json!({"target":"src/b.rs"}))]),
        response(vec![("open",json!({"target":"Cargo.toml"}))]),
        response(vec![("finish",json!({"summary":"inspected image and code"}))]),
    ]);
    let mut core = Core::open(d.path()).unwrap();
    let task = core.execute("explain this image",Intent::Inspect,
        Options { images:vec![image.clone()], context_bytes:500, ..options() },
        &model,cancel(),&|_| {}).unwrap();
    assert_eq!(task.status,Status::Inspected,"{}",task.summary);
    for request in model.requests() {
        assert!(request.messages.iter().flat_map(|message| &message.parts).any(|part| part == &image));
        assert!(request.messages.iter().any(|message| message.text().contains("explain this image")));
    }
    assert!(model.requests()[3].messages.iter().any(|message| message.text().contains("Compacted observations")));
}

#[test]
fn empty_search_scope_means_repository_and_named_scope_stays_restricted() {
    let d = repo();
    let view = WorldHub::default().view(d.path()).unwrap();
    let mut sets = Sets::default();
    for kind in [FindKind::Path, FindKind::Symbol, FindKind::Text] {
        let query = if matches!(kind, FindKind::Path) { "src/a.rs" } else { "alpha" };
        let baseline = sets.find(&view, Find { query:query.into(), kind, scope:None }).unwrap();
        let empty = sets.find(&view, Find { query:query.into(), kind, scope:Some(String::new()) }).unwrap();
        assert_eq!(empty, baseline);
        let excluded = sets.find(&view, Find { query:query.into(), kind, scope:Some("src/b.rs".into()) }).unwrap();
        assert_eq!(excluded["count"], 0);
    }
    assert!(sets.find(&view, Find { query:"alpha".into(), kind:FindKind::Symbol,
        scope:Some("missing".into()) }).is_err());
}

#[test]
fn opening_short_files_clamps_end_and_reports_actual_range() {
    let d = repo();
    fs::write(d.path().join("README.md"), "first\nsecond\nthird\n").unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("open", json!({"target":"README.md","start":0,"end":300}))]),
        response(vec![("open", json!({"target":"README.md","start":1}))]),
        response(vec![("open", json!({"target":"README.md","end":2}))]),
        response(vec![("open", json!({"target":"README.md","start":4,"end":5}))]),
        response(vec![("open", json!({"target":"README.md","start":2,"end":1}))]),
        response(vec![("open", json!({"target":"README.md","start":0,"end":401}))]),
        response(vec![("finish", json!({"summary":"inspected"}))]),
    ]);
    let events = Mutex::new(Vec::new());
    core.execute("inspect read ranges", Intent::Inspect, options(), &model, cancel(),
        &|e| { if e.detail["tool"] == "open" && e.detail["completed"] == true {
            events.lock().unwrap().push(e);
        } }).unwrap();
    let events = events.into_inner().unwrap();
    assert_eq!(events.len(), 6);
    for (index, start, end, source) in [(0,0,3,"first\nsecond\nthird"), (1,1,3,"second\nthird"), (2,0,2,"first\nsecond")] {
        assert!(!events[index].failed, "{:?}", events[index]);
        let result = &events[index].detail["result"][0];
        assert_eq!(result["start"], start);
        assert_eq!(result["end"], end);
        assert_eq!(result["lines"], 3);
        assert_eq!(result["source"], source);
    }
    assert!(events[3..].iter().all(|event| event.failed));
}

#[test]
fn assistant_commentary_precedes_tools_and_question_cancel_is_not_an_answer() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let control = loom_core::agent::InputControl::default();
    let mut first = response(vec![("find", json!({"query":"alpha"}))]);
    first.parts.insert(0, Part::Text("핵심 함수를 찾고 연결된 파일을 확인하겠습니다.".into()));
    let model = ScriptedModel::new([
        first,
        response(vec![("ask", json!({"question":"이름을 정해주세요"}))]),
        response(vec![("finish", json!({"summary":"질문 취소를 반영해 기존 이름을 유지했습니다."}))]),
    ]);
    let events = Mutex::new(Vec::new());
    let task = core.execute("inspect alpha", Intent::Inspect, Options {control: control.clone(), steps: 3, ..options()}, &model, cancel(), &|event| {
        if let Some(id) = event.detail["input_request"]["id"].as_str() { control.cancel(id.into()); }
        events.lock().unwrap().push(event);
    }).unwrap();
    assert_eq!(task.status, Status::Inspected);
    let events = events.into_inner().unwrap();
    let comment = events.iter().position(|event| event.target == "commentary").expect("tool-call commentary must be observable");
    let tool = events.iter().position(|event| event.detail["tool"] == "find" && event.detail["started"] == true).unwrap();
    assert!(comment < tool);
    assert!(events[comment].detail["text"].as_str().unwrap().contains("핵심 함수"));
    assert!(model.requests()[2].messages.iter().flat_map(|m| &m.parts).any(|part|
        matches!(part, Part::ToolResult {output, ..} if output.contains("cancelled") && !output.contains("\"answer\""))));
}

#[test]
fn large_ast_types_open_as_members_and_explicit_ranges_read_only_that_slice() {
    let d = repo();
    let mut source = String::from("package fixtures;\n\npublic class Sample {\n");
    for method in 0..12 {
        source.push_str(&format!("  public int method{method}() {{\n"));
        for line in 0..10 { source.push_str(&format!("    // method{method} evidence line {line} with enough content to make a full class expensive\n")); }
        source.push_str(&format!("    return {method};\n  }}\n"));
    }
    source.push_str("}\n");
    fs::write(d.path().join("Sample.java"), &source).unwrap();
    let view = WorldHub::default().view(d.path()).unwrap();
    let class = view.world.graph.symbols().iter().find(|s| s.name == "Sample").unwrap().id.clone();
    let method = view.world.graph.symbols().iter().find(|s| s.name == "method0").unwrap().id.clone();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("find",json!({"kind":"symbol","query":"method0"}))]),
        response(vec![("open",json!({"target":class}))]),
        response(vec![("open",json!({"target":class,"start":30,"end":36}))]),
        response(vec![("open",json!({"target":class,"start":0,"end":8}))]),
        response(vec![("open",json!({"target":method}))]),
        response(vec![("finish",json!({"summary":"inspected methods"}))]),
    ]);
    let events = Mutex::new(Vec::new());
    core.execute("inspect Sample",Intent::Inspect,options(),&model,cancel(),&|event| {
        if event.detail["completed"] == true { events.lock().unwrap().push(event); }
    }).unwrap();
    let events = events.into_inner().unwrap();
    let target = &events[0].detail["result"]["representatives"][0];
    assert!(target["start"].is_number() && target["end"].is_number());
    let outline = &events[1].detail["result"][0];
    assert_eq!(outline["view"], "outline");
    assert!(outline.get("source").is_none());
    assert_eq!(outline["select"]["count"], 12);
    assert!(outline.to_string().len() * 3 < source.len(), "outline should be smaller than a third of the class");
    let slice = &events[2].detail["result"][0];
    assert_eq!(slice["start"], 30);
    assert_eq!(slice["end"], 36);
    assert_eq!(slice["source"], source.lines().skip(30).take(6).collect::<Vec<_>>().join("\n"));
    let intersection = &events[3].detail["result"][0];
    assert_eq!(intersection["start"], 2);
    assert_eq!(intersection["end"], 8);
    assert_eq!(intersection["source"], source.lines().skip(2).take(6).collect::<Vec<_>>().join("\n"));
    let leaf = events[4].detail["result"][0]["source"].as_str().unwrap();
    assert!(leaf.contains("method0") && !leaf.contains("method1"));
}

#[test]
fn repeated_source_keeps_one_copy_in_model_context_and_complete_durable_events() {
    let d = repo();
    let source = format!("pub fn alpha() -> u32 {{\n{}1\n}}", "// Current source evidence must remain available without repeated copies in model inputs.\n".repeat(40));
    fs::write(d.path().join("src/a.rs"), &source).unwrap();
    let model = ScriptedModel::new([
        response(vec![("open",json!({"target":"src/a.rs"}))]),
        response(vec![("open",json!({"target":"src/a.rs"}))]),
        response(vec![("finish",json!({"summary":"inspected alpha"}))]),
    ]);
    let mut core = Core::open(d.path()).unwrap();
    let events = Mutex::new(Vec::new());
    core.execute("inspect alpha",Intent::Inspect,options(),&model,cancel(),&|event| {
        if event.detail["tool"] == "open" && event.detail["completed"] == true {
            events.lock().unwrap().push(event);
        }
    }).unwrap();
    let requests = model.requests();
    let encoded_source = serde_json::to_string(&source).unwrap();
    let count = requests[2].messages.iter().flat_map(|message| &message.parts)
        .filter(|part| matches!(part,Part::ToolResult {output,..} if output.contains(&encoded_source))).count();
    assert_eq!(count, 1, "duplicate source must not be billed again in each later request");
    let events = events.into_inner().unwrap();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| event.detail["result"][0]["source"] == source));
    let mut duplicated = requests[2].clone();
    for part in duplicated.messages.iter_mut().flat_map(|message| &mut message.parts) {
        if let Part::ToolResult {output,..} = part
            && output.starts_with("Identical source is retained")
        {
            *output = events[0].detail["result"].to_string();
        }
    }
    let actual = models::router::estimate_tokens(&requests[2]);
    let repeated = models::router::estimate_tokens(&duplicated);
    assert!(actual + 500 < repeated, "expected to avoid at least 500 estimated input tokens: {actual} vs {repeated}");
}
#[test]
fn query_sets_are_complete_and_revision_pinned() {
    let d = repo();
    let hub = WorldHub::default();
    let view = hub.view(d.path()).unwrap();
    let mut sets = Sets::default();
    let a = sets
        .find(
            &view,
            Find {
                query: "alpha".into(),
                kind: FindKind::Symbol,
                scope: None,
            },
        )
        .unwrap();
    let b = sets
        .find(
            &view,
            Find {
                query: "beta".into(),
                kind: FindKind::Symbol,
                scope: None,
            },
        )
        .unwrap();
    let merged = sets
        .combine(
            &view,
            Operator::Union,
            &[
                a["set"].as_str().unwrap().into(),
                b["set"].as_str().unwrap().into(),
            ],
        )
        .unwrap();
    let handle = merged["set"].as_str().unwrap();
    assert_eq!(merged["count"], 2);
    assert_eq!(
        sets.project(&view, handle, 1, 0).unwrap()["representatives"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(sets.get(&view, handle).unwrap().nodes.len(), 2);
    let files = sets.expand(&view, handle, Relation::Files).unwrap();
    assert_eq!(files["count"], 2);
    let packages = sets
        .expand(&view, files["set"].as_str().unwrap(), Relation::Package)
        .unwrap();
    assert_eq!(packages["count"], 1);
    fs::write(d.path().join("src/a.rs"), "pub fn gamma() {}\n").unwrap();
    let new = hub.view(d.path()).unwrap();
    assert!(
        sets.get(&new, handle)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    assert!(view.world.graph.get("src/a.rs::alpha").is_some());
    assert!(new.world.graph.get("src/a.rs::gamma").is_some());
}

#[test]
fn live_world_refresh_observes_edits_and_refuses_cancelled_work() {
    use std::sync::atomic::Ordering;
    let d = repo();
    let hub = WorldHub::default();
    let flag = AtomicBool::new(false);
    let first = hub.view_cancellable(d.path(), &flag).unwrap();
    fs::write(d.path().join("src/a.rs"), "pub fn after_refresh() {}\n").unwrap();
    let next = hub.view_cancellable(d.path(), &flag).unwrap();
    assert_ne!(first.revision(), next.revision());
    assert!(next.world.graph.get("src/a.rs::after_refresh").is_some());
    assert!(first.world.graph.get("src/a.rs::alpha").is_some());
    flag.store(true, Ordering::Release);
    assert!(hub.view_cancellable(d.path(), &flag).is_err());
}
#[test]
fn dag_rejects_cycles_bad_paths_and_duplicate_ids() {
    let u = |id: &str, deps: Vec<&str>| Unit {
        id: id.into(),
        goal: id.into(),
        write_files: vec![PathBuf::from("a.rs")],
        depends_on: deps.into_iter().map(Into::into).collect(),
    };
    assert!(
        Plan {
            units: vec![u("a", vec!["b"]), u("b", vec!["a"])]
        }
        .validate()
        .is_err()
    );
    assert!(
        Plan {
            units: vec![u("a", vec![]), u("a", vec![])]
        }
        .validate()
        .is_err()
    );
    let mut invalid = u("a", vec![]);
    invalid.write_files = vec!["../escape".into()];
    assert!(
        Plan {
            units: vec![invalid]
        }
        .validate()
        .is_err()
    );
}
#[test]
fn direct_proposal_stays_isolated_and_approval_rechecks() {
    let d = repo();
    let initial = Revision::capture(d.path()).unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("open", json!({"target":"src/a.rs::alpha"}))]),
        response(vec![
            (
                "change",
                json!({"op":"edit","target":"src/a.rs::alpha","old":"1","new":"3"}),
            ),
            ("finish", json!({"summary":"updated alpha"})),
        ]),
    ]);
    let t = core
        .execute(
            "edit alpha",
            Intent::Edit,
            options(),
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(t.status, Status::AwaitingApproval);
    assert_eq!(Revision::capture(d.path()).unwrap(), initial);
    let names: Vec<_> = model.requests()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert!(names.contains(&"agent".into()));
    assert!(!names.contains(&"plan".into()));
    assert!(!names.contains(&"spawn_agent".into()));
    let t = core
        .approve(&t.id, &AtomicBool::new(false), &|_| {})
        .unwrap();
    assert_eq!(t.status, Status::Applied);
    assert!(
        fs::read_to_string(d.path().join("src/a.rs"))
            .unwrap()
            .contains("3")
    );
    assert!(
        core.approve(&t.id, &AtomicBool::new(false), &|_| {})
            .is_err()
    );
}
#[test]
fn moved_user_revision_is_preserved() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([response(vec![
        (
            "change",
            json!({"op":"create","target":"created.txt","content":"proposal"}),
        ),
        ("finish", json!({"summary":"created"})),
    ])]);
    let t = core
        .execute(
            "create file",
            Intent::Edit,
            options(),
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    fs::write(d.path().join("user.txt"), "user data").unwrap();
    assert!(
        core.approve(&t.id, &AtomicBool::new(false), &|_| {})
            .is_err()
    );
    assert!(!d.path().join("created.txt").exists());
    assert_eq!(
        fs::read_to_string(d.path().join("user.txt")).unwrap(),
        "user data"
    );
    assert!(
        Core::open(d.path())
            .unwrap()
            .history()
            .unwrap()
            .iter()
            .any(|saved| saved.id == t.id && saved.pending())
    );
}
#[test]
fn failure_cannot_be_followed_by_success_in_same_batch() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([response(vec![
        (
            "change",
            json!({"op":"edit","target":"src/a.rs","old":"1","new":"7"}),
        ),
        ("finish", json!({"summary":"done"})),
    ])]);
    let t = core
        .execute("edit", Intent::Edit, options(), &model, cancel(), &|_| {})
        .unwrap();
    assert_eq!(t.status, Status::Unresolved);
    assert!(t.patch.files.is_empty());
}
#[test]
fn readonly_and_missing_validation_never_apply() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([response(vec![
        (
            "change",
            json!({"op":"create","target":"bad.txt","content":"bad"}),
        ),
        ("finish", json!({"summary":"done"})),
    ])]);
    let t = core
        .execute(
            "inspect",
            Intent::Inspect,
            options(),
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(t.status, Status::Unresolved);
    assert!(t.patch.files.is_empty());
    let model = ScriptedModel::new([response(vec![
        (
            "change",
            json!({"op":"create","target":"new.txt","content":"new"}),
        ),
        ("finish", json!({"summary":"done"})),
    ])]);
    let t = core
        .execute(
            "edit",
            Intent::Edit,
            Options {
                checks: vec!["exit 1".into()],
                ..options()
            },
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(t.status, Status::Unresolved);
    assert!(!d.path().join("new.txt").exists());
}
struct DagModel {
    requests: Mutex<Vec<Request>>,
}
impl ModelPort for DagModel {
    fn complete(&self, need: Need, r: &Request) -> Result<Served, models::ProviderError> {
        self.requests.lock().unwrap().push(r.clone());
        let first = r.messages.first().map(Message::text).unwrap_or_default();
        let have = |name: &str| {
            r.messages
                .iter()
                .flat_map(|m| &m.parts)
                .any(|p| matches!(p,Part::ToolCall{name:n,..} if n==name))
        };
        let calls = if first.contains("whole") {
            if have("agent") {
                vec![("finish", json!({"summary":"all units complete"}))]
            } else {
                vec![(
                    "agent",
                    json!({"units":[{"id":"a","goal":"create a","write_files":["a.txt"],"depends_on":[]},{"id":"b","goal":"read a and create b","write_files":["b.txt"],"depends_on":["a"]}]}),
                )]
            }
        } else if first.contains("create a") {
            vec![
                (
                    "change",
                    json!({"op":"create","target":"a.txt","content":"published predecessor"}),
                ),
                ("finish", json!({"summary":"a created"})),
            ]
        } else if !have("open") {
            vec![("open", json!({"target":"a.txt"}))]
        } else {
            assert!(r.messages.iter().any(|m|m.parts.iter().any(|p|matches!(p,Part::ToolResult{output,is_error:false,..} if output.contains("published predecessor")))));
            vec![
                (
                    "change",
                    json!({"op":"create","target":"b.txt","content":"depends on a"}),
                ),
                ("finish", json!({"summary":"b created"})),
            ]
        };
        ScriptedModel::new([response(calls)]).complete(need, r)
    }
}
#[test]
fn plan_switches_main_to_orchestration_and_successor_reads_merged_world() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = DagModel {
        requests: Default::default(),
    };
    let events = Mutex::new(Vec::new());
    let t = core
        .execute(
            "whole",
            Intent::Edit,
            Options {
                plan: true,
                ..options()
            },
            &model,
            cancel(),
            &|e| events.lock().unwrap().push(e),
        )
        .unwrap();
    assert_eq!(t.status, Status::AwaitingApproval, "{}", t.summary);
    assert!(t.delegated);
    assert_eq!(t.patch.files.len(), 2);
    assert!(!d.path().join("a.txt").exists());
    let requests = model.requests.lock().unwrap();
    let child: Vec<_> = requests
        .iter()
        .filter(|r| r.system.as_ref().unwrap().contains("cannot delegate"))
        .collect();
    assert!(!child.is_empty());
    assert!(
        child
            .iter()
            .all(|r| !r.tools.iter().any(|t| t.name == "agent" || t.name == "ask"))
    );
    let last = requests.last().unwrap();
    assert!(!last.tools.iter().any(|t| t.name == "change"));
    drop(requests);
    let t = core
        .approve(&t.id, &AtomicBool::new(false), &|_| {})
        .unwrap();
    assert_eq!(t.status, Status::Applied);
    assert!(d.path().join("b.txt").exists());
}
#[test]
fn source_mutating_validator_is_rejected() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([response(vec![
        (
            "change",
            json!({"op":"create","target":"new.txt","content":"new"}),
        ),
        ("finish", json!({"summary":"done"})),
    ])]);
    let result = core.execute(
        "edit",
        Intent::Edit,
        Options {
            checks: vec![if cfg!(windows) {
                "echo corrupt > src/a.rs"
            } else {
                "printf corrupt > src/a.rs"
            }
            .into()],
            ..options()
        },
        &model,
        cancel(),
        &|_| {},
    );
    assert_eq!(result.unwrap().status, Status::Unresolved);
    assert!(
        fs::read_to_string(d.path().join("src/a.rs"))
            .unwrap()
            .contains("alpha")
    );
}

struct DynamicModel {
    b_initial: std::sync::atomic::AtomicUsize,
}
impl ModelPort for DynamicModel {
    fn complete(&self, need: Need, r: &Request) -> Result<Served, models::ProviderError> {
        let goal = r.messages[0].text();
        let have = |name: &str| {
            r.messages
                .iter()
                .flat_map(|m| &m.parts)
                .any(|p| matches!(p,Part::ToolCall{name:n,..} if n==name))
        };
        let calls = if goal.contains("dynamic whole") {
            if have("agent") {
                vec![("finish", json!({"summary":"merged"}))]
            } else {
                vec![(
                    "agent",
                    json!({"units":[{"id":"a","goal":"change alpha","write_files":["src/a.rs"],"depends_on":[]},{"id":"b","goal":"copy alpha","write_files":["b.txt"],"depends_on":[]}]}),
                )]
            }
        } else if goal.contains("change alpha") {
            if !have("open") {
                vec![("open", json!({"target":"src/a.rs"}))]
            } else {
                vec![
                    (
                        "change",
                        json!({"op":"edit","target":"src/a.rs","old":"1","new":"9"}),
                    ),
                    ("finish", json!({"summary":"alpha changed"})),
                ]
            }
        } else if !have("open") {
            self.b_initial
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            vec![("open", json!({"target":"src/a.rs"}))]
        } else {
            let fresh = r
                .messages
                .iter()
                .flat_map(|m| &m.parts)
                .any(|p| matches!(p,Part::ToolResult{output,..} if output.contains("9")));
            vec![
                (
                    "change",
                    json!({"op":"create","target":"b.txt","content":if fresh{"fresh 9"}else{"stale 1"}}),
                ),
                ("finish", json!({"summary":"copied"})),
            ]
        };
        ScriptedModel::new([response(calls)]).complete(need, r)
    }
}
#[test]
fn dynamic_reads_refresh_a_parallel_branch_before_merge() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = DynamicModel {
        b_initial: Default::default(),
    };
    let task = core
        .execute(
            "dynamic whole",
            Intent::Edit,
            Options {
                plan: true,
                ..options()
            },
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(task.status, Status::AwaitingApproval, "{}", task.summary);
    assert_eq!(
        model.b_initial.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "stale branch must run again"
    );
    core.approve(&task.id, &AtomicBool::new(false), &|_| {})
        .unwrap();
    assert_eq!(
        fs::read_to_string(d.path().join("b.txt")).unwrap(),
        "fresh 9"
    );
}
#[test]
fn validation_evidence_survives_overlay_cleanup() {
    use loom_core::base::{ArtifactStore, FileArtifactStore};
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([response(vec![
        (
            "change",
            json!({"op":"create","target":"new.txt","content":"new"}),
        ),
        ("finish", json!({"summary":"done"})),
    ])]);
    let task = core
        .execute(
            "create",
            Intent::Edit,
            Options {
                checks: vec![if cfg!(windows) {
                    "echo persistent-evidence"
                } else {
                    "printf persistent-evidence"
                }
                .into()],
                ..options()
            },
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(task.status, Status::AwaitingApproval);
    let bytes = FileArtifactStore::open(d.path())
        .unwrap()
        .get(&task.validation[0].artifact, None)
        .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("persistent-evidence"));
}
#[test]
fn symbol_edit_cannot_modify_another_symbol_in_its_file() {
    let d = repo();
    fs::write(
        d.path().join("src/a.rs"),
        "pub fn alpha() -> u32 { 1 }\npub fn other() -> u32 { 2 }\n",
    )
    .unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("open", json!({"target":"src/a.rs::alpha"}))]),
        response(vec![
            (
                "change",
                json!({"op":"edit","target":"src/a.rs::alpha","old":"2 }","new":"8 }"}),
            ),
            ("finish", json!({"summary":"done"})),
        ]),
    ]);
    let task = core
        .execute(
            "change alpha",
            Intent::Edit,
            options(),
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(task.status, Status::Unresolved);
    assert!(task.patch.files.is_empty());
}
#[test]
fn replace_rejects_symbols_sharing_a_line() {
    let d = repo();
    fs::write(
        d.path().join("src/a.rs"),
        "pub fn alpha() {} pub fn other() {}\n",
    )
    .unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("open", json!({"target":"src/a.rs::alpha"}))]),
        response(vec![
            (
                "change",
                json!({"op":"replace","target":"src/a.rs::alpha","content":"pub fn alpha() {}\n"}),
            ),
            ("finish", json!({"summary":"done"})),
        ]),
    ]);
    let task = core
        .execute(
            "change alpha",
            Intent::Edit,
            options(),
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(task.status, Status::Unresolved);
    assert!(task.patch.files.is_empty());
}

#[test]
fn cargo_checks_keep_generated_locks_as_evidence_not_source() {
    let d = repo();
    fs::write(d.path().join("Cargo.toml"),"[package]\nname='fixture'\nversion='0.1.0'\nedition='2021'\n[lib]\npath='src/a.rs'\n[workspace]\n").unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("open", json!({"target":"src/a.rs"}))]),
        response(vec![
            (
                "change",
                json!({"op":"edit","target":"src/a.rs","old":"1 }","new":"9 }"}),
            ),
            ("finish", json!({"summary":"updated"})),
        ]),
    ]);
    let t = core
        .execute(
            "edit",
            Intent::Edit,
            Options {
                checks: vec!["cargo test --offline --quiet".into()],
                ..options()
            },
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(t.status, Status::AwaitingApproval, "{}", t.summary);
    core.approve(&t.id, &AtomicBool::new(false), &|_| {})
        .unwrap();
    assert!(!d.path().join("Cargo.lock").exists());
}
#[test]
fn questions_are_answered_through_control_and_share_call_budget() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let control = loom_core::agent::InputControl::default();
    let model = ScriptedModel::new([
        response(vec![("ask", json!({"question":"어떤 이름을 쓸까요?"}))]),
        response(vec![("finish", json!({"summary":"답변 받음"}))]),
    ]);
    let task = core
        .execute(
            "question",
            Intent::Inspect,
            Options {
                control: control.clone(),
                steps: 2,
                ..options()
            },
            &model,
            cancel(),
            &|e| {
                if let Some(id) = e.detail["input_request"]["id"].as_str() {
                    control.answer(id.into(), "선택한 이름".into());
                }
            },
        )
        .unwrap();
    assert_eq!(task.status, Status::Inspected);
    assert_eq!(task.calls, 2);
    assert!(
        model.requests()[1]
            .messages
            .iter()
            .flat_map(|m| &m.parts)
            .any(|p| matches!(p,Part::ToolResult{output,..} if output.contains("선택한 이름")))
    );
    let model = ScriptedModel::new([
        response(vec![("open", json!({"target":"src/a.rs"}))]),
        response(vec![("finish", json!({"summary":"should not run"}))]),
    ]);
    let t = core
        .execute(
            "inspect",
            Intent::Inspect,
            Options {
                steps: 1,
                ..options()
            },
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(t.status, Status::Unresolved);
    assert_eq!(t.calls, 1);
    assert_eq!(model.requests().len(), 1);
}
#[test]
fn cancellation_is_terminal_and_preserves_the_workspace() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let token = cancel();
    let initial = Revision::capture(d.path()).unwrap();
    let model = ScriptedModel::new([response(vec![
        (
            "change",
            json!({"op":"create","target":"new.txt","content":"isolated"}),
        ),
        ("finish", json!({"summary":"done"})),
    ])]);
    let t = core
        .execute(
            "edit",
            Intent::Edit,
            options(),
            &model,
            token.clone(),
            &|e| {
                if e.stage == loom_core::agent::events::Stage::Change && e.detail["completed"] == true {
                    token.store(true, std::sync::atomic::Ordering::Release);
                }
            },
        )
        .unwrap();
    assert_eq!(t.status, Status::Cancelled);
    assert_eq!(Revision::capture(d.path()).unwrap(), initial);
}

#[test]
fn restarted_session_injects_original_request_failure_and_read_evidence() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("open", json!({"target":"src/a.rs"}))]),
        response(vec![(
            "change",
            json!({"op":"edit","target":"src/a.rs","old":"MISSING_TARGET","new":"unused"}),
        )]),
    ]);
    let first = core
        .execute(
            "original request ALPHA_CONTRACT",
            Intent::Edit,
            Options {
                steps: 2,
                ..options()
            },
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(first.status, Status::Unresolved);
    let mut restarted = Core::open(d.path()).unwrap();
    let history = restarted.context(&[first.id], 40000).unwrap();
    assert_eq!(history[0].role, models::Role::User);
    assert_eq!(history[1].role, models::Role::Assistant);
    let model = ScriptedModel::new([response(vec![(
        "finish",
        json!({"summary":"history received"}),
    )])]);
    let t = restarted
        .execute(
            "continue",
            Intent::Inspect,
            Options {
                history,
                ..options()
            },
            &model,
            cancel(),
            &|_| {},
        )
        .unwrap();
    assert_eq!(t.status, Status::Inspected);
    let text = model.requests()[0]
        .messages
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("ALPHA_CONTRACT"));
    assert!(text.contains("src/a.rs"));
    assert!(text.contains("old text was not found"));
}
#[test]
fn compaction_preserves_current_goal_failed_tool_and_read_targets() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("open", json!({"target":"src/a.rs"}))]),
        response(vec![("open", json!({"target":"MISSING_FILE"}))]),
        response(vec![("open", json!({"target":"src/b.rs"}))]),
        response(vec![("project", json!({"set":"S1"}))]),
        response(vec![("finish", json!({"summary":"done"}))]),
    ]);
    core.execute(
        "KEEP_CURRENT_GOAL",
        Intent::Inspect,
        Options {
            context_bytes: 1,
            ..options()
        },
        &model,
        cancel(),
        &|_| {},
    )
    .unwrap();
    let last = model
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .map(|m| serde_json::to_string(&m.parts).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(last.contains("KEEP_CURRENT_GOAL"));
    assert!(last.contains("src/a.rs"));
    assert!(last.contains("MISSING_FILE"));
}

struct PublishModel {
    merged: Arc<AtomicBool>,
}
impl ModelPort for PublishModel {
    fn complete(&self, need: Need, r: &Request) -> Result<Served, models::ProviderError> {
        let goal = r.messages[0].text();
        let have = |name: &str| {
            r.messages
                .iter()
                .flat_map(|m| &m.parts)
                .any(|p| matches!(p,Part::ToolCall{name:n,..} if n==name))
        };
        let calls = if goal.contains("publish whole") {
            if have("agent") {
                vec![("finish", json!({"summary":"published"}))]
            } else {
                vec![(
                    "agent",
                    json!({"units":[{"id":"a","goal":"fast unit","write_files":["a.txt"],"depends_on":[]},{"id":"b","goal":"slow unit","write_files":["b.txt"],"depends_on":[]}]}),
                )]
            }
        } else if goal.contains("fast unit") {
            vec![
                (
                    "change",
                    json!({"op":"create","target":"a.txt","content":"fast"}),
                ),
                ("finish", json!({"summary":"fast done"})),
            ]
        } else {
            let start = std::time::Instant::now();
            while !self.merged.load(std::sync::atomic::Ordering::Acquire)
                && start.elapsed() < Duration::from_secs(2)
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let published = self.merged.load(std::sync::atomic::Ordering::Acquire);
            vec![
                (
                    "change",
                    json!({"op":"create","target":"b.txt","content":"slow"}),
                ),
                (
                    "finish",
                    json!({"summary":"slow done","unresolved":!published}),
                ),
            ]
        };
        ScriptedModel::new([response(calls)]).complete(need, r)
    }
}
#[test]
fn finished_unit_publishes_world_before_other_parallel_workers_finish() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let merged = Arc::new(AtomicBool::new(false));
    let model = PublishModel {
        merged: merged.clone(),
    };
    let t = core
        .execute(
            "publish whole",
            Intent::Edit,
            Options {
                plan: true,
                ..options()
            },
            &model,
            cancel(),
            &|e| {
                if e.stage == loom_core::agent::events::Stage::Merge && e.worker == "a" {
                    merged.store(true, std::sync::atomic::Ordering::Release);
                }
            },
        )
        .unwrap();
    assert_eq!(t.status, Status::AwaitingApproval, "{}", t.summary);
}

#[test]
fn nested_file_hierarchy_and_symbol_text_scope_stay_exact() {
    let d = repo();
    fs::create_dir_all(d.path().join("crates/nested/src")).unwrap();
    fs::write(
        d.path().join("crates/nested/Cargo.toml"),
        "[package]\nname='nested'\nversion='0.1.0'\n",
    )
    .unwrap();
    fs::write(
        d.path().join("src/a.rs"),
        "pub fn alpha() { let unique_alpha = 1; }\npub fn other() { let unique_other = 2; }\n",
    )
    .unwrap();
    let view = WorldHub::default().view(d.path()).unwrap();
    let mut sets = Sets::default();
    let nested = sets
        .find(
            &view,
            Find {
                query: "crates/nested".into(),
                kind: FindKind::Path,
                scope: None,
            },
        )
        .unwrap();
    let pkg = sets
        .expand(&view, nested["set"].as_str().unwrap(), Relation::Package)
        .unwrap();
    assert_eq!(pkg["count"], 1);
    assert!(
        sets.get(&view, pkg["set"].as_str().unwrap())
            .unwrap()
            .nodes
            .contains(&loom_core::agent::world::Node::Directory(
                "crates/nested".into()
            ))
    );
    let text = sets
        .find(
            &view,
            Find {
                query: "unique_alpha".into(),
                kind: FindKind::Text,
                scope: Some("src/a.rs::alpha".into()),
            },
        )
        .unwrap();
    assert_eq!(text["count"], 1);
    let outside = sets
        .find(
            &view,
            Find {
                query: "unique_other".into(),
                kind: FindKind::Text,
                scope: Some("src/a.rs::alpha".into()),
            },
        )
        .unwrap();
    assert_eq!(outside["count"], 0);
}

#[test]
fn natural_reply_uses_normal_tools_and_never_runs_implicit_repository_operations() {
    let d = repo();
    let model = ScriptedModel::new(vec![Response {
        parts: vec![Part::Text("안녕하세요!".into())], stop: Stop::EndTurn,
        ..response(vec![])
    }]);
    let events = Mutex::new(Vec::new());
    let task = Core::open(d.path()).unwrap().execute("안녕", Intent::Auto, options(), &model,
        Arc::new(AtomicBool::new(false)), &|e| events.lock().unwrap().push(e)).unwrap();
    assert_eq!(task.status, Status::Answered);
    let requests = model.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.iter().any(|tool| tool.name == "run"));
    assert_eq!(requests[0].messages.last().unwrap().text(), "안녕");
    assert!(events.lock().unwrap().iter().any(|event|
        event.target == "preparation" && event.detail["phase"] == "index" && event.detail["started"] == true));
    // Preparation progress is not a model-requested tool invocation.
    assert!(!events.lock().unwrap().iter().any(|event| event.detail["tool"].is_string()),
        "Core must not open files or execute tools without a model call");
    assert!(task.validation.is_empty() && task.patch.files.is_empty());
}

struct RetryFixture {
    calls: std::sync::atomic::AtomicUsize,
    mode: u8,
}
impl ModelPort for RetryFixture {
    fn complete(&self, _need: Need, request: &Request) -> Result<Served, models::ProviderError> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if n == 0 || self.mode == 2 {
            let mut error = models::ProviderError::new(if self.mode == 1 {
                models::ErrorKind::Auth
            } else { models::ErrorKind::Transient }, "fixture response timed out");
            error.retry_after = Some(Duration::from_millis(20));
            return Err(error);
        }
        models::ScriptedModel::new([Response {
            parts: vec![Part::Text("연결 복구 후 응답".into())], stop: Stop::EndTurn,
            ..response(vec![])
        }]).complete(Need::default(), request)
    }
}
#[test]
fn transient_model_failure_retries_same_request_within_budget_but_auth_does_not() {
    for (mode, expected_calls, status) in [(0, 2, Status::Answered), (1, 1, Status::Unresolved), (2, 3, Status::Unresolved)] {
        let d = repo();
        let model = RetryFixture { calls: Default::default(), mode };
        let events = Mutex::new(Vec::new());
        let task = Core::open(d.path()).unwrap().execute("요청에 답해줘", Intent::Auto, options(),
            &model, Arc::new(AtomicBool::new(false)), &|event| events.lock().unwrap().push(event)).unwrap();
        assert_eq!(task.status, status);
        assert_eq!(task.calls, expected_calls);
        let events = events.lock().unwrap();
        assert_eq!(events.iter().filter(|event| event.target == "model_retry").count(), expected_calls - 1);
        let requests: Vec<_> = events.iter().filter(|event| event.target == "model").collect();
        assert!(requests.windows(2).all(|pair| pair[0].detail["request"] == pair[1].detail["request"]));
        assert!(!events.iter().any(|event| event.detail["tool"].is_string()));
        assert!(task.patch.files.is_empty());
    }
}
#[test]
fn repeated_empty_replies_end_the_task_instead_of_asking_forever() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    let empty = || Response { stop: Stop::EndTurn, ..response(vec![]) };
    let model = ScriptedModel::new((0..12).map(|_| empty()));
    let t = core
        .execute("inspect alpha", Intent::Inspect, Options { steps: 12, ..options() }, &model, cancel(), &|_| {})
        .unwrap();
    assert!(model.requests().len() <= 3, "asked {} times", model.requests().len());
    assert!(t.summary.contains("empty"), "{}", t.summary);
}
#[test]
fn commands_naming_the_real_workspace_act_on_the_isolated_copy() {
    let d = repo();
    let root = dunce::canonicalize(d.path()).unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let command = if cfg!(windows) {
        format!(
            "mkdir \"{0}\\hello_world\" && echo print(\"hi\")> \"{0}\\hello_world\\hello.py\" && type \"{0}\\hello_world\\hello.py\"",
            root.display()
        )
    } else {
        format!(
            "mkdir -p {0}/hello_world && printf 'print(\"hi\")\\n' > {0}/hello_world/hello.py && cat {0}/hello_world/hello.py",
            root.display()
        )
    };
    let model = ScriptedModel::new([
        response(vec![("run", json!({"kind":"command","command":command}))]),
        response(vec![("finish", json!({"summary":"created"}))]),
    ]);
    let options = Options { checks: vec![if cfg!(windows) { "exit /b 0" } else { "true" }.into()], ..options() };
    let task = core.execute("create hello_world/hello.py", Intent::Edit, options, &model, cancel(), &|_| {}).unwrap();
    let result = model.requests()[1].messages.iter().flat_map(|m| &m.parts).find_map(|part| match part {
        Part::ToolResult { output, .. } => Some(output.clone()),
        _ => None,
    }).unwrap();
    assert!(result.contains(r#""status":"Passed""#) && result.contains(r#"print(\"hi\")"#), "the command must succeed in the copy: {result}");
    assert!(!result.contains(".loom/overlays") && !result.contains(r".loom\overlays"), "output names the workspace, not the copy: {result}");
    assert!(task.patch.files.iter().any(|file| file.path.ends_with("hello_world/hello.py")), "{:?}", task.patch.files);
    assert!(!root.join("hello_world").exists(), "the real workspace changes only after approval");
}
#[test]
fn plan_mode_accepts_a_written_plan_and_direct_work_without_forcing_agents() {
    let d = repo();
    let mut core = Core::open(d.path()).unwrap();
    // A plan written as prose ends the turn; it is not asked for again.
    let plan = Response {
        parts: vec![Part::Text("1. src/a.rs의 alpha를 7로 바꾼다\n2. 테스트를 돌린다".into())],
        stop: Stop::EndTurn,
        ..response(vec![])
    };
    let model = ScriptedModel::new([plan.clone(), plan]);
    let t = core
        .execute("plan the alpha change", Intent::Inspect, Options { plan: true, steps: 6, ..options() }, &model, cancel(), &|_| {})
        .unwrap();
    assert_eq!(model.requests().len(), 1, "a written plan must not be re-requested");
    assert!(t.summary.contains("alpha를 7로"), "{}", t.summary);
    // Work that needs no agents is done directly.
    let model = ScriptedModel::new([
        response(vec![("open", json!({"target":"src/a.rs"}))]),
        response(vec![("change", json!({"op":"edit","target":"src/a.rs","old":"1","new":"7"}))]),
        response(vec![("finish", json!({"summary":"alpha is 7"}))]),
    ]);
    let t = core
        .execute("change alpha to 7", Intent::Edit, Options { plan: true, ..options() }, &model, cancel(), &|_| {})
        .unwrap();
    assert_eq!(t.status, Status::AwaitingApproval, "{}", t.summary);
    assert!(!t.delegated);
}

#[test]
fn explicit_check_commands_run_without_replacing_core_validation() {
    let configured_cmd = if cfg!(windows) { "echo configured-check" } else { "printf configured-check" };
    let manual_cmd = if cfg!(windows) { "echo manual-check" } else { "printf manual-check" };
    for configured in [vec![], vec![configured_cmd.to_owned()]] {
        let d = repo();
        let model = ScriptedModel::new([
            response(vec![("run", json!({"kind":"check","command":manual_cmd}))]),
            response(vec![("finish", json!({"summary":"checked"}))]),
        ]);
        let mut core = Core::open(d.path()).unwrap();
        let task = core.execute("run a test", Intent::Auto,
            Options { checks: configured.clone(), ..options() }, &model, cancel(), &|_| {}).unwrap();
        let requests = model.requests();
        let result = requests[1].messages.iter().flat_map(|message| &message.parts).find_map(|part| match part {
            Part::ToolResult {output, ..} => Some(output), _ => None,
        }).unwrap();
        assert!(result.contains("manual-check") && result.contains("Passed"), "{result}");
        if !configured.is_empty() {
            assert_eq!(task.checks, configured, "worker checks must not redefine Core validation");
        }
        assert!(!task.checks.iter().any(|command| command.contains("manual-check")),
            "manual checks must not replace configured or discovered Core validation");
        assert!(task.patch.files.is_empty());
    }
}

#[test]
fn check_without_a_command_runs_configured_validation() {
    let d = repo();
    let model = ScriptedModel::new([
        response(vec![("run", json!({"kind":"check"}))]),
        response(vec![("finish", json!({"summary":"checked"}))]),
    ]);
    let mut core = Core::open(d.path()).unwrap();
    let configured_cmd = if cfg!(windows) { "echo configured-check" } else { "printf configured-check" };
    core.execute("run configured checks", Intent::Auto,
        Options { checks: vec![configured_cmd.into()], ..options() },
        &model, cancel(), &|_| {}).unwrap();
    let requests = model.requests();
    let result = requests[1].messages.iter().flat_map(|message| &message.parts).find_map(|part| match part {
        Part::ToolResult { output, .. } => Some(output), _ => None,
    }).unwrap();
    assert!(result.contains("configured-check") && result.contains("Passed"), "{result}");
}

#[test]
fn invalid_expand_handle_explains_how_to_find_a_set_and_recover() {
    let d = repo();
    let hub = WorldHub::default();
    let view = hub.view(d.path()).unwrap();
    let mut sets = Sets::default();
    let error = sets.expand(&view, "src", Relation::Files).unwrap_err().to_string();
    assert!(error.contains("find") && error.contains("path") && error.contains("S1"), "{error}");
    let found = sets.find(&view, Find { query: "src".into(), kind: FindKind::Path, scope: None }).unwrap();
    let files = sets.expand(&view, found["set"].as_str().unwrap(), Relation::Files).unwrap();
    assert_eq!(files["count"], 2);
}

#[test]
fn a_folder_that_is_not_a_repository_is_initialized_and_answered() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("notes.txt"), "hello\n").unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([response(vec![("finish", json!({"summary":"hi"}))])]);
    let events = std::sync::Mutex::new(Vec::new());
    let task = core
        .execute("hi", Intent::Auto, options(), &model, cancel(), &|event| {
            events.lock().unwrap().push(event.detail.clone())
        })
        .unwrap();
    assert!(d.path().join(".git").exists());
    assert_eq!(std::fs::read_to_string(d.path().join("notes.txt")).unwrap(), "hello\n");
    assert!(events.lock().unwrap().iter().any(|e| e["initialized"] == true));
    assert!(!matches!(task.status, Status::Unresolved), "{}", task.summary);
}

#[test]
fn standalone_python_gets_independent_syntax_validation() {
    for (target, content, expected) in [
        ("example.py", "print('ok')\n", Status::AwaitingApproval),
        ("example.py", "def broken(\n", Status::Unresolved),
        ("odd ' $() % name.py", "print('ok')\n", Status::AwaitingApproval),
    ] {
        let d = repo();
        fs::remove_file(d.path().join("Cargo.toml")).unwrap();
        let mut core = Core::open(d.path()).unwrap();
        let model = ScriptedModel::new([response(vec![
            ("change", json!({"op":"create","target":target,"content":content})),
            ("finish", json!({"summary":"created example"})),
        ])]);
        let task = core.execute("create a Python example", Intent::Edit,
            Options { checks:vec![], ..options() }, &model, cancel(), &|_| {}).unwrap();
        assert_eq!(task.status, expected, "{}", task.summary);
        assert_eq!(task.checks.len(), 1);
        assert_eq!(task.validation.len(), 1);
        assert!(!d.path().join(target).exists(), "validation must keep the proposal isolated");
    }
}

#[test]
fn configured_pre_tool_hook_blocks_the_real_tool_and_reports_the_reason() {
    let d = repo();
    fs::create_dir_all(d.path().join(".loom")).unwrap();
    fs::write(d.path().join(".loom/config.toml"), r#"
[hooks]
enabled = true
[[hooks.PreToolUse]]
matcher = "^find$"
[[hooks.PreToolUse.hooks]]
name = "deny-find"
type = "command"
command = "echo blocked-by-hook >&2 && exit 2"
timeout = 1
"#).unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("find", json!({"query":"alpha"}))]),
        response(vec![("finish", json!({"summary":"hook denied the operation", "unresolved":true}))]),
    ]);
    let events = Mutex::new(Vec::new());
    let task = core.execute("inspect alpha", Intent::Inspect, options(), &model, cancel(), &|event| events.lock().unwrap().push(event)).unwrap();
    assert_eq!(task.status, Status::Unresolved);
    let system = model.requests()[0].system.clone().unwrap();
    assert!(system.contains("Loom command hooks") && system.contains("PreToolUse / deny-find"));
    assert!(system.contains("next task") && system.contains("without the user's request"));
    assert!(model.requests()[1].messages.iter().flat_map(|m| &m.parts).any(|part|
        matches!(part, Part::ToolResult {output, is_error: true, ..} if output.contains("blocked-by-hook"))));
    assert!(events.into_inner().unwrap().iter().any(|event| event.detail["hook_event"] == "PreToolUse" && event.failed));
}

#[cfg(unix)]
#[test]
fn post_tool_hook_edits_are_isolated_indexed_and_failures_do_not_replay_the_tool() {
    let d = repo();
    fs::create_dir_all(d.path().join(".loom")).unwrap();
    fs::write(d.path().join(".loom/config.toml"), r#"
[hooks]
enabled = true
[[hooks.PostToolUse]]
matcher = "^Bash$"
[[hooks.PostToolUse.hooks]]
name = "format"
type = "command"
command = "printf 'pub fn formatted() -> u32 { 3 }\n' > src/a.rs; echo format-warning >&2; exit 2"
"#).unwrap();
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([
        response(vec![("run", json!({"command":"echo tool-once"}))]),
        response(vec![("find", json!({"query":"formatted", "kind":"symbol"}))]),
        response(vec![("finish", json!({"summary":"formatted in the isolated proposal"}))]),
    ]);
    let events = Mutex::new(Vec::new());
    let task = core.execute("format alpha", Intent::Edit, options(), &model, cancel(), &|event| events.lock().unwrap().push(event)).unwrap();
    assert_eq!(task.status, Status::AwaitingApproval);
    assert!(fs::read_to_string(d.path().join("src/a.rs")).unwrap().contains("alpha"));
    assert!(task.patch.files.iter().any(|file| file.path == PathBuf::from("src/a.rs")));
    assert!(model.requests()[1].messages.iter().flat_map(|m| &m.parts).any(|part|
        matches!(part, Part::ToolResult {output, is_error: false, ..} if output.starts_with("PostToolUse feedback") && output.contains("do not replay") && output.contains("format-warning"))));
    assert!(model.requests()[2].messages.iter().flat_map(|m| &m.parts).any(|part|
        matches!(part, Part::ToolResult {output, ..} if output.contains("formatted"))));
    assert_eq!(events.into_inner().unwrap().iter().filter(|event| event.detail["tool"] == "run" && event.detail["started"] == true).count(), 1);
}

#[cfg(unix)]
#[test]
fn prompt_denial_skips_model_and_session_end_runs_only_on_shutdown() {
    use loom_core::base::{ArtifactStore, FileArtifactStore};
    let d = repo();
    compatible_config(d.path(), json!({
        "UserPromptSubmit":[{"hooks":[{"type":"command","command":"echo startup-denied >&2; exit 2"}]}],
        "SessionEnd":[{"hooks":[{"type":"command","command":"cat"}]}]
    }));
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([]);
    let events = Mutex::new(Vec::new());
    let task = core.execute("inspect alpha", Intent::Inspect, options(), &model, cancel(), &|event| events.lock().unwrap().push(event)).unwrap();
    assert_eq!(task.status, Status::Unresolved);
    assert!(task.summary.contains("startup-denied"));
    assert!(model.requests().is_empty());
    assert!(!events.into_inner().unwrap().iter().any(|event| event.detail["hook_event"] == "SessionEnd"));
    loom_core::hooks::Hooks::end_session(d.path(), "stable-session", options().policy).unwrap();
    let text = fs::read_to_string(d.path().join(".loom/hooks-session-events.jsonl")).unwrap();
    let events:Vec<Value> = text.lines().map(|line|serde_json::from_str(line).unwrap()).collect();
    let ends:Vec<_> = events.iter().filter(|event|event["detail"]["completed"] == true).collect();
    assert_eq!(ends.len(),1);
    let reference = serde_json::from_value(ends[0]["detail"]["result"]["artifact"].clone()).unwrap();
    let bytes = FileArtifactStore::open(d.path()).unwrap().get(&reference, None).unwrap();
    let output = String::from_utf8(bytes).unwrap();
    assert!(output.contains("stable-session") && output.contains("SessionEnd") && output.contains("other"));
}

#[cfg(unix)]
#[test]
fn interrupt_runs_on_cancellation_but_cannot_modify_the_real_workspace() {
    let d = repo();
    compatible_config(d.path(),json!({
        "UserPromptSubmit":[{"hooks":[{"type":"command","command":"echo '{}'"}]}],
        "Interrupt":[{"hooks":[{"type":"command","command":"echo CHANGED > src/a.rs"}]}]
    }));
    let mut core = Core::open(d.path()).unwrap();
    let model = ScriptedModel::new([]);
    let flag = cancel();
    let events = Mutex::new(Vec::new());
    let task = core.execute("inspect alpha", Intent::Inspect, options(), &model, flag.clone(), &|event| {
        if event.detail["hook_event"] == "UserPromptSubmit" && event.detail["completed"] == true {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
        events.lock().unwrap().push(event);
    }).unwrap();
    assert_eq!(task.status, Status::Cancelled);
    assert!(fs::read_to_string(d.path().join("src/a.rs")).unwrap().contains("alpha"));
    assert!(events.into_inner().unwrap().iter().any(|event| event.detail["hook_event"] == "Interrupt" && event.failed));
    assert!(model.requests().is_empty());
}

#[cfg(unix)]
fn compatible_config(root: &std::path::Path, hooks: Value) {
    fs::create_dir_all(root.join(".loom")).unwrap();
    fs::write(root.join(".loom/hooks.json"), json!({"hooks":hooks}).to_string()).unwrap();
}
#[cfg(unix)]
fn hook_json(output: Value) -> String { format!("printf '%s' '{}'",output.to_string().replace('\'', "'\\''")) }
#[cfg(unix)]
fn answer(text: &str) -> Response {
    Response {parts:vec![Part::Text(text.into())],stop:Stop::EndTurn,..response(vec![])}
}
#[cfg(unix)]
#[test]
fn compatible_pre_hook_rewrites_real_command_and_adds_model_context() {
    let d=repo();
    compatible_config(d.path(),json!({"PreToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","command":hook_json(json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"printf safe-output"},"additionalContext":"hook-policy-context"}}))}]}]}));
    let model=ScriptedModel::new([response(vec![("run",json!({"command":"touch must-not-run"}))]),answer("done")]);
    let mut core=Core::open(d.path()).unwrap();
    let events=Mutex::new(Vec::new());
    let task=core.execute("run command",Intent::Auto,options(),&model,cancel(),&|event|events.lock().unwrap().push(event)).unwrap();
    assert_eq!(task.status,Status::Answered);
    assert!(model.requests()[1].system.as_ref().unwrap().contains("hook-policy-context"));
    assert!(model.requests()[1].messages.iter().flat_map(|m|&m.parts).any(|p|matches!(p,Part::ToolResult {output,..} if output.contains("safe-output"))));
    assert!(events.into_inner().unwrap().iter().any(|e|e.detail["tool"]=="run" && e.detail["args"]["command"]=="printf safe-output"));
    assert!(!d.path().join("must-not-run").exists());
}
#[cfg(unix)]
#[test]
fn compatible_prompt_denial_never_calls_model_and_post_feedback_never_replays_tools() {
    let d=repo();
    compatible_config(d.path(),json!({"UserPromptSubmit":[{"hooks":[{"type":"command","command":hook_json(json!({"decision":"block","reason":"prompt denied"}))}]}]}));
    let model=ScriptedModel::new([]);
    let mut core=Core::open(d.path()).unwrap();
    let task=core.execute("blocked",Intent::Auto,options(),&model,cancel(),&|_|{}).unwrap();
    assert_eq!(task.status,Status::Unresolved);
    assert!(task.summary.contains("prompt denied") && model.requests().is_empty());
    compatible_config(d.path(),json!({"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":hook_json(json!({"decision":"block","reason":"checked feedback"}))}]}]}));
    let model=ScriptedModel::new([response(vec![("run",json!({"command":"printf original-result"}))]),answer("reviewed")]);
    let events=Mutex::new(Vec::new());
    let task=core.execute("inspect",Intent::Auto,options(),&model,cancel(),&|e|events.lock().unwrap().push(e)).unwrap();
    assert_eq!(task.status,Status::Answered);
    assert!(model.requests()[1].messages.iter().flat_map(|m|&m.parts).any(|p|matches!(p,Part::ToolResult {output,is_error:false,..} if output.contains("checked feedback") && !output.contains("original-result"))));
    assert_eq!(events.into_inner().unwrap().iter().filter(|e|e.detail["tool"]=="run" && e.detail["started"]==true).count(),1);
}
#[cfg(unix)]
#[test]
fn compatible_stop_requests_bounded_continuation_and_session_start_context_is_real() {
    let d=repo();
    let stop=r#"input=$(cat); case "$input" in *'"stop_hook_active":false'*) printf '%s' '{"decision":"block","reason":"perform another check"}';; *) printf '{}';; esac"#;
    compatible_config(d.path(),json!({"Stop":[{"hooks":[{"type":"command","command":stop}]}],"SessionStart":[{"matcher":"^resume$","hooks":[{"type":"command","command":"printf session-knowledge"}]}]}));
    let mut options=options(); options.session_id=Some("stable-session".into()); options.session_start=Some("resume".into());
    let model=ScriptedModel::new([answer("first answer"),answer("checked answer")]);
    let events=Mutex::new(Vec::new());
    let task=Core::open(d.path()).unwrap().execute("inspect",Intent::Inspect,options,&model,cancel(),&|e|events.lock().unwrap().push(e)).unwrap();
    assert_eq!(task.status,Status::Inspected);
    assert_eq!(task.summary,"checked answer");
    assert_eq!(model.requests().len(),2);
    assert!(model.requests()[0].system.as_ref().unwrap().contains("session-knowledge"));
    assert!(model.requests()[1].messages.iter().any(|m|m.text().contains("perform another check")));
    assert_eq!(events.into_inner().unwrap().iter().filter(|e|e.detail["hook_event"]=="SessionStart" && e.detail["started"]==true).count(),1);
}

#[cfg(unix)]
#[test]
fn stop_hook_cannot_continue_forever() {
    let d=repo();
    compatible_config(d.path(),json!({"Stop":[{"hooks":[{"type":"command","command":hook_json(json!({"decision":"block","reason":"keep checking"}))}]}]}));
    let model=ScriptedModel::new([answer("one"),answer("two"),answer("three"),answer("four")]);
    let task=Core::open(d.path()).unwrap().execute("inspect",Intent::Inspect,options(),&model,cancel(),&|_|{}).unwrap();
    assert_eq!(model.requests().len(),4);
    assert_eq!(task.status,Status::Unresolved);
    assert!(task.summary.contains("continuation limit"));
}
