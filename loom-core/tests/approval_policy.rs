mod isolation;
use loom_core::{
    agent::{ApprovalPolicy, Core, InputControl, Intent, Options, Status, Task},
    effects::{ExecutionPolicy, SandboxMode},
    models::{Part, Response, ScriptedModel, Stop},
};
use serde_json::json;
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

fn repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(root.path())
            .status()
            .unwrap()
            .success()
    );
    fs::write(root.path().join("initial.txt"), "fixture").unwrap();
    root
}

fn model() -> ScriptedModel {
    ScriptedModel::new([Response {
        parts: vec![
            Part::ToolCall {
                id: "create".into(),
                name: "change".into(),
                arguments: json!({"op":"create","target":"result.txt","content":"validated result"}),
            },
            Part::ToolCall {
                id: "finish".into(),
                name: "finish".into(),
                arguments: json!({"summary":"created result"}),
            },
        ],
        usage: Default::default(),
        stop: Stop::ToolUse,
        model: "scripted".into(),
        latency: Duration::ZERO,
        route: None,
        gateway_connection: None,
        gateway_upstream_ms: None,
    }])
}

fn options(policy: ApprovalPolicy) -> Options {
    let mut control = InputControl::default();
    control.set_approval_policy(policy);
    Options {
        control,
        checks: vec![if cfg!(windows) {
            "if exist result.txt (exit /b 0) else (exit /b 1)"
        } else {
            "test -f result.txt"
        }
        .into()],
        steps: 1,
        seconds: 30,
        validation_reserve: 0,
        ..Default::default()
    }
}

#[test]
fn selected_approval_policy_controls_real_apply_and_is_durable() {
    for (policy, status) in [
        (ApprovalPolicy::OnRequest, Status::AwaitingApproval),
        (ApprovalPolicy::Untrusted, Status::AwaitingApproval),
        (ApprovalPolicy::Never, Status::Applied),
    ] {
        let root = repo();
        let mut core = Core::open(root.path()).unwrap();
        let task = core
            .execute(
                "create result",
                Intent::Edit,
                options(policy),
                &model(),
                Arc::new(AtomicBool::new(false)),
                &|_| {},
            )
            .unwrap();
        assert_eq!(task.status, status);
        assert_eq!(task.approval_policy, policy);
        assert_eq!(
            root.path().join("result.txt").exists(),
            policy == ApprovalPolicy::Never
        );
        let stored: Task = serde_json::from_slice(&fs::read(&task.records).unwrap()).unwrap();
        assert_eq!(stored.approval_policy, policy);
        assert_eq!(stored.status, status);
        assert_eq!(stored.policy, ExecutionPolicy::default());
        if policy != ApprovalPolicy::Never {
            let applied = core
                .approve(&task.id, &AtomicBool::new(false), &|_| {})
                .unwrap();
            assert_eq!(applied.status, Status::Applied);
            assert!(root.path().join("result.txt").exists());
        }
    }
}

#[test]
fn never_keeps_failed_validation_isolated() {
    let root = repo();
    let task = Core::open(root.path())
        .unwrap()
        .execute(
            "create result",
            Intent::Edit,
            Options {
                checks: vec!["exit 1".into()],
                ..options(ApprovalPolicy::Never)
            },
            &model(),
            Arc::new(AtomicBool::new(false)),
            &|_| {},
        )
        .unwrap();
    assert_eq!(task.status, Status::Unresolved);
    assert_eq!(task.approval_policy, ApprovalPolicy::Never);
    assert!(!root.path().join("result.txt").exists());
}

#[test]
fn never_keeps_readonly_restrictions() {
    for (read_only, sandbox) in [
        (true, SandboxMode::WorkspaceWrite),
        (false, SandboxMode::ReadOnly),
    ] {
        let root = repo();
        let result = Core::open(root.path()).unwrap().execute(
            "create result",
            Intent::Edit,
            Options {
                read_only,
                policy: ExecutionPolicy {
                    sandbox,
                    network: false,
                },
                ..options(ApprovalPolicy::Never)
            },
            &model(),
            Arc::new(AtomicBool::new(false)),
            &|_| {},
        );
        assert!(result.unwrap_err().to_string().contains("read-only"));
        assert!(!root.path().join("result.txt").exists());
    }
}

#[test]
fn policy_transport_is_snapshotted_and_legacy_task_defaults_are_conservative() {
    let mut control = InputControl::default();
    let snapshot = control.clone();
    control.set_approval_policy(ApprovalPolicy::Never);
    assert_eq!(snapshot.approval_policy(), ApprovalPolicy::OnRequest);
    assert_eq!(control.approval_policy(), ApprovalPolicy::Never);
    for value in ["on-request", "untrusted", "never"] {
        assert_eq!(ApprovalPolicy::parse(value).unwrap().as_str(), value);
    }
    assert!(ApprovalPolicy::parse("on-failure").is_err());
    assert!(ApprovalPolicy::parse("unknown").is_err());
    let root = repo();
    let task = Core::open(root.path())
        .unwrap()
        .execute(
            "create result",
            Intent::Edit,
            options(ApprovalPolicy::OnRequest),
            &model(),
            Arc::new(AtomicBool::new(false)),
            &|_| {},
        )
        .unwrap();
    let mut legacy = serde_json::to_value(task).unwrap();
    legacy.as_object_mut().unwrap().remove("approval_policy");
    assert_eq!(
        serde_json::from_value::<Task>(legacy)
            .unwrap()
            .approval_policy,
        ApprovalPolicy::OnRequest
    );
}

#[test]
fn existing_explicit_auto_apply_records_effective_never_policy() {
    let root = repo();
    let task = Core::open(root.path())
        .unwrap()
        .execute(
            "create result",
            Intent::Edit,
            Options {
                auto_apply: true,
                ..options(ApprovalPolicy::OnRequest)
            },
            &model(),
            Arc::new(AtomicBool::new(false)),
            &|_| {},
        )
        .unwrap();
    assert_eq!(task.status, Status::Applied);
    assert_eq!(task.approval_policy, ApprovalPolicy::Never);
}

#[test]
fn blocked_never_apply_preserves_user_changes_without_leaving_a_prompt() {
    let root = repo();
    let mut core = Core::open(root.path()).unwrap();
    let result = core.execute(
        "create result",
        Intent::Edit,
        options(ApprovalPolicy::Never),
        &model(),
        Arc::new(AtomicBool::new(false)),
        &|event| {
            if event.stage == loom_core::agent::events::Stage::Check
                && event.detail["started"] != true
            {
                fs::write(root.path().join("initial.txt"), "user changed this").unwrap();
            }
        },
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("workspace changed")
    );
    assert_eq!(
        fs::read_to_string(root.path().join("initial.txt")).unwrap(),
        "user changed this"
    );
    assert!(!root.path().join("result.txt").exists());
    let history = Core::open(root.path()).unwrap().history().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, Status::Unresolved);
    assert_eq!(history[0].approval_policy, ApprovalPolicy::Never);
}

#[test]
fn interrupted_never_apply_recovers_without_requesting_approval() {
    let root = repo();
    let task = Core::open(root.path())
        .unwrap()
        .execute(
            "create result",
            Intent::Edit,
            options(ApprovalPolicy::OnRequest),
            &model(),
            Arc::new(AtomicBool::new(false)),
            &|_| {},
        )
        .unwrap();
    let mut interrupted = task.clone();
    interrupted.approval_policy = ApprovalPolicy::Never;
    fs::write(&task.records, serde_json::to_vec(&interrupted).unwrap()).unwrap();
    let history = Core::open(root.path()).unwrap().history().unwrap();
    assert_eq!(history[0].status, Status::Unresolved);
    assert!(!root.path().join("result.txt").exists());
}

#[test]
fn cancellation_before_preparation_creates_no_overlay_or_task() {
    let root = repo();
    let mut core = Core::open(root.path()).unwrap();
    let model = model();
    let result = core.execute(
        "create result",
        Intent::Edit,
        options(ApprovalPolicy::Never),
        &model,
        Arc::new(AtomicBool::new(true)),
        &|_| {},
    );
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    assert!(!root.path().join(".loom/overlays").exists());
    assert!(core.history().unwrap().is_empty());
    assert!(model.requests().is_empty());
}

#[test]
fn never_revalidation_cancellation_preserves_workspace_without_prompting() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = repo();
    let cancel = Arc::new(AtomicBool::new(false));
    let checks = AtomicUsize::new(0);
    let mut core = Core::open(root.path()).unwrap();
    assert!(
        core.execute(
            "create result",
            Intent::Edit,
            options(ApprovalPolicy::Never),
            &model(),
            cancel.clone(),
            &|event| {
                if event.stage == loom_core::agent::events::Stage::Check
                    && event.detail["started"] == true
                    && checks.fetch_add(1, Ordering::SeqCst) == 1
                {
                    cancel.store(true, Ordering::Release);
                }
            }
        )
        .is_err()
    );
    assert!(!root.path().join("result.txt").exists());
    let history = core.history().unwrap();
    assert_eq!(history[0].status, Status::Cancelled);
    assert_eq!(history[0].approval_policy, ApprovalPolicy::Never);
}

#[test]
fn active_never_apply_is_not_actionable_approval_in_concurrent_history() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let root = repo();
    let checks = AtomicUsize::new(0);
    let prompted = AtomicBool::new(false);
    let task = Core::open(root.path())
        .unwrap()
        .execute(
            "create result",
            Intent::Edit,
            options(ApprovalPolicy::Never),
            &model(),
            Arc::new(AtomicBool::new(false)),
            &|event| {
                if event.stage == loom_core::agent::events::Stage::Check
                    && event.detail["started"] == true
                    && checks.fetch_add(1, Ordering::SeqCst) == 1
                {
                    let history = Core::open(root.path()).unwrap().history().unwrap();
                    prompted.store(history.iter().any(Task::pending), Ordering::Release);
                }
            },
        )
        .unwrap();
    assert!(
        !prompted.load(Ordering::Acquire),
        "another backend would request approval during automatic apply"
    );
    assert_eq!(task.status, Status::Applied);
}

#[test]
fn cancelled_pristine_worker_error_does_not_scan_the_discarded_overlay() {
    use loom_core::models::{ErrorKind, ModelPort, Need, ProviderError, Request, Served};
    use std::sync::atomic::Ordering;
    struct CancelBeforeEdits {
        root: std::path::PathBuf,
        cancel: Arc<AtomicBool>,
    }
    impl ModelPort for CancelBeforeEdits {
        fn complete(&self, _: Need, _: &Request) -> Result<Served, ProviderError> {
            // A pristine private worktree can be discarded on cancellation.
            // Any final capture here would fail instead of acknowledging stop.
            for revision in fs::read_dir(self.root.join(".loom/overlays"))
                .unwrap()
                .flatten()
            {
                if !revision.path().is_dir() {
                    continue;
                }
                for slot in fs::read_dir(revision.path()).unwrap().flatten() {
                    if slot.path().join(".git").exists() {
                        fs::remove_dir_all(slot.path()).unwrap();
                    }
                }
            }
            self.cancel.store(true, Ordering::Release);
            Err(ProviderError::new(
                ErrorKind::Cancelled,
                "cancelled before any edit",
            ))
        }
    }
    let root = repo();
    let cancel = Arc::new(AtomicBool::new(false));
    let model = CancelBeforeEdits {
        root: root.path().to_owned(),
        cancel: cancel.clone(),
    };
    let task = Core::open(root.path())
        .unwrap()
        .execute(
            "create result",
            Intent::Edit,
            options(ApprovalPolicy::Never),
            &model,
            cancel,
            &|_| {},
        )
        .unwrap();
    assert_eq!(task.status, Status::Cancelled);
    assert!(task.patch.files.is_empty());
    assert!(!root.path().join("result.txt").exists());
}

#[test]
fn cancellation_after_edit_retains_an_isolated_recoverable_patch() {
    use std::sync::atomic::Ordering;
    let root = repo();
    let cancel = Arc::new(AtomicBool::new(false));
    let task = Core::open(root.path())
        .unwrap()
        .execute(
            "create result",
            Intent::Edit,
            options(ApprovalPolicy::Never),
            &model(),
            cancel.clone(),
            &|event| {
                if event.stage == loom_core::agent::events::Stage::Change
                    && event.detail["completed"] == true
                {
                    cancel.store(true, Ordering::Release);
                }
            },
        )
        .unwrap();
    assert_eq!(task.status, Status::Cancelled);
    assert!(!root.path().join("result.txt").exists());
    assert!(!task.patch.files.is_empty());
    task.patch.apply(root.path()).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("result.txt")).unwrap(),
        "validated result"
    );
}

#[test]
fn preparation_phases_are_observable_before_task_work() {
    let root = repo();
    let events = std::sync::Mutex::new(Vec::new());
    Core::open(root.path())
        .unwrap()
        .execute(
            "create result",
            Intent::Edit,
            options(ApprovalPolicy::OnRequest),
            &model(),
            Arc::new(AtomicBool::new(false)),
            &|event| events.lock().unwrap().push(event),
        )
        .unwrap();
    let events = events.into_inner().unwrap();
    assert_eq!(events[0].target, "preparation");
    assert_eq!(events[0].stage, loom_core::agent::events::Stage::Direct);
    assert_eq!(events[0].detail, json!({"phase":"revision","started":true}));
    assert_eq!(events[1].target, "preparation");
    assert_eq!(events[1].detail, json!({"phase":"overlay","started":true}));
}
