use loom_core::base::{ArtifactStore, EventLog, FileArtifactStore, Revision};
use serde::{Deserialize, Serialize};
use std::{fs, process::Command, sync::Arc};

fn git(root: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn revision_is_content_based_and_preserves_index_and_head() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    fs::write(root.join("tracked.txt"), "committed\n").unwrap();
    fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Gate",
            "-c",
            "user.email=gate@example.test",
            "commit",
            "-qm",
            "base",
        ],
    );
    fs::write(root.join("tracked.txt"), "staged\n").unwrap();
    git(root, &["add", "tracked.txt"]);
    fs::write(root.join("tracked.txt"), "working\n").unwrap();
    fs::write(root.join("new file.txt"), "untracked\n").unwrap();
    let index = fs::read(root.join(".git/index")).unwrap();
    let head = git(root, &["rev-parse", "HEAD"]);
    let first = Revision::capture(root).unwrap();
    assert_eq!(first, Revision::capture(root).unwrap());
    fs::create_dir_all(root.join(".loom/artifacts")).unwrap();
    fs::write(root.join(".loom/artifacts/log"), "internal state").unwrap();
    fs::create_dir_all(root.join("ignored")).unwrap();
    fs::write(root.join("ignored/cache"), "ignored state").unwrap();
    assert_eq!(first, Revision::capture(root).unwrap());
    fs::write(root.join("tracked.txt"), "another\n").unwrap();
    assert_ne!(first, Revision::capture(root).unwrap());
    fs::write(root.join("tracked.txt"), "working\n").unwrap();
    assert_eq!(first, Revision::capture(root).unwrap());
    fs::remove_file(root.join("new file.txt")).unwrap();
    assert_ne!(first, Revision::capture(root).unwrap());
    assert_eq!(index, fs::read(root.join(".git/index")).unwrap());
    assert_eq!(head, git(root, &["rev-parse", "HEAD"]));
}

#[test]
fn revision_supports_unborn_repositories_and_file_modes() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    let empty = Revision::capture(dir.path()).unwrap();
    let path = dir.path().join("script");
    fs::write(&path, "echo hello\n").unwrap();
    let regular = Revision::capture(dir.path()).unwrap();
    assert_ne!(empty, regular);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(regular, Revision::capture(dir.path()).unwrap());
    }
}

#[cfg(unix)]
#[test]
fn raw_revision_round_trips_filters_crlf_binary_modes_and_unix_paths() {
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
    };
    let source = tempfile::tempdir().unwrap();
    let root = source.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "core.autocrlf", "true"]);
    git(root, &["config", "core.filemode", "false"]);
    git(root, &["config", "filter.lossy.clean", "sed '/USER/d'"]);
    git(
        root,
        &["config", "filter.lossy.smudge", "sed 's/base/smudged/g'"],
    );
    fs::write(
        root.join(".gitattributes"),
        "filtered.txt filter=lossy\ncrlf.txt text\n",
    )
    .unwrap();
    fs::write(root.join("filtered.txt"), b"base\nUSER first\n").unwrap();
    fs::write(root.join("crlf.txt"), b"one\r\ntwo\r\n").unwrap();
    fs::write(root.join("ignored.txt"), b"tracked but ignored\n").unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &[
            "-c",
            "user.name=Gate",
            "-c",
            "user.email=gate@example.test",
            "commit",
            "-qm",
            "base",
        ],
    );
    fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
    let index = fs::read(root.join(".git/index")).unwrap();
    let head = git(root, &["rev-parse", "HEAD"]);
    let first = Revision::capture(root).unwrap();
    fs::write(root.join("filtered.txt"), b"base\nUSER second\n").unwrap();
    assert_ne!(
        first,
        Revision::capture(root).unwrap(),
        "clean filters must not hide edits"
    );
    fs::write(root.join("binary.bin"), b"\0\xff\r\n\x80").unwrap();
    fs::write(root.join("script"), b"#!/bin/sh\r\necho raw\r\n").unwrap();
    fs::set_permissions(root.join("script"), fs::Permissions::from_mode(0o755)).unwrap();
    let unusual = Path::new("dir/quote\"\n한글.txt");
    fs::create_dir(root.join("dir")).unwrap();
    fs::write(root.join(unusual), b"odd filename\0").unwrap();
    symlink(unusual, root.join("link")).unwrap();
    let revision = Revision::capture(root).unwrap();
    let destination = tempfile::tempdir().unwrap();
    let worktree = destination.path().join("worktree");
    let commit = git(
        root,
        &[
            "-c",
            "user.name=Gate",
            "-c",
            "user.email=gate@example.test",
            "commit-tree",
            revision.as_str(),
            "-m",
            "overlay",
        ],
    );
    git(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            worktree.to_str().unwrap(),
            std::str::from_utf8(&commit).unwrap().trim(),
        ],
    );
    let destination_head = git(&worktree, &["rev-parse", "HEAD"]);
    git(&worktree, &["read-tree", revision.as_str()]);
    let index_path = git(&worktree, &["rev-parse", "--git-path", "index"]);
    let index_path = worktree.join(std::str::from_utf8(&index_path).unwrap().trim());
    let destination_index = fs::read(&index_path).unwrap();
    revision.materialize(&worktree).unwrap();
    assert_eq!(fs::read(&index_path).unwrap(), destination_index);
    for path in [
        Path::new("filtered.txt"),
        Path::new("crlf.txt"),
        Path::new("binary.bin"),
        Path::new("script"),
        Path::new("ignored.txt"),
        unusual,
    ] {
        assert_eq!(
            fs::read(root.join(path)).unwrap(),
            fs::read(worktree.join(path)).unwrap(),
            "{}",
            path.display()
        );
    }
    assert_eq!(fs::read_link(worktree.join("link")).unwrap(), unusual);
    assert_eq!(
        fs::metadata(worktree.join("script"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(Revision::capture(&worktree).unwrap(), revision);
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), destination_head);
    fs::write(worktree.join("filtered.txt"), b"overwrite this\n").unwrap();
    fs::write(worktree.join("unseen.txt"), b"preserve unseen\n").unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::write(outside.path(), b"outside bytes\n").unwrap();
    fs::remove_file(worktree.join("script")).unwrap();
    symlink(outside.path(), worktree.join("script")).unwrap();
    revision.materialize(&worktree).unwrap();
    assert_eq!(fs::read(&index_path).unwrap(), destination_index);
    assert_eq!(
        fs::read(worktree.join("filtered.txt")).unwrap(),
        b"base\nUSER second\n"
    );
    assert_eq!(
        fs::read(worktree.join("unseen.txt")).unwrap(),
        b"preserve unseen\n"
    );
    assert_eq!(fs::read(outside.path()).unwrap(), b"outside bytes\n");
    assert!(
        !fs::symlink_metadata(worktree.join("script"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(
        fs::metadata(worktree.join("script"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    fs::write(
        worktree.join("filtered.txt"),
        b"leave unchanged on preflight error\n",
    )
    .unwrap();
    fs::rename(worktree.join("dir"), worktree.join("unseen-directory")).unwrap();
    symlink("unseen-directory", worktree.join("dir")).unwrap();
    assert!(
        revision
            .materialize(&worktree)
            .unwrap_err()
            .to_string()
            .contains("symlink")
    );
    assert_eq!(
        fs::read(worktree.join("filtered.txt")).unwrap(),
        b"leave unchanged on preflight error\n"
    );
}

#[test]
fn capture_rejects_gitlinks_and_keeps_executor_batch_count_constant() {
    let source = tempfile::tempdir().unwrap();
    let root = source.path();
    git(root, &["init", "-q"]);
    for i in 0..128 {
        fs::write(root.join(format!("file-{i}.txt")), format!("raw {i}\r\n")).unwrap();
    }
    let mut calls = 0;
    let revision = Revision::capture_with(root, |command| {
        calls += 1;
        Ok(command.output()?)
    })
    .unwrap();
    assert!(
        calls <= 10,
        "capture spawned {calls} commands for 128 files"
    );
    assert!(
        Revision::capture_with(root, |_| anyhow::bail!("cancelled"))
            .unwrap_err()
            .to_string()
            .contains("Running Git")
    );
    let commit = git(
        root,
        &[
            "-c",
            "user.name=Gate",
            "-c",
            "user.email=gate@example.test",
            "commit-tree",
            revision.as_str(),
            "-m",
            "submodule",
        ],
    );
    git(
        root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!(
                "160000,{},submodule",
                std::str::from_utf8(&commit).unwrap().trim()
            ),
        ],
    );
    let index = fs::read(root.join(".git/index")).unwrap();
    fs::create_dir(root.join("submodule")).unwrap();
    fs::write(root.join("submodule/user-edit.txt"), "preserve").unwrap();
    assert!(
        Revision::capture(root)
            .unwrap_err()
            .to_string()
            .contains("submodule")
    );
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read(root.join("submodule/user-edit.txt")).unwrap(),
        b"preserve"
    );
    git(root, &["add", "file-0.txt"]);
    let tree = git(root, &["write-tree"]);
    let unsupported =
        Revision::try_from(std::str::from_utf8(&tree).unwrap().trim().to_owned()).unwrap();
    let commit = git(
        root,
        &[
            "-c",
            "user.name=Gate",
            "-c",
            "user.email=gate@example.test",
            "commit-tree",
            unsupported.as_str(),
            "-m",
            "gitlink overlay",
        ],
    );
    let destination = tempfile::tempdir().unwrap();
    let worktree = destination.path().join("worktree");
    git(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            worktree.to_str().unwrap(),
            std::str::from_utf8(&commit).unwrap().trim(),
        ],
    );
    let index = fs::read(root.join(".git/index")).unwrap();
    assert!(
        unsupported
            .materialize(&worktree)
            .unwrap_err()
            .to_string()
            .contains("submodule")
    );
    assert_eq!(
        fs::read_dir(&worktree).unwrap().count(),
        1,
        "reject gitlinks before writing any blobs"
    );
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
}

#[test]
fn cancelled_revision_operations_leave_workspace_and_index_unchanged() {
    use std::sync::atomic::AtomicBool;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "-q"]);
    fs::write(root.join("file.bin"), b"\0captured\xff").unwrap();
    git(root, &["add", "file.bin"]);
    let revision = Revision::capture(root).unwrap();
    fs::write(root.join("file.bin"), b"\0user edit\x80").unwrap();
    let index = fs::read(root.join(".git/index")).unwrap();
    let head = fs::read(root.join(".git/HEAD")).unwrap();
    let cancel = AtomicBool::new(true);
    for error in [Revision::capture_cancellable(root, &cancel).unwrap_err(),
        revision.materialize_cancellable(root, &cancel).unwrap_err()] {
        assert!(format!("{error:#}").contains("cancelled"), "{error:#}");
    }
    assert_eq!(fs::read(root.join("file.bin")).unwrap(), b"\0user edit\x80");
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(fs::read(root.join(".git/HEAD")).unwrap(), head);
    assert!(!fs::read_dir(root).unwrap().any(|entry|
        entry.unwrap().file_name().to_string_lossy().starts_with(".loom-materialize-")));
}

#[test]
fn cancellable_materialization_restores_raw_binary_blobs_and_keeps_index() {
    use std::sync::atomic::AtomicBool;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "filter.reject.smudge", "false"]);
    git(root, &["config", "filter.reject.required", "true"]);
    fs::write(root.join(".gitattributes"), "file.bin filter=reject\n").unwrap();
    let bytes: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
    fs::write(root.join("file.bin"), &bytes).unwrap();
    git(root, &["add", ".gitattributes"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        fs::set_permissions(root.join("file.bin"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("file.bin", root.join("link")).unwrap();
    }
    let revision = Revision::capture_cancellable(root, &AtomicBool::new(false)).unwrap();
    let index = fs::read(root.join(".git/index")).unwrap();
    let head = fs::read(root.join(".git/HEAD")).unwrap();
    fs::write(root.join("file.bin"), b"edited").unwrap();
    fs::write(root.join("unseen.txt"), b"preserve").unwrap();
    #[cfg(unix)]
    fs::remove_file(root.join("link")).unwrap();
    revision.materialize_cancellable(root, &AtomicBool::new(false)).unwrap();
    assert_eq!(fs::read(root.join("file.bin")).unwrap(), bytes);
    assert_eq!(fs::read(root.join("unseen.txt")).unwrap(), b"preserve");
    assert_eq!(fs::read(root.join(".git/index")).unwrap(), index);
    assert_eq!(fs::read(root.join(".git/HEAD")).unwrap(), head);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::read_link(root.join("link")).unwrap(), std::path::Path::new("file.bin"));
        assert_eq!(fs::metadata(root.join("file.bin")).unwrap().permissions().mode() & 0o777, 0o755);
    }
}

#[cfg(unix)]
#[test]
fn cancellable_status_preserves_configured_stdin_stdout_stderr_and_exit_code() {
    use loom_core::base::cancellable_status;
    use std::{io::{Read, Seek, SeekFrom, Write}, sync::atomic::AtomicBool};
    let bytes: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
    let mut input = tempfile::tempfile().unwrap();
    input.write_all(&bytes).unwrap();
    input.seek(SeekFrom::Start(0)).unwrap();
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let status = cancellable_status(Command::new("sh")
        .args(["-c", "cat; printf diagnostic >&2; exit 7"])
        .stdin(input).stdout(stdout.try_clone().unwrap()).stderr(stderr.try_clone().unwrap()),
        &AtomicBool::new(false)).unwrap();
    assert_eq!(status.code(), Some(7));
    stdout.seek(SeekFrom::Start(0)).unwrap();
    let mut actual = Vec::new();
    stdout.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, bytes);
    stderr.seek(SeekFrom::Start(0)).unwrap();
    let mut diagnostic = String::new();
    stderr.read_to_string(&mut diagnostic).unwrap();
    assert_eq!(diagnostic, "diagnostic");
    let marker = tempfile::tempdir().unwrap();
    assert!(cancellable_status(Command::new("sh").current_dir(marker.path())
        .args(["-c", "touch should-not-start"]), &AtomicBool::new(true)).is_err());
    assert!(!marker.path().join("should-not-start").exists());
}

#[test]
fn artifacts_round_trip_ranges_deduplicate_and_detect_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileArtifactStore::open(dir.path()).unwrap();
    let bytes = "원문\nraw output".as_bytes();
    let reference = store.put(bytes).unwrap();
    assert_eq!(reference, store.put(bytes).unwrap());
    assert_eq!(reference.bytes, bytes.len() as u64);
    assert_eq!(store.get(&reference, None).unwrap(), bytes);
    assert_eq!(store.get(&reference, Some(0..6)).unwrap(), &bytes[..6]);
    assert_eq!(
        store
            .get(&reference, Some(bytes.len()..bytes.len()))
            .unwrap(),
        b""
    );
    assert!(store.get(&reference, Some(0..bytes.len() + 1)).is_err());
    fs::write(store.path(&reference).unwrap(), b"corrupt").unwrap();
    assert!(store.get(&reference, None).is_err());
    assert!(
        serde_json::from_str::<loom_core::base::ArtifactRef>(r#"{"id":"../escape","bytes":1}"#)
            .is_err()
    );
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
enum Event {
    Set(String),
    Add(i64),
}

#[test]
fn event_replay_restores_state_and_recovers_an_interrupted_append() {
    let dir = tempfile::tempdir().unwrap();
    let log = EventLog::open(dir.path()).unwrap();
    assert_eq!(log.append(&Event::Set("task".into())).unwrap(), 1);
    assert_eq!(log.append(&Event::Add(3)).unwrap(), 2);
    let state = log
        .fold((String::new(), 0), |state, event| match event {
            Event::Set(name) => state.0 = name,
            Event::Add(n) => state.1 += n,
        })
        .unwrap();
    assert_eq!(state, ("task".into(), 3));
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(log.path())
        .unwrap();
    write!(file, "{{\"sequence\":3,").unwrap();
    drop(file);
    let reopened = EventLog::open(dir.path()).unwrap();
    assert_eq!(reopened.read::<Event>().unwrap().len(), 2);
    assert_eq!(reopened.append(&Event::Add(4)).unwrap(), 3);
    assert_eq!(reopened.read::<Event>().unwrap()[2].event, Event::Add(4));
    // A complete but corrupt record must never be silently discarded.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(log.path())
        .unwrap();
    writeln!(file, "invalid json").unwrap();
    assert!(reopened.read::<Event>().is_err());
    assert!(reopened.append(&Event::Add(5)).is_err());
}

#[test]
fn concurrent_event_writers_have_one_contiguous_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let root = Arc::new(dir.path().to_owned());
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let root = Arc::clone(&root);
            std::thread::spawn(move || {
                let log = EventLog::open(&root).unwrap();
                for _ in 0..10 {
                    log.append(&Event::Add(1)).unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let events = EventLog::open(&root).unwrap().read::<Event>().unwrap();
    assert_eq!(events.len(), 40);
    assert_eq!(
        events
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        (1..=40).collect::<Vec<_>>()
    );
}

#[test]
fn stat_cache_never_hides_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    std::process::Command::new("git").args(["init", "-q"]).current_dir(dir.path()).status().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "aaaa").unwrap();
    let first = Revision::capture(dir.path()).unwrap();
    // Racy: same size, rewritten right after the cache was written.
    std::fs::write(&file, "bbbb").unwrap();
    let racy = Revision::capture(dir.path()).unwrap();
    assert_ne!(first, racy, "an edit within the racy window is hashed again");
    // Settled: an old mtime is reused only while the stat matches exactly.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    std::fs::File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
    let settled = Revision::capture(dir.path()).unwrap();
    assert_eq!(settled, racy);
    std::fs::write(&file, "cccc").unwrap();
    std::fs::File::options().write(true).open(&file).unwrap()
        .set_modified(old + std::time::Duration::from_secs(1)).unwrap();
    let edited = Revision::capture(dir.path()).unwrap();
    assert_ne!(edited, settled, "a different mtime is hashed again");
    #[cfg(unix)]
    {
        let same_mtime = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "dddd").unwrap();
        fs::File::options().write(true).open(&file).unwrap().set_modified(same_mtime).unwrap();
        assert_ne!(Revision::capture(dir.path()).unwrap(), edited,
            "same-size edits with restored mtime must invalidate the stat cache");
        fs::write(&file, "cccc").unwrap();
    }
    // A cache naming a blob Git no longer has falls back to hashing everything.
    std::fs::write(dir.path().join(".loom/revision-stat-cache.json"), "{\"written_ns\":0,\"entries\":{}}").unwrap();
    assert_eq!(Revision::capture(dir.path()).unwrap(), edited);
}
