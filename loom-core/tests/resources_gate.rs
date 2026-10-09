use loom_core::{
    base::{
        FileArtifactStore,
        resources::{AdmissionError, ResourceGovernor, ResourceKind},
    },
    effects::{ExecutionPolicy, RunStatus, Runner, SandboxMode},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

fn until(mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "condition was not reached"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn reservations_release_and_permanent_oversize_does_not_wait_forever() {
    let resources = ResourceGovernor::new(10, 4);
    let resident = resources
        .acquire(ResourceKind::Resident, 7, || Ok(()))
        .unwrap();
    assert!(matches!(
        resources.acquire(ResourceKind::Tool, 4, || Ok(())),
        Err(AdmissionError::TooLarge { resident: 7, .. })
    ));
    assert_eq!(resources.snapshot().waiting_tools, 0);
    drop(resident);
    let tool = resources.acquire(ResourceKind::Tool, 4, || Ok(())).unwrap();
    assert_eq!(resources.snapshot().tool_reserved_bytes, 4);
    drop(tool);
    assert_eq!(resources.snapshot().tool_reserved_bytes, 0);
    assert_eq!(resources.snapshot().resident_bytes, 0);
}

#[test]
fn queued_tools_run_before_more_inference_and_release_wakes_waiters() {
    let resources = ResourceGovernor::new(8, 6);
    let _resident = resources
        .acquire(ResourceKind::Resident, 2, || Ok(()))
        .unwrap();
    let busy = resources
        .acquire(ResourceKind::Inference, 6, || Ok(()))
        .unwrap();
    let (admitted, received) = mpsc::channel();
    let (release, released) = mpsc::channel();
    std::thread::scope(|scope| {
        let inference = Arc::clone(&resources);
        let admitted_inference = admitted.clone();
        scope.spawn(move || {
            let _work = inference
                .acquire(ResourceKind::Inference, 2, || Ok(()))
                .unwrap();
            admitted_inference.send("inference").unwrap();
        });
        until(|| resources.snapshot().waiting_inference == 1);
        let tools = Arc::clone(&resources);
        scope.spawn(move || {
            let _tool = tools.acquire(ResourceKind::Tool, 6, || Ok(())).unwrap();
            admitted.send("tool").unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        until(|| resources.snapshot().waiting_tools == 1);
        drop(busy);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            "tool"
        );
        assert_eq!(resources.snapshot().waiting_inference, 1);
        release.send(()).unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            "inference"
        );
    });
    assert_eq!(resources.snapshot().inference_bytes, 0);
    assert_eq!(resources.snapshot().tool_reserved_bytes, 0);
    assert_eq!(resources.snapshot().waits, 2);
}

#[test]
fn cancellation_and_deadlines_remove_waiters_without_needing_a_release() {
    let resources = ResourceGovernor::new(8, 4);
    let _busy = resources
        .acquire(ResourceKind::Inference, 8, || Ok(()))
        .unwrap();
    let cancelled = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let resources = &resources;
        let cancelled = &cancelled;
        let waiter = scope.spawn(move || {
            resources.acquire(ResourceKind::Tool, 4, || {
                if cancelled.load(Ordering::Acquire) {
                    Err(AdmissionError::Cancelled)
                } else {
                    Ok(())
                }
            })
        });
        until(|| resources.snapshot().waiting_tools == 1);
        cancelled.store(true, Ordering::Release);
        assert!(matches!(
            waiter.join().unwrap(),
            Err(AdmissionError::Cancelled)
        ));
    });
    let deadline = Instant::now() + Duration::from_millis(60);
    assert!(matches!(
        resources.acquire(ResourceKind::Inference, 1, || {
            if Instant::now() >= deadline {
                Err(AdmissionError::TimedOut)
            } else {
                Ok(())
            }
        }),
        Err(AdmissionError::TimedOut)
    ));
    assert_eq!(resources.snapshot().waiting_tools, 0);
    assert_eq!(resources.snapshot().waiting_inference, 0);
}

#[cfg(unix)]
#[test]
fn real_runner_holds_its_reservation_until_exit_then_inference_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let resources = ResourceGovernor::new(4, 2);
    let runner = Runner::with_resources(
        FileArtifactStore::open(dir.path()).unwrap(),
        Arc::clone(&resources),
    );
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let child = scope.spawn(|| {
            runner
                .run(
                    dir.path(),
                    "printf ready > ready; while [ ! -f finish ]; do sleep 0.02; done",
                    Duration::from_secs(5),
                    &cancel,
                    None,
                )
                .unwrap()
        });
        until(|| dir.path().join("ready").exists());
        assert_eq!(resources.snapshot().tool_reserved_bytes, 2);
        let inference = scope.spawn(|| {
            resources
                .acquire(ResourceKind::Inference, 3, || Ok(()))
                .unwrap()
        });
        until(|| resources.snapshot().waiting_inference == 1);
        std::fs::write(dir.path().join("finish"), "").unwrap();
        assert_eq!(child.join().unwrap().status, RunStatus::Passed);
        drop(inference.join().unwrap());
    });
    assert_eq!(resources.snapshot().tool_reserved_bytes, 0);
    assert_eq!(resources.snapshot().inference_bytes, 0);
    let failed = runner
        .run(dir.path(), "exit 7", Duration::from_secs(2), &cancel, None)
        .unwrap();
    assert_eq!(failed.status, RunStatus::Failed);
    assert_eq!(resources.snapshot().tool_reserved_bytes, 0);
}

#[cfg(unix)]
#[test]
fn resource_wait_counts_toward_tool_timeout_and_never_launches_cancelled_work() {
    let dir = tempfile::tempdir().unwrap();
    let resources = ResourceGovernor::new(4, 2);
    let runner = Runner::with_resources(
        FileArtifactStore::open(dir.path()).unwrap(),
        Arc::clone(&resources),
    );
    let busy = resources
        .acquire(ResourceKind::Inference, 4, || Ok(()))
        .unwrap();
    let result = runner
        .run(
            dir.path(),
            "touch launched",
            Duration::from_millis(60),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
    assert_eq!(result.status, RunStatus::TimedOut);
    assert!(!dir.path().join("launched").exists());
    let result = runner
        .run(
            dir.path(),
            "touch launched",
            Duration::from_secs(1),
            &AtomicBool::new(true),
            None,
        )
        .unwrap();
    assert_eq!(result.status, RunStatus::Cancelled);
    assert!(!dir.path().join("launched").exists());
    drop(busy);
    assert_eq!(resources.snapshot().waiting_tools, 0);
    assert_eq!(resources.snapshot().tool_reserved_bytes, 0);
}

#[cfg(unix)]
#[test]
fn pty_keeps_reservation_after_yield_and_returns_it_on_supervised_exit() {
    use loom_core::effects::TerminalManager;
    let dir = tempfile::tempdir().unwrap();
    let resources = ResourceGovernor::new(4, 2);
    let manager = TerminalManager::with_resources(
        dir.path(),
        FileArtifactStore::open(dir.path()).unwrap(),
        ExecutionPolicy {
            sandbox: SandboxMode::FullAccess,
            network: true,
        },
        Arc::clone(&resources),
    );
    let cancel = AtomicBool::new(false);
    let out = manager
        .exec("sleep 10", 0, Duration::from_secs(15), &cancel)
        .unwrap();
    let session = out
        .session_id
        .expect("command should be running after yield");
    assert_eq!(resources.snapshot().tool_reserved_bytes, 2);
    manager.cancel(session).unwrap();
    assert_eq!(resources.snapshot().tool_reserved_bytes, 0);
    manager.close(session).unwrap();

    let out = manager
        .exec("sleep 10", 0, Duration::from_millis(80), &cancel)
        .unwrap();
    assert!(out.session_id.is_some());
    until(|| resources.snapshot().tool_reserved_bytes == 0);
}
