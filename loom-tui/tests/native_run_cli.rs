use serde_json::json;
use std::{fs, process::Command};

#[test]
fn native_run_respects_explicit_read_only_sandbox() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("repository");
    fs::create_dir(&repository).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repository)
            .status()
            .unwrap()
            .success()
    );
    let file = repository.join("file.txt");
    fs::write(&file, "old\n").unwrap();
    let script = directory.path().join("script.json");
    let response = |name: &str, arguments| {
        json!({
            "parts":[{"ToolCall":{"id":name,"name":name,"arguments":arguments}}],
            "usage":{"input":10,"cached_input":0,"output":2},
            "stop":"ToolUse","model":"scripted",
            "latency":{"secs":0,"nanos":0},"route":null
        })
    };
    fs::write(
        &script,
        serde_json::to_vec(&vec![
            response("read", json!({"path":"file.txt"})),
            response("edit", json!({"path":"file.txt","old":"old","new":"new"})),
            response("finish", json!({"summary":"edited the file"})),
        ])
        .unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args(["--run", "change file.txt", "--cwd"])
        .arg(&repository)
        .args(["--script"])
        .arg(&script)
        .args([
            "--native-sandbox",
            "read-only",
            "--max-steps",
            "4",
            "--max-seconds",
            "8",
        ])
        .output()
        .unwrap();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "old\n",
        "--native-sandbox read-only must prohibit edits; exit={:?}; stdout={}; stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn loom_bench_core_runs_edit_flow_and_applies_after_validation() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("repository-loom");
    fs::create_dir(&repository).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repository)
            .status()
            .unwrap()
            .success()
    );
    fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname='loom_cli_fixture'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::create_dir(repository.join("src")).unwrap();
    let file = repository.join("src/lib.rs");
    fs::write(
        &file,
        "pub fn value() -> u32 { 1 }\npub fn caller() -> u32 { value() }\n",
    )
    .unwrap();

    let script = directory.path().join("loom-script.json");
    let response = |name: &str, arguments| {
        json!({
            "parts":[{"ToolCall":{"id":name,"name":name,"arguments":arguments}}],
            "usage":{"input":10,"cached_input":0,"output":2},
            "stop":"ToolUse","model":"scripted",
            "latency":{"secs":0,"nanos":0},"route":null
        })
    };
    fs::write(
        &script,
        serde_json::to_vec(&vec![
            response("open", json!({"target":"src/lib.rs"})),
            response(
                "change",
                json!({"op":"edit","target":"src/lib.rs","old":"{ 1 }","new":"{ 2 }"}),
            ),
            response("finish", json!({"summary":"value corrected"})),
        ])
        .unwrap(),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom"))
        .args([
            "--backend",
            "loom",
            "--bench-core",
            "loom",
            "--run",
            "change value to 2",
            "--cwd",
        ])
        .arg(&repository)
        .args(["--script"])
        .arg(&script)
        .args([
            "--core-check",
            "grep -q '{ 2 }' src/lib.rs",
            "--max-steps",
            "8",
            "--max-seconds",
            "12",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "exit={:?}; stdout={}; stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(&file).unwrap().contains("{ 2 }"),
        "bench-core loom must apply the validated proposal"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"Loom\"") && stdout.contains("\"Applied\""),
        "{stdout}"
    );
}
