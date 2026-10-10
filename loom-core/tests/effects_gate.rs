mod isolation;
use loom_core::{
    base::{ArtifactStore, Revision},
    effects::{Overlay, RunStatus, Runner},
};
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn repository() -> tempfile::TempDir {
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
    directory
}

#[test]
fn overlay_changes_only_its_copy_and_patch_round_trips() {
    let directory = repository();
    let root = directory.path();
    let base = Revision::capture(root).unwrap();
    let mut overlay = Overlay::create(root, &base).unwrap();
    assert!(overlay.edit(Path::new("lib.rs"), "{ 1 }", "{ 2 }").is_err());
    let read = overlay.read(Path::new("lib.rs"), None).unwrap();
    assert_eq!(read.phase, loom_core::base::Phase::Orient);
    assert!(read.structured["text"].as_str().unwrap().contains("{ 1 }"));
    overlay.edit(Path::new("lib.rs"), "{ 1 }", "{ 2 }").unwrap();
    overlay.write(Path::new("new.txt"), "new file").unwrap();
    assert_eq!(
        fs::read_to_string(root.join("lib.rs")).unwrap(),
        "pub fn value() -> u32 { 1 }\n"
    );
    assert!(!root.join("new.txt").exists());
    let patch = overlay.patch().unwrap();
    assert_eq!(overlay.touched().unwrap().len(), 2);
    let overlay_tree = Revision::capture(overlay.dir()).unwrap();
    patch.apply(root).unwrap();
    assert_eq!(Revision::capture(root).unwrap(), overlay_tree);
    assert_eq!(patch.files.len(), 2);
    let path = overlay.dir().to_owned();
    drop(overlay);
    // Removal runs in the background so a turn does not wait for it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!path.exists());
}

#[test]
fn overlay_materialization_uses_pinned_content_when_source_changed() {
    let directory = repository();
    let base = Revision::capture(directory.path()).unwrap();
    let original = fs::read(directory.path().join("lib.rs")).unwrap();
    fs::write(directory.path().join("lib.rs"), "pub fn value() -> u32 { 99 }\n").unwrap();
    let overlay = Overlay::create(directory.path(), &base).unwrap();
    assert_eq!(fs::read(overlay.dir().join("lib.rs")).unwrap(), original);
    assert_eq!(Revision::capture(overlay.dir()).unwrap(), base);
    assert!(fs::read_to_string(directory.path().join("lib.rs")).unwrap().contains("99"));
}

#[test]
fn overlay_creation_sets_stable_mtimes_without_changing_content_or_source() {
    let directory = repository();
    let root = directory.path();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("nested/data.bin"), [0, 255, 1, 128]).unwrap();
    fs::write(root.join("empty.txt"), []).unwrap();
    let base = Revision::capture(root).unwrap();
    let sources: Vec<_> = ["lib.rs", "nested/data.bin", "empty.txt"]
        .into_iter()
        .map(|name| {
            (name, fs::read(root.join(name)).unwrap(),
                fs::metadata(root.join(name)).unwrap().modified().unwrap())
        })
        .collect();
    let expected_mtime = std::time::UNIX_EPOCH + Duration::from_secs(1);
    for cancellable in [false, true] {
        let overlay = if cancellable {
            Overlay::create_cancellable(root, &base, &AtomicBool::new(false))
        } else {
            Overlay::create(root, &base)
        }.unwrap();
        for (name, content, source_mtime) in &sources {
            let copied = overlay.dir().join(name);
            assert_eq!(&fs::read(&copied).unwrap(), content);
            assert_eq!(fs::metadata(&copied).unwrap().modified().unwrap(), expected_mtime);
            assert_eq!(&fs::read(root.join(name)).unwrap(), content);
            assert_eq!(fs::metadata(root.join(name)).unwrap().modified().unwrap(), *source_mtime);
        }
        assert_eq!(Revision::capture(overlay.dir()).unwrap(), base);
    }
}

#[test]
fn initial_overlay_cancelled_creation_does_not_touch_the_workspace() {
    let directory = repository();
    let base = Revision::capture(directory.path()).unwrap();
    let result = Overlay::create_cancellable(directory.path(), &base, &AtomicBool::new(true));
    assert!(result.err().is_some_and(|error| error.to_string().contains("cancelled")));
    assert!(!directory.path().join(".loom/overlays").exists());
}

#[test]
fn initial_overlay_cancellable_creation_keeps_pinned_content_on_clone_fallback() {
    let directory = repository();
    let base = Revision::capture(directory.path()).unwrap();
    let original = fs::read(directory.path().join("lib.rs")).unwrap();
    fs::write(directory.path().join("lib.rs"), "pub fn changed_source() {}\n").unwrap();
    let overlay = Overlay::create_cancellable(directory.path(), &base, &AtomicBool::new(false)).unwrap();
    assert_eq!(fs::read(overlay.dir().join("lib.rs")).unwrap(), original);
    assert_eq!(Revision::capture(overlay.dir()).unwrap(), base);
}

#[cfg(unix)]
#[test]
fn overlay_fallback_cancels_during_materialization_without_touching_source() {
    use std::os::unix::fs::PermissionsExt;
    let harness = tempfile::tempdir().unwrap();
    let real_git = Command::new("sh").args(["-c", "command -v git"]).output().unwrap();
    assert!(real_git.status.success());
    let real_git = std::str::from_utf8(&real_git.stdout).unwrap().trim();
    let wrapper = harness.path().join("git");
    // Delay only the blob spool. PATH is changed solely in the isolated test child.
    fs::write(&wrapper, r#"#!/bin/sh
for arg in "$@"; do
    if [ "$arg" = cat-file ]; then
        printf started > "$LOOM_CANCEL_GIT_MARKER"
        sleep 2
    fi
done
exec "$LOOM_CANCEL_REAL_GIT" "$@"
"#).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::join_paths(std::iter::once(harness.path().to_owned())
        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap()))).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "overlay_materialization_cancellation_child", "--ignored", "--nocapture"])
        .env("PATH", path)
        .env("LOOM_CANCEL_REAL_GIT", real_git)
        .env("LOOM_CANCEL_GIT_MARKER", harness.path().join("cat-file-started"))
        .output().unwrap();
    assert!(output.status.success(), "{}\n{}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[cfg(unix)]
#[test]
#[ignore = "invoked by the parent test with an isolated Git wrapper"]
fn overlay_materialization_cancellation_child() {
    let Some(marker) = std::env::var_os("LOOM_CANCEL_GIT_MARKER") else { return };
    let marker = std::path::PathBuf::from(marker);
    let directory = repository();
    let root = directory.path();
    assert!(Command::new("git").current_dir(root).args(["add", "lib.rs"])
        .status().unwrap().success());
    let base = Revision::capture(root).unwrap();
    // Force the APFS clone verification to fail and enter immutable materialization.
    let changed = b"pub fn changed_source() {}\n";
    fs::write(root.join("lib.rs"), changed).unwrap();
    let index = fs::read(root.join(".git/index")).unwrap();
    let head = fs::read(root.join(".git/HEAD")).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = cancel.clone();
    let interrupter = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let reached_spool = marker.exists();
        let cancelled_at = Instant::now();
        trigger.store(true, Ordering::Release);
        (reached_spool, cancelled_at)
    });
    let result = Overlay::create_cancellable(root, &base, &cancel);
    let finished = Instant::now();
    let (reached_spool, cancelled_at) = interrupter.join().unwrap();
    assert!(reached_spool, "test never reached materialization's cat-file spool");
    let error = result.err().expect("cancelled materialization succeeded");
    assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
    assert!(finished.duration_since(cancelled_at) < Duration::from_millis(750),
        "cancellation waited for the delayed blob spool");
    assert_eq!(fs::read(root.join("lib.rs")).unwrap(), changed);
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(fs::read(root.join(".git/HEAD")).unwrap(), head);
}

#[test]
fn initial_overlay_cancellation_interrupts_registry_lock_wait() {
    use fs2::FileExt;
    let directory = repository();
    let base = Revision::capture(directory.path()).unwrap();
    let registry = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(directory.path().join(".loom/overlays.lock")).unwrap();
    registry.lock_exclusive().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let root = directory.path().to_owned();
    let (started, entering) = std::sync::mpsc::channel();
    let (finished, done) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        started.send(()).unwrap();
        let result = Overlay::create_cancellable(&root, &base, &worker_cancel).map(|_| ());
        finished.send(result).unwrap();
    });
    entering.recv().unwrap();
    let waiting = done.recv_timeout(Duration::from_millis(30));
    cancel.store(true, Ordering::Release);
    // Keep the lock held until after observing cancellation; a blocking lock cannot pass.
    let result = done.recv_timeout(Duration::from_millis(500));
    drop(registry);
    worker.join().unwrap();
    assert!(matches!(waiting, Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
        "registry contention returned instead of waiting: {waiting:?}");
    assert!(result.expect("cancelled preparation waited for the registry lock")
        .unwrap_err().to_string().contains("cancelled"));
}

#[test]
fn initial_overlay_stat_cache_reuses_hashes_but_detects_restored_mtime_edits() {
    let directory = repository();
    let base = Revision::capture(directory.path()).unwrap();
    let overlay = Overlay::create(directory.path(), &base).unwrap();
    let file = overlay.dir().join("lib.rs");
    let timestamp = fs::metadata(&file).unwrap().modified().unwrap();
    assert_eq!(Revision::capture(overlay.dir()).unwrap(), base);
    let mut hash_batches = 0;
    let mut capture = || Revision::capture_with(overlay.dir(), |command| {
        if command.get_args().any(|arg| arg == "hash-object") {
            hash_batches += 1;
        }
        Ok(command.output()?)
    });
    assert_eq!(capture().unwrap(), base);
    assert_eq!(hash_batches, 0, "unchanged materialized files should reuse their blob hashes");
    assert!(timestamp > std::time::UNIX_EPOCH);
    #[cfg(unix)]
    {
        fs::write(&file, "pub fn value() -> u32 { 2 }\n").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(timestamp).unwrap();
        assert_ne!(Revision::capture(overlay.dir()).unwrap(), base,
            "same-size edits with restored stable mtime must still invalidate the cache");
    }
}

#[test]
fn sequential_overlays_reuse_build_paths_but_concurrent_overlays_are_distinct() {
    let directory = repository();
    let base = Revision::capture(directory.path()).unwrap();
    let first = Overlay::create(directory.path(), &base).unwrap();
    let path = first.dir().to_owned();
    let timestamp = fs::metadata(path.join("lib.rs"))
        .unwrap()
        .modified()
        .unwrap();
    let concurrent = Overlay::create(directory.path(), &base).unwrap();
    let cancellable = Overlay::create_cancellable(directory.path(), &base, &AtomicBool::new(false)).unwrap();
    assert_ne!(first.dir(), concurrent.dir());
    assert_ne!(first.dir(), cancellable.dir());
    assert_ne!(concurrent.dir(), cancellable.dir());
    let original = fs::read(directory.path().join("lib.rs")).unwrap();
    for overlay in [&first, &concurrent, &cancellable] {
        assert_eq!(fs::read(overlay.dir().join("lib.rs")).unwrap(), original);
        assert_eq!(Revision::capture(overlay.dir()).unwrap(), base);
    }
    drop(first);
    {
        use fs2::FileExt;
        let lease = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match lease.try_lock_exclusive() {
                Ok(()) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "overlay cleanup did not release {}",
                        path.display()
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("overlay lease check failed: {error}"),
            }
        }
    }
    let second = Overlay::create(directory.path(), &base).unwrap();
    assert_eq!(second.dir(), path);
    assert_eq!(
        fs::metadata(path.join("lib.rs"))
            .unwrap()
            .modified()
            .unwrap(),
        timestamp
    );
}

#[test]
fn patch_conflicts_preserve_user_changes_and_stale_reads_are_rejected() {
    let directory = repository();
    let root = directory.path();
    let mut overlay = Overlay::create(root, &Revision::capture(root).unwrap()).unwrap();
    overlay.read(Path::new("lib.rs"), None).unwrap();
    fs::write(
        overlay.dir().join("lib.rs"),
        "pub fn value() -> u32 { 3 }\n",
    )
    .unwrap();
    assert!(overlay.edit(Path::new("lib.rs"), "{ 3 }", "{ 4 }").is_err());
    overlay.read(Path::new("lib.rs"), None).unwrap();
    assert!(overlay.edit(Path::new("lib.rs"), "{ 3 }", "{ }").is_ok());
    let patch = overlay.patch().unwrap();
    fs::write(root.join("lib.rs"), "user edit\n").unwrap();
    assert!(patch.apply(root).is_err());
    assert_eq!(
        fs::read_to_string(root.join("lib.rs")).unwrap(),
        "user edit\n"
    );
    assert!(overlay.write(Path::new("../escape"), "bad").is_err());
    assert!(overlay.write(Path::new(".git/config"), "bad").is_err());
}

#[cfg(unix)]
#[test]
fn overlay_refuses_symlinks_that_escape_isolation() {
    use std::os::unix::fs::symlink;
    let directory = repository();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("file"), "outside").unwrap();
    symlink(outside.path(), directory.path().join("escape")).unwrap();
    let mut overlay = Overlay::create(
        directory.path(),
        &Revision::capture(directory.path()).unwrap(),
    )
    .unwrap();
    assert!(overlay.read(Path::new("escape/file"), None).is_err());
    assert!(overlay.write(Path::new("escape/new"), "bad").is_err());
    assert!(!outside.path().join("new").exists());
}

#[test]
fn runner_structures_failures_and_keeps_the_raw_output() {
    let directory = repository();
    let base = Revision::capture(directory.path()).unwrap();
    let overlay = Overlay::create(directory.path(), &base).unwrap();
    let runner = Runner::new(overlay.artifacts().clone());
    let command = if cfg!(windows) {
        "echo test result: FAILED. 2 passed; 1 failed; & echo ---- broken stdout ---- & exit /b 1"
    } else {
        "printf 'test result: FAILED. 2 passed; 1 failed;\\n---- broken stdout ----\\n'; exit 1"
    };
    let result = runner.run(overlay.dir(), command, Duration::from_secs(5), &AtomicBool::new(false), None).unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.passed, 2);
    assert_eq!(result.failed, 1);
    assert_eq!(result.failing, ["broken"]);
    assert!(
        runner
            .artifacts()
            .get(&result.artifact, None)
            .unwrap()
            .starts_with(b"$ ")
    );
}

#[test]
fn zero_exit_with_failed_tests_is_still_a_failure_with_an_artifact() {
    let directory = repository();
    let overlay = Overlay::create(
        directory.path(),
        &Revision::capture(directory.path()).unwrap(),
    )
    .unwrap();
    let runner = Runner::new(overlay.artifacts().clone());
    let result = runner
        .run(
            overlay.dir(),
            if cfg!(windows) {
                "echo test result: FAILED. 0 passed; 1 failed; & exit /b 0"
            } else {
                "printf 'test result: FAILED. 0 passed; 1 failed;\\n'"
            },
            Duration::from_secs(5),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.failed, 1);
    assert!(
        runner
            .artifacts()
            .get(&result.artifact, None)
            .unwrap()
            .len()
            > 10
    );
}

#[test]
fn patch_paths_are_stable_when_git_disables_diff_prefixes() {
    let directory = repository();
    assert!(
        Command::new("git")
            .current_dir(directory.path())
            .args(["config", "diff.noprefix", "true"])
            .status()
            .unwrap()
            .success()
    );
    fs::create_dir(directory.path().join("src")).unwrap();
    fs::write(directory.path().join("src/file.txt"), "old\n").unwrap();
    let mut overlay = Overlay::create(
        directory.path(),
        &Revision::capture(directory.path()).unwrap(),
    )
    .unwrap();
    overlay.read(Path::new("src/file.txt"), None).unwrap();
    overlay
        .edit(Path::new("src/file.txt"), "old", "new")
        .unwrap();
    overlay.patch().unwrap().apply(directory.path()).unwrap();
    assert_eq!(
        fs::read_to_string(directory.path().join("src/file.txt")).unwrap(),
        "new\n"
    );
    assert!(!directory.path().join("file.txt").exists());
}

#[cfg(unix)]
#[test]
fn overlays_and_merged_patches_preserve_bytes_under_clean_and_smudge_filters() {
    let directory = repository();
    for args in [
        ["config", "filter.lossy.clean", "sed '/USER/d'"],
        ["config", "filter.lossy.smudge", "sed 's/base/smudged/g'"],
    ] {
        assert!(
            Command::new("git")
                .current_dir(directory.path())
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(
        directory.path().join(".gitattributes"),
        "filtered.txt filter=lossy\n",
    )
    .unwrap();
    let original = "base\nkeep 1\nkeep 2\nkeep 3\nkeep 4\nkeep 5\nkeep 6\nUSER first\n";
    fs::write(directory.path().join("filtered.txt"), original).unwrap();
    let base = Revision::capture(directory.path()).unwrap();
    let mut proposal = Overlay::create(directory.path(), &base).unwrap();
    assert_eq!(
        fs::read_to_string(proposal.dir().join("filtered.txt")).unwrap(),
        original
    );
    proposal.read(Path::new("filtered.txt"), None).unwrap();
    proposal
        .edit(Path::new("filtered.txt"), "base", "changed")
        .unwrap();
    let patch = proposal.patch().unwrap();
    fs::write(
        directory.path().join("filtered.txt"),
        original.replace("first", "second"),
    )
    .unwrap();
    let merged = Overlay::create(
        directory.path(),
        &Revision::capture(directory.path()).unwrap(),
    )
    .unwrap();
    patch.merge_into(&merged).unwrap();
    let expected = original
        .replace("base", "changed")
        .replace("first", "second");
    assert_eq!(
        fs::read_to_string(merged.dir().join("filtered.txt")).unwrap(),
        expected
    );
    merged.patch().unwrap().apply(directory.path()).unwrap();
    assert_eq!(
        fs::read_to_string(directory.path().join("filtered.txt")).unwrap(),
        expected
    );
}

#[cfg(unix)]
#[test]
fn raw_patch_round_trips_binary_deletions_executable_modes_symlinks_and_attributes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = repository();
    let root = directory.path();
    fs::write(root.join("bytes.bin"), b"\0\xff\r\n").unwrap();
    fs::write(root.join("script"), b"#!/bin/sh\necho old\n").unwrap();
    symlink("lib.rs", root.join("link")).unwrap();
    fs::write(root.join(".gitattributes"), "bytes.bin -text\n").unwrap();
    let overlay = Overlay::create(root, &Revision::capture(root).unwrap()).unwrap();
    fs::write(overlay.dir().join("bytes.bin"), b"\0\x80changed\xff\r\n").unwrap();
    fs::set_permissions(
        overlay.dir().join("script"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::remove_file(overlay.dir().join("link")).unwrap();
    symlink("bytes.bin", overlay.dir().join("link")).unwrap();
    fs::remove_file(overlay.dir().join("lib.rs")).unwrap();
    fs::write(
        overlay.dir().join(".gitattributes"),
        "* -text\nbytes.bin binary\n",
    )
    .unwrap();
    let expected = Revision::capture(overlay.dir()).unwrap();
    overlay.patch().unwrap().apply(root).unwrap();
    assert_eq!(Revision::capture(root).unwrap(), expected);
    assert_eq!(
        fs::read_link(root.join("link")).unwrap(),
        Path::new("bytes.bin")
    );
    assert!(!root.join("lib.rs").exists());
}

#[test]
fn runner_timeout_and_pre_cancel_never_report_passed() {
    let directory = repository();
    let overlay = Overlay::create(
        directory.path(),
        &Revision::capture(directory.path()).unwrap(),
    )
    .unwrap();
    let runner = Runner::new(overlay.artifacts().clone());
    let result = runner
        .run(
            overlay.dir(),
            if cfg!(windows) {
                "powershell.exe -NoProfile -NonInteractive -Command \"Start-Sleep -Seconds 30\""
            } else {
                "sleep 30"
            },
            Duration::from_millis(50),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
    assert_eq!(result.status, RunStatus::TimedOut);
    let result = runner
        .run(
            overlay.dir(),
            if cfg!(windows) {
                "type nul > should-not-run"
            } else {
                "touch should-not-run"
            },
            Duration::from_secs(5),
            &AtomicBool::new(true),
            None,
        )
        .unwrap();
    assert_eq!(result.status, RunStatus::Cancelled);
    assert!(!overlay.dir().join("should-not-run").exists());
}

#[test]
fn rename_preserves_comments_and_literals_across_files() {
    let directory = repository();
    fs::write(
        directory.path().join("caller.rs"),
        "// value stays here\npub fn caller() -> u32 { let _ = \"value\"; value() }\n",
    )
    .unwrap();
    let world = loom_core::world::World::open(directory.path()).unwrap();
    let mut overlay = Overlay::create(directory.path(), &world.snapshot.revision).unwrap();
    overlay.rename("lib.rs::value", "answer", &world).unwrap();
    assert!(
        fs::read_to_string(overlay.dir().join("lib.rs"))
            .unwrap()
            .contains("fn answer()")
    );
    assert_eq!(
        fs::read_to_string(overlay.dir().join("caller.rs")).unwrap(),
        "// value stays here\npub fn caller() -> u32 { let _ = \"value\"; answer() }\n"
    );
    assert_eq!(overlay.touched().unwrap().len(), 2);
}

#[cfg(unix)]
#[test]
fn runner_cancellation_terminates_the_process_group() {
    let directory = repository();
    let overlay = Overlay::create(
        directory.path(),
        &Revision::capture(directory.path()).unwrap(),
    )
    .unwrap();
    let runner = Runner::new(overlay.artifacts().clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let trigger = Arc::clone(&cancel);
    let pid_file = overlay.dir().join("child.pid");
    let watched = pid_file.clone();
    let thread = std::thread::spawn(move || {
        let started = Instant::now();
        while !watched.exists() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        trigger.store(true, Ordering::SeqCst);
    });
    let start = Instant::now();
    let result = runner
        .run(
            overlay.dir(),
            "sleep 30 & echo $! > child.pid; wait",
            Duration::from_secs(10),
            &cancel,
            None,
        )
        .unwrap();
    thread.join().unwrap();
    assert_eq!(result.status, RunStatus::Cancelled);
    assert!(start.elapsed() < Duration::from_secs(5));
    let pid: i32 = fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // macOS may retain a killed child briefly as a zombie; it must not be running.
    let ps = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    assert!(ps.stdout.is_empty() || String::from_utf8_lossy(&ps.stdout).trim().starts_with('Z'));
}

#[cfg(unix)]
#[test]
fn applying_a_content_edit_preserves_private_file_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let d = repository();
    let path = d.path().join("private.txt");
    fs::write(&path, "private old\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut overlay = Overlay::create(d.path(), &Revision::capture(d.path()).unwrap()).unwrap();
    overlay
        .read(std::path::Path::new("private.txt"), None)
        .unwrap();
    overlay
        .edit(
            std::path::Path::new("private.txt"),
            "private old",
            "private new",
        )
        .unwrap();
    overlay.patch().unwrap().apply(d.path()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "private new\n");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn abandoned_overlays_of_other_revisions_are_swept_but_leased_ones_are_kept() {
    let directory = repository();
    let root = directory.path();
    let overlays = root.join(".loom/overlays");
    // A revision left behind by a process that ended: slot dir + free slot lock.
    let abandoned = overlays.join("0000abandoned");
    fs::create_dir_all(abandoned.join("0/src")).unwrap();
    fs::write(abandoned.join("0/src/big.bin"), vec![0u8; 4096]).unwrap();
    fs::write(abandoned.join("0.lock"), b"").unwrap();
    // A revision a live worker still leases.
    let live = overlays.join("1111live");
    fs::create_dir_all(live.join("0")).unwrap();
    let lease = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(live.join("0.lock"))
        .unwrap();
    fs2::FileExt::lock_exclusive(&lease).unwrap();
    let base = Revision::capture(root).unwrap();
    let overlay = Overlay::create(root, &base).unwrap();
    assert!(!abandoned.exists(), "abandoned revision swept");
    assert!(live.exists(), "leased revision kept");
    drop(overlay);
    drop(lease);
}

#[cfg(unix)]
#[test]
fn restricted_commands_get_a_writable_uv_cache_and_offline_policy() {
    let directory = repository();
    let base = Revision::capture(directory.path()).unwrap();
    let overlay = Overlay::create(directory.path(), &base).unwrap();
    let runner = Runner::new(overlay.artifacts().clone());
    let result = runner.run_with_policy(overlay.dir(),
        "test -n \"$UV_CACHE_DIR\" && test \"$UV_OFFLINE\" = 1 && mkdir -p \"$UV_CACHE_DIR\" && touch \"$UV_CACHE_DIR/probe\"",
        Duration::from_secs(5), &AtomicBool::new(false), None,
        loom_core::effects::ExecutionPolicy::default()).unwrap();
    assert_eq!(result.status, RunStatus::Passed);
    assert!(overlay.dir().join(".loom/tmp/uv-cache/probe").is_file());
    if std::process::Command::new("uv").arg("--version").output().is_ok() {
        let result = runner.run_with_policy(overlay.dir(),
            "uv init --bare --vcs none --no-python-downloads --no-workspace ml-demo",
            Duration::from_secs(10), &AtomicBool::new(false), None,
            loom_core::effects::ExecutionPolicy::default()).unwrap();
        let output = runner.artifacts().get(&result.artifact, None).unwrap();
        assert_eq!(result.status, RunStatus::Passed, "{}", String::from_utf8_lossy(&output));
        assert!(overlay.dir().join("ml-demo/pyproject.toml").is_file());
    }
}
