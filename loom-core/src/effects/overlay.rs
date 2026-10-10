use super::{
    Output, Patch,
    editing::{find_matches, new_syntax_error},
};
use crate::{
    base::{FileArtifactStore, Phase, Revision, cancellable_output},
    world::{World, graph},
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    ops::Range,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// One shared modification time for every file of an immutable overlay base.
fn base_mtime() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + Duration::from_secs(1)
}

/// An isolated Git worktree of the actual working-tree revision, including dirty files.
pub struct Overlay {
    pub base: Revision,
    root: PathBuf,
    dir: PathBuf,
    _lease: Option<fs::File>,
    defer_cleanup: bool,
    artifacts: FileArtifactStore,
    reads: HashMap<PathBuf, Vec<u8>>,
}

impl Overlay {
    /// Cancel preparation between files, while waiting for leases, and during Git calls.
    pub fn create_cancellable(workspace: &Path, base: &Revision, cancel: &AtomicBool) -> Result<Self> {
        Self::create_inner(workspace, base, Some(cancel))
    }

    pub fn create(workspace: &Path, base: &Revision) -> Result<Self> {
        Self::create_inner(workspace, base, None)
    }

    fn create_inner(workspace: &Path, base: &Revision, cancel: Option<&AtomicBool>) -> Result<Self> {
        check_preparation(cancel)?;
        let root = dunce::canonicalize(workspace)?;
        let artifacts = FileArtifactStore::open(&root)?;
        // Allocation and sweeping share one registry lock. Otherwise a sweeper
        // could remove a revision while a new worker is creating its first slot.
        let registry = fs::OpenOptions::new().create(true).truncate(false)
            .read(true).write(true).open(root.join(".loom/overlays.lock"))?;
        if cancel.is_some() {
            loop {
                check_preparation(cancel)?;
                match registry.try_lock_exclusive() {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock =>
                        std::thread::sleep(Duration::from_millis(10)),
                    Err(error) => return Err(error.into()),
                }
            }
        } else {
            registry.lock_exclusive()?;
        }
        sweep_abandoned(&root, base, cancel)?;
        let overlays = root.join(".loom/overlays").join(base.as_str());
        fs::create_dir_all(&overlays)?;
        // Stable slots make Cargo dep-info reusable across sequential overlays. A lock
        // gives concurrent workers independent slots and is released after a crash.
        let (dir, lease) = (0..)
            .find_map(|slot| {
                if let Err(error) = check_preparation(cancel) {
                    return Some(Err(error));
                }
                let lease = fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(overlays.join(format!("{slot}.lock")));
                let lease = match lease {
                    Ok(lease) => lease,
                    Err(e) => return Some(Err(e.into())),
                };
                // A just-released lease can still be held for a moment by a child process
                // forked elsewhere before its exec closes inherited descriptors. Waiting
                // briefly keeps sequential workers on their slot and its warm caches.
                for attempt in 0..4 {
                    match lease.try_lock_exclusive() {
                        Ok(()) => return Some(Ok((overlays.join(format!("{slot}")), lease))),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if attempt < 3 {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                        }
                        Err(e) => return Some(Err(e.into())),
                    }
                }
                None
            })
            .context("overlay slot unavailable")??;
        drop(registry);
        // Recover a slot left by process termination, without touching another lease.
        let _ = preparation_output(Command::new("git")
            .current_dir(&root)
            .args(["worktree", "remove", "--force"])
            .arg(&dir), cancel);
        check_preparation(cancel)?;
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        let commit = preparation_output(Command::new("git")
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", "Loom")
            .env("GIT_AUTHOR_EMAIL", "loom@localhost")
            .env("GIT_COMMITTER_NAME", "Loom")
            .env("GIT_COMMITTER_EMAIL", "loom@localhost")
            .args(["commit-tree", base.as_str(), "-m", "Loom isolated overlay"]), cancel)?;
        if !commit.status.success() {
            bail!(
                "Creating overlay commit: {}",
                String::from_utf8_lossy(&commit.stderr)
            );
        }
        let commit = std::str::from_utf8(&commit.stdout)?.trim();
        let mut overlay = Self {
            base: base.clone(),
            root,
            dir,
            _lease: Some(lease),
            // An interrupted constructor must not wait for synchronous Drop cleanup.
            defer_cleanup: cancel.is_some(),
            artifacts,
            reads: HashMap::new(),
        };
        let output = preparation_output(Command::new("git")
            .current_dir(&overlay.root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
            ])
            .arg(&overlay.dir)
            .arg(commit), cancel)?;
        if !output.status.success() {
            bail!(
                "Creating overlay worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let index = preparation_output(Command::new("git")
            .current_dir(&overlay.dir)
            .args(["read-tree", base.as_str()]), cancel)?;
        if !index.status.success() {
            bail!("initializing isolated overlay index failed");
        }
        let cloned = overlay.clone_base(cancel)?;
        if !cloned {
            check_preparation(cancel)?;
            match cancel {
                Some(cancel) => base.materialize_cancellable(&overlay.dir, cancel)?,
                None => base.materialize(&overlay.dir)?,
            }
            check_preparation(cancel)?;
        }
        // A checkout assigns new mtimes even when content did not change. Use one
        // nonzero timestamp for this immutable base, allowing stat-cache reuse;
        // revision-specific paths prevent stale builds from crossing revisions.
        // Edits retain real current mtimes. Clones already received it.
        let names = if cloned { None } else { Some(preparation_output(Command::new("git")
            .current_dir(&overlay.dir)
            .args(["ls-files", "-z"]), cancel)?) };
        if names.as_ref().is_some_and(|names| !names.status.success()) {
            bail!("reading overlay file list failed");
        }
        for name in names.iter().flat_map(|names| names.stdout.split(|b| *b == 0)).filter(|p| !p.is_empty()) {
            check_preparation(cancel)?;
            let path = overlay.dir.join(std::str::from_utf8(name)?);
            let meta = fs::symlink_metadata(&path)?;
            if meta.is_file() {
                fs::File::open(path)?
                    .set_times(fs::FileTimes::new().set_modified(base_mtime()))?;
            }
        }
        check_preparation(cancel)?;
        overlay.defer_cleanup = false;
        Ok(overlay)
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn defer_cleanup(&mut self) {
        self.defer_cleanup = true;
    }
    /// APFS clones avoid copying file data. Verify the finished clone against the
    /// pinned Git tree: concurrent/user edits fall back to immutable Git blobs.
    #[cfg(target_os = "macos")]
    fn clone_base(&self, cancel: Option<&AtomicBool>) -> Result<bool> {
        use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
        let attempt = (|| -> Result<()> {
            let listing = preparation_output(Command::new("git").current_dir(&self.root)
                .args(["ls-tree", "-r", "-z", self.base.as_str()]), cancel)?;
            anyhow::ensure!(listing.status.success(), "listing clone base failed");
            // A clone whose source kept the stat the workspace capture hashed is that blob.
            // Recording it spares re-hashing every file of the new overlay (seconds per turn).
            let known = crate::base::KnownBlobs::load(&self.root, cancel)?;
            // Parse and check every entry first; directories are created once, serially.
            struct Entry<'a> {
                mode: &'a str,
                blob: &'a str,
                name: &'a [u8],
                path: &'a Path,
            }
            let mut entries = Vec::new();
            let mut parents = BTreeSet::new();
            for entry in listing.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
                check_preparation(cancel)?;
                let tab = entry.iter().position(|b| *b == b'\t').context("invalid tree entry")?;
                let mut words = std::str::from_utf8(&entry[..tab])?.split_whitespace();
                let mode = words.next().context("missing file mode")?;
                let blob = words.nth(1).context("missing blob id")?;
                let path = Path::new(std::ffi::OsStr::from_bytes(&entry[tab + 1..]));
                anyhow::ensure!(!path.as_os_str().is_empty() && path.components().all(|p| matches!(p, Component::Normal(_))), "invalid clone path");
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    parents.insert(parent);
                }
                entries.push(Entry { mode, blob, name: &entry[tab + 1..], path });
            }
            // Every source ancestor must be a real directory (no symlinked escape); each is
            // checked once (BTreeSet order visits parents before children).
            let mut checked = BTreeSet::new();
            for parent in &parents {
                let mut dir = PathBuf::new();
                for part in parent.components() {
                    dir.push(part);
                    if checked.insert(dir.clone()) {
                        check_preparation(cancel)?;
                        anyhow::ensure!(fs::symlink_metadata(self.root.join(&dir))?.is_dir(), "source parent is not a directory");
                    }
                }
                fs::create_dir_all(self.dir.join(parent))?;
            }
            // Cloning is a handful of syscalls per file; spread the files over a few threads.
            let proven = std::sync::Mutex::new(Vec::new());
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 8);
            let per = entries.len().div_ceil(threads).max(1);
            std::thread::scope(|scope| -> Result<()> {
                let workers: Vec<_> = entries
                    .chunks(per)
                    .map(|chunk| {
                        let (proven, known) = (&proven, &known);
                        scope.spawn(move || -> Result<()> {
                            for e in chunk {
                                check_preparation(cancel)?;
                                let source = self.root.join(e.path);
                                let dest = self.dir.join(e.path);
                                if e.mode == "120000" {
                                    std::os::unix::fs::symlink(fs::read_link(&source)?, &dest)?;
                                    continue;
                                }
                                let before = fs::symlink_metadata(&source)?;
                                anyhow::ensure!(matches!(e.mode, "100644" | "100755") && before.is_file(), "source is not a regular file");
                                let source_c = std::ffi::CString::new(source.as_os_str().as_bytes())?;
                                let dest_c = std::ffi::CString::new(dest.as_os_str().as_bytes())?;
                                if unsafe { libc::clonefile(source_c.as_ptr(), dest_c.as_ptr(), 0) } != 0 {
                                    return Err(std::io::Error::last_os_error().into());
                                }
                                fs::set_permissions(&dest, fs::Permissions::from_mode(if e.mode == "100755" { 0o755 } else { 0o644 }))?;
                                // The shared base timestamp (see create_inner), set before the stat is recorded.
                                fs::File::open(&dest)?.set_times(fs::FileTimes::new().set_modified(base_mtime()))?;
                                let unchanged = fs::symlink_metadata(&source).is_ok_and(|after| {
                                    known.blob(e.name, &after, e.mode) == Some(e.blob)
                                        && known.blob(e.name, &before, e.mode) == Some(e.blob)
                                });
                                if unchanged {
                                    let stat = fs::symlink_metadata(&dest)?;
                                    proven.lock().unwrap().push((e.name.to_vec(), stat, e.mode, e.blob.to_owned()));
                                }
                            }
                            Ok(())
                        })
                    })
                    .collect();
                for worker in workers {
                    worker.join().map_err(|_| anyhow::anyhow!("overlay clone worker panicked"))??;
                }
                Ok(())
            })?;
            let mut proven = proven.into_inner().unwrap();
            proven.sort_by(|a, b| a.0.cmp(&b.0));
            crate::base::remember_blobs(&self.dir, &proven, cancel)?;
            let captured = match cancel {
                Some(cancel) => Revision::capture_cancellable(&self.dir, cancel)?,
                None => Revision::capture(&self.dir)?,
            };
            anyhow::ensure!(captured == self.base, "source changed since the pinned revision");
            Ok(())
        })();
        // Cancellation is terminal, never a reason to retry with materialization.
        check_preparation(cancel)?;
        if attempt.is_ok() { return Ok(true); }
        // This worktree belongs exclusively to this lease. Clear partial clones
        // before immutable materialization, retaining only Git's control file.
        for entry in fs::read_dir(&self.dir)? {
            check_preparation(cancel)?;
            let entry = entry?;
            if entry.file_name() == ".git" { continue; }
            if entry.file_type()?.is_dir() { fs::remove_dir_all(entry.path())?; }
            else { fs::remove_file(entry.path())?; }
        }
        Ok(false)
    }
    #[cfg(not(target_os = "macos"))]
    fn clone_base(&self, cancel: Option<&AtomicBool>) -> Result<bool> {
        check_preparation(cancel)?;
        Ok(false)
    }
    pub(crate) fn workspace(&self) -> &Path {
        &self.root
    }
    pub fn artifacts(&self) -> &FileArtifactStore {
        &self.artifacts
    }
    /// Runner passes this explicitly to Cargo; it is never a writable symlink in the overlay.
    pub fn target_dir(&self) -> PathBuf {
        self.root.join(".loom/target")
    }

    pub fn resolve(&self, path: &Path) -> Result<PathBuf> {
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("overlay path must be relative, without traversal");
        }
        if path.components().any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some(".git" | ".loom" | ".custom-tui")
            )
        }) {
            bail!("overlay metadata is not a file effect");
        }
        let absolute = self.dir.join(path);
        let mut ancestor = absolute.as_path();
        loop {
            match fs::symlink_metadata(ancestor) {
                Ok(_) => {
                    if !dunce::canonicalize(ancestor)?.starts_with(&self.dir) {
                        bail!("path escapes overlay through a symlink");
                    }
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    ancestor = ancestor.parent().context("overlay parent missing")?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(absolute)
    }
    /// Files read so far in this overlay (workspace-relative).
    pub fn read_paths(&self) -> std::collections::BTreeSet<PathBuf> {
        self.reads.keys().cloned().collect()
    }
    pub fn read(&mut self, path: &Path, lines: Option<Range<usize>>) -> Result<Output> {
        let absolute = self.resolve(path)?;
        let bytes = fs::read(&absolute)?;
        let text = std::str::from_utf8(&bytes)?;
        let all: Vec<_> = text.lines().collect();
        let structured = if lines.is_none() && all.len() > 400 {
            let symbols = graph::extract(&path.to_string_lossy(), text).unwrap_or_default();
            json!({"path": path, "lines": all.len(), "outline": symbols.iter().map(|s| json!({"id":s.id, "start":s.start, "end":s.end})).collect::<Vec<_>>()})
        } else {
            let range = lines.unwrap_or(0..all.len());
            if range.start > range.end || range.end > all.len() || range.len() > 400 {
                bail!("read range must fit the file and at most 400 lines (zero based)");
            }
            json!({"path":path, "start":range.start, "end":range.end, "text":all[range].join("\n"), "digest": format!("{:x}", Sha256::digest(&bytes))})
        };
        let output = Output::record(
            &self.artifacts,
            Phase::Orient,
            format!("Read {} ({} lines)", path.display(), all.len()),
            structured,
            &bytes,
        )?;
        self.reads.insert(absolute, bytes);
        Ok(output)
    }
    pub fn edit(&mut self, path: &Path, old: &str, new: &str) -> Result<Output> {
        self.edit_all(path, old, new, false)
    }
    pub fn edit_all(
        &mut self,
        path: &Path,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<Output> {
        let absolute = self.resolve(path)?;
        let bytes = fs::read(&absolute)?;
        if self.reads.get(&absolute) != Some(&bytes) {
            bail!("read the current file before editing");
        }
        let before = std::str::from_utf8(&bytes)?;
        if old.is_empty() {
            bail!("old must not be empty");
        }
        let (ranges, matched) = find_matches(before, old).context("old text was not found")?;
        if ranges.len() != 1 && !replace_all {
            bail!("old text matches multiple locations; add context");
        }
        let mut after = String::new();
        let mut last = 0;
        for (start, end) in &ranges {
            after.push_str(&before[last..*start]);
            after.push_str(new);
            last = *end;
        }
        after.push_str(&before[last..]);
        if let Some(problem) = new_syntax_error(&path.to_string_lossy(), before, &after) {
            bail!("edit would break Rust syntax: {problem}");
        }
        let output = Output::record(
            &self.artifacts,
            Phase::Edit,
            format!("Edited {}", path.display()),
            json!({"path": path, "matched":matched}),
            after.as_bytes(),
        )?;
        fs::write(&absolute, &after)?;
        self.reads.insert(absolute, after.into_bytes());
        Ok(output)
    }
    pub fn write(&mut self, path: &Path, content: &str) -> Result<Output> {
        let absolute = self.resolve(path)?;
        if fs::symlink_metadata(&absolute).is_ok() {
            bail!("file already exists; read and edit it");
        }
        if path.extension().is_some_and(|e| e == "rs") {
            syn::parse_file(content)?;
        }
        let output = Output::record(
            &self.artifacts,
            Phase::Edit,
            format!("Created {}", path.display()),
            json!({"path": path, "bytes": content.len()}),
            content.as_bytes(),
        )?;
        fs::create_dir_all(absolute.parent().context("file parent missing")?)?;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&absolute)?;
        use std::io::Write;
        file.write_all(content.as_bytes())?;
        self.reads.insert(absolute, content.as_bytes().to_vec());
        Ok(output)
    }
    /// Name-based mechanical rename is deliberately restricted to a unique graph item.
    pub fn rename(&mut self, symbol: &str, new_name: &str, world: &World) -> Result<Output> {
        if world.snapshot.revision != crate::base::Revision::capture(&self.dir)? {
            bail!("rename world does not match the current overlay revision");
        }
        let target = world.graph.get(symbol).context("unknown rename symbol")?;
        if world.graph.named(&target.name).count() != 1
            || matches!(
                target.kind,
                graph::SymbolKind::Field | graph::SymbolKind::Variant
            )
        {
            bail!("rename requires an unambiguous item name");
        }
        syn::parse_str::<syn::Ident>(new_name).context("invalid Rust identifier")?;
        let mut files = BTreeSet::from([PathBuf::from(&target.file)]);
        files.extend(
            world
                .graph
                .referrers(symbol)
                .iter()
                .map(|s| PathBuf::from(&s.file)),
        );
        let mut proposals = Vec::new();
        for path in &files {
            let before = fs::read_to_string(self.resolve(path)?)?;
            let after = super::editing::rename_identifiers(&before, &target.name, new_name)?;
            syn::parse_file(&after)?;
            proposals.push((path.clone(), before, after));
        }
        for (path, before, after) in proposals {
            self.read(&path, Some(0..0))?;
            self.edit(&path, &before, &after)?;
        }
        let raw = serde_json::to_vec(&files)?;
        Output::record(
            &self.artifacts,
            Phase::Edit,
            format!("Renamed {symbol} to {new_name}"),
            json!({"files":files}),
            &raw,
        )
    }
    pub fn patch(&self) -> Result<Patch> {
        Patch::between(&self.dir, &self.base, &Revision::capture(&self.dir)?)
    }
    pub fn touched(&self) -> Result<BTreeSet<PathBuf>> {
        Ok(self.patch()?.files.into_iter().map(|f| f.path).collect())
    }
}

fn check_preparation(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
        bail!("overlay preparation cancelled");
    }
    Ok(())
}

fn preparation_output(command: &mut Command, cancel: Option<&AtomicBool>) -> Result<std::process::Output> {
    match cancel {
        Some(cancel) => cancellable_output(command, cancel),
        None => Ok(command.output()?),
    }
}

/// Remove overlays of other revisions that no live worker leases. A process that ended
/// without dropping its overlay (crash, closed window, kill) left a full worktree behind,
/// and slots are only reused for the same revision, so they piled up (gigabytes).
fn sweep_abandoned(root: &Path, keep: &Revision, cancel: Option<&AtomicBool>) -> Result<()> {
    let Ok(revisions) = fs::read_dir(root.join(".loom/overlays")) else { return Ok(()) };
    let mut removed = false;
    for revision in revisions.flatten() {
        check_preparation(cancel)?;
        let dir = revision.path();
        if !dir.is_dir() || revision.file_name() == keep.as_str() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let locks: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "lock"))
            .collect();
        // Hold every slot lock while removing, so no worker can start in this revision.
        let mut held = Vec::new();
        let free = locks.iter().all(|lock| {
            if check_preparation(cancel).is_err() { return false; }
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock)
                .ok()
                .filter(|file| file.try_lock_exclusive().is_ok())
                .map(|file| held.push(file))
                .is_some()
        });
        check_preparation(cancel)?;
        if !free {
            continue;
        }
        for lock in &locks {
            check_preparation(cancel)?;
            let slot = lock.with_extension("");
            let _ = preparation_output(Command::new("git")
                .current_dir(root)
                .args(["worktree", "remove", "--force"])
                .arg(&slot), cancel);
        }
        check_preparation(cancel)?;
        removed |= fs::remove_dir_all(&dir).is_ok();
        drop(held);
    }
    if removed {
        let _ = preparation_output(Command::new("git").current_dir(root).args(["worktree", "prune"]), cancel);
    }
    check_preparation(cancel)
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // Removing a worktree of a large repository takes most of a second; the turn does
        // not wait for it. The slot stays leased until removal finishes, and a slot left by
        // an exit mid-removal is recovered by the next `create`.
        if std::env::var_os("LOOM_OVERLAY_SYNC_CLEANUP").is_some() {
            let _ = Command::new("git")
                .current_dir(&self.root)
                .args(["worktree", "remove", "--force"])
                .arg(&self.dir)
                .output();
            return;
        }
        let root = self.root.clone();
        let dir = self.dir.clone();
        let lease = self._lease.take();
        // Cancellation should acknowledge immediately. Keep this slot leased until
        // cleanup finishes so a following worker cannot race the detached remover.
        let _ = std::thread::Builder::new()
            .name("loom-overlay-cleanup".into())
            .spawn(move || {
                let _lease = lease;
                let _ = Command::new("git")
                    .current_dir(&root)
                    .args(["worktree", "remove", "--force"])
                    .arg(&dir)
                    .output();
            });
    }
}
