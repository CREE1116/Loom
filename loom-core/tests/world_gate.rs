mod isolation;
use loom_core::{base::Revision, effects::Overlay, world::{GraphIndex, World}};
use std::{fs, path::Path, process::Command, sync::{Arc, Mutex, MutexGuard}};

/// One test swaps the process-wide PATH for a slow `git`; the others run git too.
fn env_lock() -> MutexGuard<'static, ()> {
    static ENV: Mutex<()> = Mutex::new(());
    ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repository() -> tempfile::TempDir {
    // These tests count parses; the shared on-disk symbol cache would serve repeat runs.
    // SAFETY: set before any thread reads the environment in this test binary's tests.
    unsafe { std::env::set_var("LOOM_NO_GRAPH_CACHE", "1") };
    let dir = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .current_dir(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    fs::write(
        dir.path().join("a.rs"),
        "pub struct Opts { pub depth: usize }\npub fn a() {}\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("b.rs"),
        "pub enum Mode { Fast, Slow(u32) }\npub fn b() { a(); }\n",
    )
    .unwrap();
    dir
}

#[test]
fn cancelled_index_cannot_build_or_return_a_cached_graph() {
    let _env = env_lock();
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = repository();
    let revision = Revision::capture(dir.path()).unwrap();
    let graph = Arc::new(GraphIndex::default());
    let flag = AtomicBool::new(true);
    assert!(World::open_at_cancellable(dir.path(), &revision, graph.clone(), &flag).is_err());
    assert_eq!(graph.parses(), 0);
    flag.store(false, Ordering::Release);
    let world = World::open_at_cancellable(dir.path(), &revision, graph.clone(), &flag).unwrap();
    flag.store(true, Ordering::Release);
    assert!(world.index.snapshot_at_cancellable(&revision, &flag).is_err());
    assert!(graph.graph_cancellable(&world.snapshot.files, &flag).is_err());
}

#[test]
#[cfg(unix)]
fn cancellation_during_snapshot_blob_loading_stops_the_git_process() {
    let _env = env_lock();
    use std::{os::unix::fs::PermissionsExt, sync::{mpsc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
    let dir = repository();
    let revision = Revision::capture(dir.path()).unwrap();
    let scripts = tempfile::tempdir().unwrap();
    let marker = scripts.path().join("entered");
    let old_path = std::env::var_os("PATH").unwrap();
    let git = std::env::split_paths(&old_path).map(|path| path.join("git")).find(|path| path.is_file()).unwrap();
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let wrapper = scripts.path().join("git");
    fs::write(&wrapper, format!("#!/bin/sh\nif [ \"$1\" = cat-file ]; then : > {}; sleep 5; fi\nexec {} \"$@\"\n", quote(&marker), quote(&git))).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    struct Restore(std::ffi::OsString);
    impl Drop for Restore { fn drop(&mut self) { unsafe { std::env::set_var("PATH", &self.0); } } }
    let _restore = Restore(old_path.clone());
    let paths = std::env::join_paths(std::iter::once(scripts.path().to_owned()).chain(std::env::split_paths(&old_path))).unwrap();
    unsafe { std::env::set_var("PATH", paths); }
    let flag = Arc::new(AtomicBool::new(false));
    let cancel = flag.clone();
    let root = dir.path().to_owned();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = World::open_at_cancellable(&root, &revision, Arc::new(GraphIndex::default()), &cancel);
        let _ = tx.send(result.map(|_| ()));
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !marker.exists() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(5)); }
    assert!(marker.exists());
    flag.store(true, Ordering::Release);
    assert!(rx.recv_timeout(Duration::from_millis(350)).expect("indexing ignored Esc").is_err());
    worker.join().unwrap();
}

#[test]
fn initial_pinned_world_skips_recapture_and_advance_observes_live_edits() {
    let _env = env_lock();
    let dir = repository();
    let base = Revision::capture(dir.path()).unwrap();
    // Materialization must still use the captured blobs if the source has changed.
    fs::write(dir.path().join("b.rs"), "pub fn changed_source() {}\n").unwrap();
    let mut overlay = Overlay::create(dir.path(), &base).unwrap();
    let cache = overlay.dir().join(".loom/revision-stat-cache.json");
    if cache.exists() {
        fs::remove_file(&cache).unwrap();
    }
    let world = World::open_at(overlay.dir(), &overlay.base, Arc::new(GraphIndex::default())).unwrap();
    assert!(!cache.exists(), "initial indexing must not recapture the materialized overlay");
    assert_eq!(world.snapshot.revision, base);
    assert!(world.graph.get("b.rs::b").is_some());
    assert!(world.graph.get("b.rs::changed_source").is_none());
    overlay.read(Path::new("b.rs"), None).unwrap();
    overlay.edit(Path::new("b.rs"), "pub fn b()", "pub fn edited()").unwrap();
    let next = world.advance(&[]).unwrap();
    assert!(cache.exists(), "refresh must capture live edits rather than reuse the initial base");
    assert_ne!(next.snapshot.revision, base);
    assert!(next.graph.get("b.rs::edited").is_some());
    assert!(next.graph.get("b.rs::b").is_none());
    assert!(world.graph.get("b.rs::b").is_some());
}

#[test]
fn advance_reparses_one_file_and_keeps_the_old_world_immutable() {
    let _env = env_lock();
    let dir = repository();
    let world = World::open(dir.path()).unwrap();
    assert_eq!(world.parses(), 2);
    fs::write(dir.path().join("b.rs"), "pub fn b2() { a(); }\n").unwrap();
    let next = world.advance(&[Path::new("b.rs").to_owned()]).unwrap();
    assert_eq!(next.parses(), 3);
    assert_ne!(world.snapshot.revision, next.snapshot.revision);
    assert!(world.graph.get("b.rs::b").is_some());
    assert!(next.graph.get("b.rs::b").is_none());
    assert!(next.graph.get("b.rs::b2").is_some());
    assert_eq!(
        world.find("b", 2048).unwrap().evidence[0].excerpt,
        "pub enum Mode { Fast, Slow(u32) }"
    );
    fs::remove_file(dir.path().join("b.rs")).unwrap();
    let deleted = next.advance(&["b.rs".into()]).unwrap();
    assert!(deleted.graph.get("b.rs::b2").is_none());
    assert_eq!(deleted.parses(), 3);
}

#[test]
fn simultaneous_queries_share_one_index_and_one_result() {
    let _env = env_lock();
    let dir = repository();
    let world = Arc::new(World::open(dir.path()).unwrap());
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let world = Arc::clone(&world);
            std::thread::spawn(move || world.find("depth", 2048).unwrap())
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(results.iter().all(|r| Arc::ptr_eq(r, &results[0])));
    assert_eq!(world.query_count(), 1);
    assert_eq!(results[0].revision, world.snapshot.revision.as_str());
}

#[test]
fn fields_variants_and_signatures_have_stable_ids() {
    let _env = env_lock();
    let dir = repository();
    let world = World::open(dir.path()).unwrap();
    assert!(world.graph.get("a.rs::Opts.depth").is_some());
    assert!(world.graph.get("b.rs::Mode.Fast").is_some());
    assert!(world.graph.get("b.rs::Mode.Slow.0").is_some());
    assert_eq!(world.signature("a.rs::a").unwrap(), "pub fn a()");
    assert!(
        world
            .graph
            .neighbors("b.rs::b")
            .iter()
            .any(|s| s.id == "a.rs::a")
    );
}
