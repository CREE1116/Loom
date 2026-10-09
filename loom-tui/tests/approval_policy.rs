use anyhow::{Result, ensure};
use loom_tui::{
    agent::*,
    backend::{
        version_core::{self as core, Settings, Version},
        versions::VersionBackend,
    },
};
use loom_core::models as m;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

const BEFORE: &str = "pub fn value() -> u32 { 1 }\n";
const AFTER: &str = "pub fn value() -> u32 { 2 }\n";

fn fixture() -> Result<tempfile::TempDir> {
    let root = tempfile::tempdir()?;
    ensure!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()?
            .success(),
        "git init failed"
    );
    fs::create_dir(root.path().join("src"))?;
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
    )?;
    fs::write(root.path().join("src/lib.rs"), BEFORE)?;
    Ok(root)
}

fn script() -> Arc<m::ScriptedModel> {
    Arc::new(m::ScriptedModel::new([m::Response {
        parts: vec![
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
        ],
        usage: Default::default(),
        stop: m::Stop::ToolUse,
        model: "fixture-model".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    }]))
}

fn settings(state: &tempfile::TempDir) -> Settings {
    Settings {
        state: Some(state.path().to_owned()),
        checks: vec!["grep -q '{ 2 }' src/lib.rs".into()],
        steps: 2,
        attempts: 1,
        ..Default::default()
    }
}

fn runtime_policy_path(
    root: &tempfile::TempDir,
    state: &tempfile::TempDir,
) -> Result<std::path::PathBuf> {
    let canonical = root.path().canonicalize()?;
    let workspace = format!(
        "{:x}",
        Sha256::digest(canonical.as_os_str().as_encoded_bytes())
    );
    Ok(core::state_directory(&settings(state))
        .join("permissions")
        .join(format!("{workspace}.json")))
}

fn backend(root: &tempfile::TempDir, settings: Settings) -> Result<VersionBackend> {
    VersionBackend::with_model(Version::Loom, root.path(), script(), settings)
}

fn update(backend: &mut VersionBackend, kind: &str, value: &str) -> Result<()> {
    backend.command(AgentCommand::UpdatePermission {
        kind: kind.into(),
        value: value.into(),
    })
}

fn submit(backend: &mut VersionBackend) -> Result<()> {
    backend.command(AgentCommand::Submit {
        text: ":edit fix value in src/lib.rs".into(),
        model: "fixture-model".into(),
        effort: None,
        images: vec![],
        skills: vec![],
    })
}

fn wait(
    backend: &mut VersionBackend,
    predicate: impl Fn(&AgentUpdate) -> bool,
) -> Result<AgentUpdate> {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = backend.poll()? {
            if predicate(&event.update) {
                return Ok(event.update);
            }
        }
        ensure!(
            Instant::now() < until,
            "timed out waiting for expected event"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn initial_permissions(backend: &mut VersionBackend) -> Result<Permissions> {
    let AgentUpdate::SessionLoaded { permissions, .. } =
        wait(backend, |u| matches!(u, AgentUpdate::SessionLoaded { .. }))?
    else {
        unreachable!()
    };
    Ok(permissions)
}

fn changed_permissions(backend: &mut VersionBackend) -> Result<Permissions> {
    let AgentUpdate::PermissionsUpdated { permissions, .. } = wait(backend, |u| {
        matches!(u, AgentUpdate::PermissionsUpdated { .. })
    })?
    else {
        unreachable!()
    };
    Ok(permissions)
}

#[test]
fn repository_supplied_never_cannot_grant_automatic_approval() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    fs::create_dir(root.path().join(".loom"))?;
    fs::write(
        root.path().join(".loom/approval-policy.json"),
        r#"{"approval_policy":"never"}"#,
    )?;
    let mut agent = backend(&root, settings(&state))?;
    let reported = initial_permissions(&mut agent)?.approval;
    submit(&mut agent)?;
    let event = wait(&mut agent, |u| {
        matches!(
            u,
            AgentUpdate::ApprovalRequested(_) | AgentUpdate::TurnCompleted { .. }
        )
    })?;
    let source = fs::read_to_string(root.path().join("src/lib.rs"))?;
    agent.shutdown()?;
    assert!(
        reported.as_deref() == Some("on-request")
            && matches!(event, AgentUpdate::ApprovalRequested(_))
            && source == BEFORE,
        "repository file granted approval: reported={reported:?}, event={event:?}, source={source:?}"
    );
    Ok(())
}

#[test]
fn never_selection_is_accepted() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let mut agent = backend(&root, settings(&state))?;
    let result = update(&mut agent, "approval", "never");
    assert!(result.is_ok(), "never selection rejected: {result:?}");
    agent.shutdown()?;
    Ok(())
}

#[test]
fn accepted_modes_report_and_restore_user_runtime_selection_per_workspace() -> Result<()> {
    for mode in ["on-request", "untrusted", "never"] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let mut agent = backend(&root, settings(&state))?;
        assert_eq!(
            initial_permissions(&mut agent)?.approval.as_deref(),
            Some("on-request")
        );
        update(&mut agent, "approval", mode)?;
        assert_eq!(
            changed_permissions(&mut agent)?.approval.as_deref(),
            Some(mode)
        );
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(runtime_policy_path(&root, &state)?)?)?;
        assert_eq!(persisted, json!({"approval_policy": mode}));
        assert!(!root.path().join(".loom/approval-policy.json").exists());
        agent.shutdown()?;
        let mut restored = backend(&root, settings(&state))?;
        assert_eq!(
            initial_permissions(&mut restored)?.approval.as_deref(),
            Some(mode)
        );
        restored.shutdown()?;
        let other_root = fixture()?;
        let mut other = backend(&other_root, settings(&state))?;
        assert_eq!(
            initial_permissions(&mut other)?.approval.as_deref(),
            Some("on-request")
        );
        other.shutdown()?;
        let fresh_state = tempfile::tempdir()?;
        let mut fresh = backend(&root, settings(&fresh_state))?;
        assert_eq!(
            initial_permissions(&mut fresh)?.approval.as_deref(),
            Some("on-request")
        );
        fresh.shutdown()?;
    }
    Ok(())
}

#[test]
fn invalid_mode_leaves_report_persistence_and_runtime_unchanged() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let mut agent = backend(&root, settings(&state))?;
    initial_permissions(&mut agent)?;
    update(&mut agent, "approval", "never")?;
    changed_permissions(&mut agent)?;
    let path = runtime_policy_path(&root, &state)?;
    let saved = fs::read(&path)?;
    for invalid in ["always", "Never", "", "never ", "on_request"] {
        assert!(update(&mut agent, "approval", invalid).is_err());
        assert_eq!(fs::read(&path)?, saved);
    }
    update(&mut agent, "sandbox", "workspaceWrite")?;
    assert_eq!(
        changed_permissions(&mut agent)?.approval.as_deref(),
        Some("never")
    );
    submit(&mut agent)?;
    expect_auto_applied(&mut agent)?;
    assert_eq!(fs::read_to_string(root.path().join("src/lib.rs"))?, AFTER);
    agent.shutdown()?;
    Ok(())
}

fn expect_auto_applied(agent: &mut VersionBackend) -> Result<()> {
    let event = wait(agent, |u| {
        matches!(
            u,
            AgentUpdate::ApprovalRequested(_) | AgentUpdate::TurnCompleted { .. }
        )
    })?;
    assert!(
        matches!(event, AgentUpdate::TurnCompleted { ref status, .. } if status == "completed"),
        "never must apply without requesting approval: {event:?}"
    );
    Ok(())
}

#[test]
fn user_runtime_restored_never_applies_real_core_edit_without_approval() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let mut agent = backend(&root, settings(&state))?;
    update(&mut agent, "approval", "never")?;
    agent.shutdown()?;
    let mut restored = backend(&root, settings(&state))?;
    initial_permissions(&mut restored)?;
    submit(&mut restored)?;
    expect_auto_applied(&mut restored)?;
    assert_eq!(fs::read_to_string(root.path().join("src/lib.rs"))?, AFTER);
    let tasks = loom_core::agent::Core::open(root.path())?.history()?;
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].approval_policy,
        loom_core::agent::ApprovalPolicy::Never
    );
    assert_eq!(
        tasks[0].policy,
        loom_core::effects::ExecutionPolicy::default()
    );
    restored.shutdown()?;
    Ok(())
}

#[test]
fn on_request_and_untrusted_keep_edit_pending_and_permission_guard() -> Result<()> {
    for mode in ["on-request", "untrusted"] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let mut agent = backend(&root, settings(&state))?;
        update(&mut agent, "approval", mode)?;
        submit(&mut agent)?;
        assert!(
            update(&mut agent, "approval", "never").is_err(),
            "active guard"
        );
        let event = wait(&mut agent, |u| {
            matches!(
                u,
                AgentUpdate::ApprovalRequested(_) | AgentUpdate::TurnCompleted { .. }
            )
        })?;
        assert!(
            matches!(event, AgentUpdate::ApprovalRequested(_)),
            "{event:?}"
        );
        assert_eq!(fs::read_to_string(root.path().join("src/lib.rs"))?, BEFORE);
        let path = runtime_policy_path(&root, &state)?;
        let saved = fs::read(&path)?;
        assert!(
            update(&mut agent, "approval", "never").is_err(),
            "pending guard"
        );
        assert_eq!(fs::read(&path)?, saved);
        agent.shutdown()?;
    }
    Ok(())
}

#[test]
fn never_preserves_read_only_and_workspace_trust_restrictions() -> Result<()> {
    for untrusted in [false, true] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let mut options = settings(&state);
        options.readonly = !untrusted;
        let model = script();
        let mut agent =
            VersionBackend::with_model(Version::Loom, root.path(), model.clone(), options)?;
        initial_permissions(&mut agent)?;
        if untrusted {
            agent.command(AgentCommand::TrustWorkspace(false))?;
            changed_permissions(&mut agent)?;
        }
        update(&mut agent, "approval", "never")?;
        let permissions = changed_permissions(&mut agent)?;
        assert_eq!(permissions.approval.as_deref(), Some("never"));
        assert_eq!(permissions.sandbox.as_deref(), Some("readOnly"));
        if untrusted {
            assert!(update(&mut agent, "sandbox", "workspaceWrite").is_err());
        }
        assert!(submit(&mut agent).is_err());
        assert!(model.requests().is_empty());
        assert_eq!(fs::read_to_string(root.path().join("src/lib.rs"))?, BEFORE);
        agent.shutdown()?;
    }
    Ok(())
}

#[test]
fn invalid_sandbox_does_not_clear_read_only() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let mut options = settings(&state);
    options.readonly = true;
    let mut agent = backend(&root, options)?;
    initial_permissions(&mut agent)?;
    assert!(update(&mut agent, "sandbox", "invalid").is_err());
    update(&mut agent, "approval", "on-request")?;
    assert_eq!(
        changed_permissions(&mut agent)?.sandbox.as_deref(),
        Some("readOnly")
    );
    assert!(submit(&mut agent).is_err());
    agent.shutdown()?;
    Ok(())
}

#[test]
fn validation_failure_with_never_preserves_files_and_selected_policy() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let mut options = settings(&state);
    options.checks = vec!["false".into()];
    let mut agent = backend(&root, options)?;
    initial_permissions(&mut agent)?;
    update(&mut agent, "approval", "never")?;
    changed_permissions(&mut agent)?;
    submit(&mut agent)?;
    let event = wait(&mut agent, |u| {
        matches!(
            u,
            AgentUpdate::ApprovalRequested(_) | AgentUpdate::TurnCompleted { .. }
        )
    })?;
    assert!(
        matches!(event, AgentUpdate::TurnCompleted { ref status, .. } if status != "completed"),
        "failed validation cannot apply or request approval: {event:?}"
    );
    assert_eq!(fs::read_to_string(root.path().join("src/lib.rs"))?, BEFORE);
    update(&mut agent, "sandbox", "workspaceWrite")?;
    assert_eq!(
        changed_permissions(&mut agent)?.approval.as_deref(),
        Some("never")
    );
    agent.shutdown()?;
    Ok(())
}

#[test]
fn failed_persistence_does_not_change_policy() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let mut agent = backend(&root, settings(&state))?;
    initial_permissions(&mut agent)?;
    fs::create_dir_all(runtime_policy_path(&root, &state)?)?;
    assert!(update(&mut agent, "approval", "never").is_err());
    update(&mut agent, "sandbox", "workspaceWrite")?;
    assert_eq!(
        changed_permissions(&mut agent)?.approval.as_deref(),
        Some("on-request")
    );
    submit(&mut agent)?;
    let event = wait(&mut agent, |u| {
        matches!(
            u,
            AgentUpdate::ApprovalRequested(_) | AgentUpdate::TurnCompleted { .. }
        )
    })?;
    assert!(
        matches!(event, AgentUpdate::ApprovalRequested(_)),
        "{event:?}"
    );
    assert_eq!(fs::read_to_string(root.path().join("src/lib.rs"))?, BEFORE);
    agent.shutdown()?;
    Ok(())
}

#[test]
fn preserve_explicit_caller_policy_when_user_runtime_selection_is_absent() -> Result<()> {
    for policy in [
        loom_core::agent::ApprovalPolicy::Untrusted,
        loom_core::agent::ApprovalPolicy::Never,
    ] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let mut options = settings(&state);
        options.input_control.set_approval_policy(policy);
        let mut agent = backend(&root, options)?;
        assert_eq!(
            initial_permissions(&mut agent)?.approval.as_deref(),
            Some(policy.as_str())
        );
        assert!(!root.path().join(".loom/approval-policy.json").exists());
        assert!(!runtime_policy_path(&root, &state)?.exists());
        agent.shutdown()?;
    }
    Ok(())
}

#[test]
fn preserve_untrusted_network_denial_after_approval_choice() -> Result<()> {
    let root = fixture()?;
    let state = tempfile::tempdir()?;
    let mut agent = backend(&root, settings(&state))?;
    agent.command(AgentCommand::TrustWorkspace(false))?;
    for mode in ["on-request", "untrusted", "never"] {
        update(&mut agent, "approval", mode)?;
        for allow in ["allow", "true"] {
            assert!(
                update(&mut agent, "network", allow).is_err(),
                "untrusted network {allow}"
            );
        }
        for deny in ["deny", "false"] {
            update(&mut agent, "network", deny)?;
        }
    }
    agent.shutdown()?;
    Ok(())
}

#[test]
fn invalid_user_runtime_policy_is_rejected_without_rewriting_state() -> Result<()> {
    for stored in [
        r#"{"approval_policy":"always"}"#,
        r#"{"approval_policy":"Never"}"#,
        r#"{"approval_policy":"never","network":true}"#,
        r#"{"approval_policy":false}"#,
        "{}",
        "invalid json",
    ] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        let path = runtime_policy_path(&root, &state)?;
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, stored)?;
        assert!(
            backend(&root, settings(&state)).is_err(),
            "accepted {stored}"
        );
        assert_eq!(fs::read_to_string(path)?, stored);
        assert_eq!(fs::read_to_string(root.path().join("src/lib.rs"))?, BEFORE);
    }
    Ok(())
}

#[test]
fn invalid_repository_policy_files_are_ignored() -> Result<()> {
    for stored in [
        r#"{"approval_policy":"always"}"#,
        r#"{"approval_policy":"never","network":true}"#,
        "{}",
        "invalid json",
    ] {
        let root = fixture()?;
        let state = tempfile::tempdir()?;
        fs::create_dir(root.path().join(".loom"))?;
        let path = root.path().join(".loom/approval-policy.json");
        fs::write(&path, stored)?;
        let mut agent = backend(&root, settings(&state))?;
        assert_eq!(
            initial_permissions(&mut agent)?.approval.as_deref(),
            Some("on-request")
        );
        assert_eq!(fs::read_to_string(&path)?, stored);
        assert!(!runtime_policy_path(&root, &state)?.exists());
        agent.shutdown()?;
    }
    Ok(())
}
