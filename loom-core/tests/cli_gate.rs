mod isolation;
use loom_core::models::{Part, Response, Stop, Usage};
use serde_json::{Value, json};
use std::{
    fs,
    process::{Command, Output},
    time::Duration,
};

fn invoke(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loom"))
        .arg("--cwd")
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn cli_reports_tool_activity_and_exhaustion_without_polluting_json_output() {
    let repo = tempfile::tempdir().unwrap();
    Command::new("git")
        .current_dir(repo.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    fs::write(repo.path().join("file.txt"), "old\n").unwrap();
    let script = tempfile::NamedTempFile::new().unwrap();
    let replies: Vec<_> = [
        ("read", json!({"path":"file.txt"})),
        ("edit", json!({"path":"file.txt","old":"old","new":"new"})),
    ]
    .into_iter()
    .map(|(name, arguments)| Response {
        parts: vec![Part::ToolCall {
            id: name.into(),
            name: name.into(),
            arguments,
        }],
        usage: Usage::default(),
        stop: Stop::ToolUse,
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    })
    .collect();
    fs::write(script.path(), serde_json::to_vec(&replies).unwrap()).unwrap();
    let args = [
        "run",
        "change file",
        "--intent",
        "edit",
        "--max-attempts",
        "1",
        "--max-steps",
        "2",
        "--script",
        script.path().to_str().unwrap(),
    ];
    let human = invoke(repo.path(), &args);
    // CLI exit 2 denotes a recorded task that did not reach Accepted.
    assert_eq!(human.status.code(), Some(2));
    let progress = String::from_utf8_lossy(&human.stderr);
    let start = progress
        .find("실행 중: read · file.txt")
        .expect("missing tool start");
    let end = progress
        .find("read · file.txt · 결과:")
        .expect("missing tool result");
    assert!(start < end, "{progress}");
    assert!(progress.contains("edit · file.txt"));
    assert!(
        !progress.contains("Result:"),
        "internal result text leaked into human CLI progress: {progress}"
    );
    for expected in [
        "Core 실행 보고",
        "원본 반영: 없음",
        "남은 작업",
        "격리 사본 수정 제안: file.txt",
    ] {
        assert!(
            String::from_utf8_lossy(&human.stdout).contains(expected),
            "{human:?}"
        );
    }
    let mut json_args = args.to_vec();
    json_args.push("--json");
    let output = invoke(repo.path(), &json_args);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "Unresolved");
    assert!(
        result["summary"]
            .as_str()
            .unwrap()
            .contains("Core 실행 보고")
    );
    assert!(output.stderr.is_empty(), "JSON mode emitted human progress");
    assert_eq!(
        fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn cli_intent_is_enforced_for_edit_and_inspect() {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .current_dir(repo.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    fs::write(repo.path().join("file.txt"), "old\n").unwrap();
    let script = tempfile::NamedTempFile::new().unwrap();
    let replies = vec![Response {
        parts: vec![Part::Text("here is an explanation".into())],
        usage: Usage::default(),
        stop: Stop::EndTurn,
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    }];
    fs::write(script.path(), serde_json::to_vec(&replies).unwrap()).unwrap();
    let path = script.path().to_str().unwrap();
    let edit = invoke(
        repo.path(),
        &[
            "run",
            "change file",
            "--intent",
            "edit",
            "--max-attempts",
            "1",
            "--script",
            path,
            "--json",
        ],
    );
    let result: Value = serde_json::from_slice(&edit.stdout).unwrap();
    assert_eq!(result["status"], "Unresolved");
    assert_eq!(
        fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "old\n"
    );
    let inspect = invoke(
        repo.path(),
        &[
            "run",
            "explain file",
            "--intent",
            "inspect",
            "--script",
            path,
            "--json",
        ],
    );
    let result: Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert_eq!(result["status"], "Accepted");
    let conflict = invoke(
        repo.path(),
        &["run", "change file", "--read-only", "--intent", "edit"],
    );
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("conflicts"));
}

#[test]
fn cli_proposes_without_writing_then_approves_and_replays_from_another_process() {
    let repo = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .current_dir(repo.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    fs::write(repo.path().join("file.txt"), "old\n").unwrap();
    let script = tempfile::NamedTempFile::new().unwrap();
    let replies: Vec<_> = [
        ("read", json!({"path":"file.txt"})),
        ("edit", json!({"path":"file.txt","old":"old","new":"new"})),
        ("finish", json!({"summary":"changed file"})),
    ]
    .into_iter()
    .map(|(name, arguments)| Response {
        parts: vec![Part::ToolCall {
            id: name.into(),
            name: name.into(),
            arguments,
        }],
        usage: Usage::default(),
        stop: Stop::ToolUse,
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    })
    .collect();
    fs::write(script.path(), serde_json::to_vec(&replies).unwrap()).unwrap();
    let output = invoke(
        repo.path(),
        &[
            "run",
            "change file",
            "--script",
            script.path().to_str().unwrap(),
            "--no-approve",
            "--check",
            "true",
            "--json",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let proposed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(proposed["status"], "AwaitingApproval");
    assert_eq!(proposed["finished"], false);
    assert_eq!(
        fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "old\n"
    );
    let id = proposed["id"].as_str().unwrap();
    let blocked = invoke(repo.path(), &["approve", id, "--read-only", "--json"]);
    assert!(
        !blocked.status.success(),
        "read-only CLI applied a pending edit"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "old\n"
    );
    let accepted = invoke(repo.path(), &["approve", id, "--json"]);
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let accepted: Value = serde_json::from_slice(&accepted.stdout).unwrap();
    assert_eq!(accepted["status"], "Accepted");
    assert_eq!(
        fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "new\n"
    );
    let status: Value = serde_json::from_slice(&invoke(repo.path(), &["status"]).stdout).unwrap();
    assert_eq!(status["tasks"][id]["status"], "Accepted");
    assert!(invoke(repo.path(), &["approve", id]).status.success());
    assert!(
        !invoke(repo.path(), &["run", "paid", "--max-cost", "paid"])
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn killed_cli_replays_validation_and_can_finish_without_repeating_model_calls() {
    use std::{io, process::Stdio, time::Instant};
    let repo = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .current_dir(repo.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    fs::write(repo.path().join("file.txt"), "old\n").unwrap();
    let fixtures = tempfile::tempdir().unwrap();
    let script = fixtures.path().join("script.json");
    let marker = fixtures.path().join("validation.pid");
    let replies: Vec<_> = [
        ("read", json!({"path":"file.txt"})),
        ("edit", json!({"path":"file.txt","old":"old","new":"new"})),
        ("finish", json!({"summary":"proposal before crash"})),
    ]
    .into_iter()
    .map(|(name, arguments)| Response {
        parts: vec![Part::ToolCall {
            id: name.into(),
            name: name.into(),
            arguments,
        }],
        usage: Usage::default(),
        stop: Stop::ToolUse,
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    })
    .collect();
    fs::write(&script, serde_json::to_vec(&replies).unwrap()).unwrap();
    let check = format!(
        "if [ ! -f '{0}' ]; then echo $$ > '{0}'; sleep 30; fi",
        marker.display()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_loom"))
        .arg("--cwd")
        .arg(repo.path())
        .args([
            "run",
            "change file",
            "--no-approve",
            "--sandbox",
            "full-access",
            "--check",
            &check,
            "--script",
        ])
        .arg(&script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !marker.exists() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    let pid: i32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    // Runner owns a separate process group; clean up only this fixture's group.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    let state: Value = serde_json::from_slice(&invoke(repo.path(), &["status"]).stdout).unwrap();
    let (id, task) = state["tasks"].as_object().unwrap().iter().next().unwrap();
    assert_eq!(task["status"], "Validating");
    assert_eq!(
        fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "old\n"
    );
    // Empty ScriptedModel would fail if resume incorrectly started the worker again.
    fs::write(&script, b"[]").unwrap();
    let resumed = invoke(
        repo.path(),
        &[
            "resume",
            id,
            "--script",
            script.to_str().unwrap(),
            "--no-approve",
            "--json",
        ],
    );
    assert_eq!(
        resumed.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let resumed: Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(resumed["status"], "AwaitingApproval");
    assert!(invoke(repo.path(), &["approve", id]).status.success());
    assert_eq!(
        fs::read_to_string(repo.path().join("file.txt")).unwrap(),
        "new\n"
    );
}

#[test]
fn help_and_version_exit_successfully_without_opening_a_repository() {
    let missing = tempfile::tempdir().unwrap().path().join("missing");
    for flag in ["--help", "--version"] {
        let output = invoke(&missing, &[flag]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty());
    }
}
