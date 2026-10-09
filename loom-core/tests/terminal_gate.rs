#![cfg(unix)]

use loom_core::{
    base::{ArtifactStore, FileArtifactStore},
    effects::{ExecutionPolicy, SandboxMode, TerminalManager, TerminalOutput},
};
use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn full_access() -> ExecutionPolicy {
    ExecutionPolicy {
        sandbox: SandboxMode::FullAccess,
        network: true,
    }
}
fn manager(root: &Path) -> (TerminalManager, FileArtifactStore) {
    let store = FileArtifactStore::open(root).unwrap();
    (
        TerminalManager::new(root, store.clone(), full_access()),
        store,
    )
}
fn finish(manager: &TerminalManager, mut out: TerminalOutput) -> TerminalOutput {
    let started = Instant::now();
    while let Some(id) = out.session_id {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "session did not finish"
        );
        out = manager
            .write_stdin(id, "", 100, &AtomicBool::new(false))
            .unwrap();
    }
    out
}
fn assert_dead(pid: i32) {
    let started = Instant::now();
    loop {
        let ps = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        if ps.stdout.is_empty() || String::from_utf8_lossy(&ps.stdout).trim().starts_with('Z') {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "child {pid} survived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn child_pid(root: &Path) -> i32 {
    let start = Instant::now();
    loop {
        if let Ok(pid) = fs::read_to_string(root.join("child.pid"))
            && let Ok(pid) = pid.trim().parse()
        {
            return pid;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn policy_defaults_and_serde_are_explicit() {
    let policy = ExecutionPolicy::default();
    assert_eq!(policy.sandbox, SandboxMode::WorkspaceWrite);
    assert!(!policy.network);
    assert_eq!(
        serde_json::from_str::<ExecutionPolicy>("{}").unwrap(),
        policy
    );
    assert_eq!(
        serde_json::from_str::<ExecutionPolicy>(&serde_json::to_string(&policy).unwrap()).unwrap(),
        policy
    );
}

#[test]
fn real_pty_accepts_interactive_input_and_keeps_raw_bytes() {
    let root = tempfile::tempdir().unwrap();
    let (manager, store) = manager(root.path());
    let cancel = AtomicBool::new(false);
    let out = manager.exec("test -t 0 && test -t 1 && test -t 2 || exit 9; printf 'ready>' > /dev/tty; read answer < /dev/tty; printf 'answer=%s\\n' \"$answer\"", 100, Duration::from_secs(5), &cancel).unwrap();
    assert!(out.output.contains("ready>"));
    let id = out.session_id.expect("interactive session must persist");
    let out = finish(
        &manager,
        manager.write_stdin(id, "hello\n", 500, &cancel).unwrap(),
    );
    assert_eq!(out.exit_code, Some(0));
    let raw = store.get(&out.artifact, None).unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("answer=hello"));
    assert!(raw.windows(2).any(|w| w == b"\r\n"));
}

#[test]
fn output_is_bounded_but_artifact_contains_all_bytes() {
    let root = tempfile::tempdir().unwrap();
    let (manager, store) = manager(root.path());
    let out = manager
        .exec(
            "head -c 200000 /dev/zero | tr '\\000' x; printf '\\377END'",
            2000,
            Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
    let out = finish(&manager, out);
    assert_eq!(out.exit_code, Some(0));
    assert!(out.output.len() <= 32768);
    let raw = store.get(&out.artifact, None).unwrap();
    assert_eq!(raw.len(), 200004);
    assert!(raw.ends_with(b"\xffEND"));
}

#[test]
fn timeout_is_enforced_after_yield_and_kills_descendants() {
    let root = tempfile::tempdir().unwrap();
    let (manager, _) = manager(root.path());
    let out = manager
        .exec(
            "sleep 30 & echo $! > child.pid; wait",
            0,
            Duration::from_millis(400),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(out.session_id.is_some());
    let pid = child_pid(root.path());
    std::thread::sleep(Duration::from_millis(700));
    assert_dead(pid);
    let out = finish(&manager, out);
    assert_ne!(out.exit_code, Some(0));
}

#[test]
fn cancellation_during_wait_kills_child() {
    let root = tempfile::tempdir().unwrap();
    let (manager, _) = manager(root.path());
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = cancel.clone();
    let path = root.path().to_owned();
    let thread = std::thread::spawn(move || {
        let pid = child_pid(&path);
        trigger.store(true, Ordering::SeqCst);
        pid
    });
    let out = manager
        .exec(
            "sleep 30 & echo $! > child.pid; wait",
            5000,
            Duration::from_secs(10),
            &cancel,
        )
        .unwrap();
    let pid = thread.join().unwrap();
    assert!(out.session_id.is_none());
    assert_ne!(out.exit_code, Some(0));
    assert_dead(pid);
}

#[test]
fn cancel_close_and_drop_kill_children() {
    for action in ["cancel", "close", "drop"] {
        let root = tempfile::tempdir().unwrap();
        let (manager, _) = manager(root.path());
        let out = manager
            .exec(
                "sleep 30 & echo $! > child.pid; wait",
                50,
                Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .unwrap();
        let id = out.session_id.unwrap();
        let pid = child_pid(root.path());
        match action {
            "cancel" => {
                manager.cancel(id).unwrap();
            }
            "close" => {
                manager.close(id).unwrap();
            }
            _ => drop(manager),
        }
        assert_dead(pid);
    }
}

#[test]
fn dropping_interactive_shell_kills_jobs_in_separate_process_groups() {
    let root = tempfile::tempdir().unwrap();
    let (manager, _) = manager(root.path());
    let out = manager
        .exec(
            "/bin/bash -i -c 'sleep 30 & echo $! > child.pid; wait'",
            100,
            Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(out.session_id.is_some());
    let pid = child_pid(root.path());
    // bash job control creates a group distinct from the session leader.
    assert_ne!(unsafe { libc::getpgid(pid) }, unsafe { libc::getsid(pid) });
    drop(manager);
    assert_dead(pid);
}

#[test]
fn readonly_terminal_uses_shared_artifacts_and_private_temp_without_source_writes() {
    let root = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let store = FileArtifactStore::open(shared.path()).unwrap();
    let manager = TerminalManager::new(
        root.path(),
        store.clone(),
        ExecutionPolicy {
            sandbox: SandboxMode::ReadOnly,
            network: false,
        },
    );
    let out = manager.exec("test -t 0 || exit 8; f=$(mktemp); echo temp > \"$f\"; cat \"$f\"; echo build > \"$CARGO_TARGET_DIR/built\"; echo denied > source", 1000, Duration::from_secs(5), &AtomicBool::new(false));
    #[cfg(target_os = "linux")]
    if let Err(error) = &out {
        assert!(error.to_string().contains("sandbox bwrap unavailable"));
        assert!(!Path::new("/usr/bin/bwrap").is_file() && !Path::new("/bin/bwrap").is_file());
        return;
    }
    let out = finish(&manager, out.unwrap());
    assert_ne!(out.exit_code, Some(0));
    assert!(String::from_utf8_lossy(&store.get(&out.artifact, None).unwrap()).contains("temp"));
    assert!(root.path().join(".loom/target/built").exists());
    assert!(!root.path().join("source").exists());
}

#[test]
fn sandbox_accepts_writable_files_and_denies_adjacent_files() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let file = outside.path().join("stdout");
    fs::write(&file, "").unwrap();
    let script = format!(
        "echo output > '{}'; echo bad > '{}'",
        file.display(),
        outside.path().join("denied").display()
    );
    if let Some(out) = sandbox_command(
        ExecutionPolicy::default(),
        root.path(),
        std::slice::from_ref(&file),
        &script,
    ) {
        assert!(!out.status.success());
        assert_eq!(fs::read_to_string(file).unwrap(), "output\n");
    }
    assert!(!outside.path().join("denied").exists());
}

#[test]
fn pre_cancel_and_zero_timeout_do_not_launch_commands() {
    let root = tempfile::tempdir().unwrap();
    let (manager, _) = manager(root.path());
    for (timeout, cancelled) in [(Duration::from_secs(1), true), (Duration::ZERO, false)] {
        let out = manager
            .exec("touch forbidden", 0, timeout, &AtomicBool::new(cancelled))
            .unwrap();
        assert!(out.session_id.is_none());
        assert_ne!(out.exit_code, Some(0));
        assert!(!root.path().join("forbidden").exists());
    }
    assert!(
        manager
            .write_stdin(u64::MAX, "", 0, &AtomicBool::new(false))
            .is_err()
    );
}

fn sandbox_command(
    policy: ExecutionPolicy,
    root: &Path,
    writable: &[std::path::PathBuf],
    script: &str,
) -> Option<std::process::Output> {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", script])
        .current_dir(root)
        .env("LOOM_TEST_ENV", "kept");
    if let Err(error) = policy.configure_command(&mut cmd, root, writable) {
        // Missing Linux bwrap is allowed only as an explicit fail-closed error.
        #[cfg(target_os = "linux")]
        {
            assert!(error.to_string().contains("sandbox bwrap unavailable"));
            assert!(!Path::new("/usr/bin/bwrap").is_file() && !Path::new("/bin/bwrap").is_file());
            return None;
        }
        #[cfg(not(target_os = "linux"))]
        panic!("{error:#}");
    }
    Some(cmd.output().unwrap())
}

#[test]
fn sandbox_denies_outside_writes_and_preserves_reads_and_environment() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("readable"), "source").unwrap();
    let script = format!(
        "test \"$LOOM_TEST_ENV\" = kept && cat '{}' && echo ok > local && (echo escape > '{}' || exit 7)",
        outside.path().join("readable").display(),
        outside.path().join("denied").display()
    );
    if let Some(out) = sandbox_command(ExecutionPolicy::default(), root.path(), &[], &script) {
        assert_eq!(out.status.code(), Some(7), "{out:?}");
        assert_eq!(out.stdout, b"source");
        assert!(root.path().join("local").exists());
    }
    assert!(!outside.path().join("denied").exists());
}

#[test]
fn readonly_denies_repository_writes_but_allows_explicit_shared_build_paths() {
    let root = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let target = shared.path().join(".loom/target");
    fs::create_dir_all(&target).unwrap();
    let policy = ExecutionPolicy {
        sandbox: SandboxMode::ReadOnly,
        network: false,
    };
    let script = format!(
        "echo built > '{}'; echo denied > source",
        target.join("built").display()
    );
    if let Some(out) = sandbox_command(policy, root.path(), std::slice::from_ref(&target), &script)
    {
        assert!(!out.status.success());
        assert_eq!(fs::read_to_string(target.join("built")).unwrap(), "built\n");
    }
    assert!(!root.path().join("source").exists());
}

#[test]
fn sandbox_denies_config_and_symlink_escape_writes() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::write(root.path().join(".git/config"), "original").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
    let _ = sandbox_command(
        ExecutionPolicy::default(),
        root.path(),
        &[],
        "echo bad > .git/config; echo bad > escape/denied",
    );
    assert_eq!(
        fs::read_to_string(root.path().join(".git/config")).unwrap(),
        "original"
    );
    assert!(!outside.path().join("denied").exists());
}

#[test]
fn network_is_denied_and_explicit_network_access_works() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let root = tempfile::tempdir().unwrap();
    let script = format!(
        "/bin/bash -c 'echo probe > /dev/tcp/127.0.0.1/{}'",
        listener.local_addr().unwrap().port()
    );
    if let Some(out) = sandbox_command(ExecutionPolicy::default(), root.path(), &[], &script) {
        assert!(!out.status.success(), "network must be denied");
    }
    let policy = ExecutionPolicy {
        network: true,
        ..ExecutionPolicy::default()
    };
    if let Some(out) = sandbox_command(policy, root.path(), &[], &script) {
        assert!(out.status.success(), "{out:?}");
    }
}

#[test]
fn full_access_is_an_explicit_filesystem_escape_hatch() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let script = format!(
        "echo explicit > '{}'",
        outside.path().join("allowed").display()
    );
    assert!(
        sandbox_command(full_access(), root.path(), &[], &script)
            .unwrap()
            .status
            .success()
    );
    assert!(outside.path().join("allowed").exists());
}

#[test]
fn sandbox_denials_are_recognized_in_test_output() {
    assert!(loom_core::effects::denied_by_sandbox("panicked: Operation not permitted (os error 1)"));
    assert!(loom_core::effects::denied_by_sandbox("Os { code: 1, kind: PermissionDenied }"));
    assert!(!loom_core::effects::denied_by_sandbox("assertion failed: left == right"));
    assert!(!loom_core::effects::loopback_allowed() || std::env::var("LOOM_SANDBOX_LOOPBACK").is_ok());
}
