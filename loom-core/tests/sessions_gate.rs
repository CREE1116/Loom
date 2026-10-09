mod isolation;
use loom_core::{
    control::{Core, ExecutionOptions},
    models::{Part, Response, ScriptedModel, Stop, Usage},
    sessions::Sessions,
};
use serde_json::json;
use std::{fs, process::Command, sync::atomic::AtomicBool, time::Duration};
fn finish(summary: &str) -> Response {
    Response {
        parts: vec![Part::ToolCall {
            id: "finish".into(),
            name: "finish".into(),
            arguments: json!({"summary":summary}),
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
#[test]
fn sessions_pass_previous_answers_and_project_guidance_to_the_next_model_after_restart() {
    let d = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(d.path())
        .status()
        .unwrap();
    fs::write(
        d.path().join("AGENTS.md"),
        "always use the project convention",
    )
    .unwrap();
    let options = ExecutionOptions {
        readonly: true,
        session: Some("conversation".into()),
        ..Default::default()
    };
    Core::open(d.path())
        .unwrap()
        .execute(
            "remember cobalt",
            options.clone(),
            &ScriptedModel::new([finish("cobalt remembered")]),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    let model = ScriptedModel::new([finish("answered")]);
    Core::open(d.path())
        .unwrap()
        .execute(
            "what was the word?",
            options,
            &model,
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    let text = model.requests()[0]
        .messages
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("cobalt remembered"));
    assert!(text.contains("always use the project convention"));
    let sessions = Sessions::open(d.path()).unwrap();
    assert_eq!(sessions.get("conversation").unwrap().turns.len(), 2);
    let branch = sessions
        .fork(
            "conversation",
            &sessions.get("conversation").unwrap().turns[0].task.0,
        )
        .unwrap();
    assert_eq!(branch.turns.len(), 1);
    let compacted = sessions.compact("conversation", 0).unwrap();
    assert_eq!(compacted.turns.len(), 2);
    assert!(compacted.archive.is_some());
    assert!(
        sessions
            .context("conversation")
            .unwrap()
            .0
            .contains("cobalt")
    );
}

#[test]
fn accepted_turns_can_fork_and_rewind_code_without_overwriting_unrelated_user_changes() {
    fn call(name: &str, args: serde_json::Value) -> Response {
        let mut r = finish("");
        r.parts = vec![Part::ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: args,
        }];
        r
    }
    let d = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(d.path())
        .status()
        .unwrap();
    fs::write(d.path().join("file.txt"), "one\n").unwrap();
    let options = ExecutionOptions {
        session: Some("rewind".into()),
        checks: Some(vec!["true".into()]),
        ..Default::default()
    };
    let mut core = Core::open(d.path()).unwrap();
    let first = core
        .execute(
            "one to two",
            options.clone(),
            &ScriptedModel::new([
                call("read", json!({"path":"file.txt"})),
                call("edit", json!({"path":"file.txt","old":"one","new":"two"})),
                finish("two"),
            ]),
            &AtomicBool::new(false),
            |_| {},
        )
        .unwrap();
    core.execute(
        "two to three",
        options,
        &ScriptedModel::new([
            call("read", json!({"path":"file.txt"})),
            call("edit", json!({"path":"file.txt","old":"two","new":"three"})),
            finish("three"),
        ]),
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    let preview = core.rewind_preview("rewind", &first.id.0);
    assert!(preview.blocked.is_none());
    assert_eq!((preview.added, preview.removed), (1, 1));
    assert_eq!(preview.files, vec!["file.txt"]);
    let conversation = core.rewind_conversation("rewind", &first.id.0).unwrap();
    assert_eq!(conversation.turns.len(), 1);
    assert_eq!(fs::read_to_string(d.path().join("file.txt")).unwrap(), "three\n");
    fs::write(d.path().join("file.txt"), "user modification\n").unwrap();
    assert!(core.rewind_preview("rewind", &first.id.0).blocked.is_some());
    assert!(core.rewind("rewind", &first.id.0).is_err());
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "user modification\n"
    );
    fs::write(d.path().join("file.txt"), "three\n").unwrap();
    let fork = core.rewind("rewind", &first.id.0).unwrap();
    assert_eq!(fork.turns.len(), 1);
    assert_eq!(
        fs::read_to_string(d.path().join("file.txt")).unwrap(),
        "two\n"
    );
}
