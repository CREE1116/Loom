use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, Read, Seek, SeekFrom, Write},
    path::{Component, Path},
    process::{Command, Output},
    sync::atomic::{AtomicBool, Ordering},
};

/// A Git tree of tracked and non-ignored working files, including uncommitted changes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Revision(String);

impl TryFrom<String> for Revision {
    type Error = String;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        if matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            Ok(Self(value))
        } else {
            Err("invalid Git tree hash".into())
        }
    }
}
impl From<Revision> for String {
    fn from(value: Revision) -> Self {
        value.0
    }
}
impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Revision {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn capture(root: &Path) -> Result<Self> {
        Self::capture_with(root, |command| Ok(command.output()?))
    }

    pub fn capture_cancellable(root: &Path, cancel: &AtomicBool) -> Result<Self> {
        Self::capture_using(root, |command| super::cancellable_output(command, cancel), Some(cancel))
    }

    /// The surface may provide cancellable process execution without L0 depending on it.
    pub fn capture_with(
        root: &Path,
        execute: impl FnMut(&mut Command) -> Result<Output>,
    ) -> Result<Self> {
        Self::capture_using(root, execute, None)
    }

    fn capture_using(
        root: &Path,
        mut execute: impl FnMut(&mut Command) -> Result<Output>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Self> {
        check_cancel(cancel)?;
        // Never retry an interrupted/failed executor. Only cache/content errors
        // can be repaired by recapturing without the stat cache.
        let execution_failed = std::cell::Cell::new(false);
        let mut execute = |command: &mut Command| {
            let result = execute(command);
            if result.is_err() { execution_failed.set(true); }
            result
        };
        match Self::capture_inner(root, &mut execute, true, cancel) {
            Ok(revision) => Ok(revision),
            Err(error) if execution_failed.get() => Err(error),
            Err(_) => {
                // File-loop cancellation does not pass through the executor.
                check_cancel(cancel)?;
                Self::capture_inner(root, &mut execute, false, cancel)
            }
        }
    }

    fn capture_inner(
        root: &Path,
        execute: &mut impl FnMut(&mut Command) -> Result<Output>,
        use_cache: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<Self> {
        check_cancel(cancel)?;
        let root = dunce::canonicalize(&root).context("Opening revision workspace")?;
        let mut run = |command: &mut Command| -> Result<Vec<u8>> {
            check_cancel(cancel)?;
            let Output {
                status,
                stdout,
                stderr,
            } = execute(command).context("Running Git for revision")?;
            check_cancel(cancel)?;
            if !status.success() {
                bail!("Git revision: {}", String::from_utf8_lossy(&stderr));
            }
            Ok(stdout)
        };
        let git = || {
            let mut command = Command::new("git");
            command.current_dir(&root).arg("--literal-pathspecs");
            command
        };
        let top = run(git().args(["rev-parse", "--show-toplevel"]))?;
        if dunce::canonicalize(raw_path(top.strip_suffix(b"\n").unwrap_or(&top))?)? != root {
            bail!("revision workspace must be the Git repository root");
        }
        // Starting from an empty, private index also handles an unborn HEAD and deletions.
        let temporary = tempfile::tempdir()?;
        let index = temporary.path().join("index");
        run(git()
            .env("GIT_INDEX_FILE", &index)
            .args(["read-tree", "--empty"]))?;
        let tracked = run(git().args(["ls-files", "--stage", "-z"]))?;
        let mut names = BTreeSet::new();
        for entry in tracked.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            check_cancel(cancel)?;
            let (metadata, name) = split_entry(entry)?;
            if metadata.starts_with(b"160000 ") {
                bail!(
                    "submodule revisions are unsupported: {}",
                    raw_path(name)?.display()
                );
            }
            names.insert(name.to_vec());
        }
        let untracked = run(git().args(["ls-files", "--others", "--exclude-standard", "-z"]))?;
        for name in untracked.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            check_cancel(cancel)?;
            names.insert(name.to_vec());
        }
        // Buffered: quoted_path writes byte by byte, one syscall each when unbuffered.
        let mut paths = std::io::BufWriter::new(tempfile::tempfile()?);
        let mut entries = Vec::new();
        // Blob ids of files whose stat is unchanged since the last capture (like Git's index).
        let cache = if use_cache { stat_cache::load(&root, cancel)? } else { Default::default() };
        let mut known: Vec<Option<String>> = Vec::new();
        let mut stats: Vec<Option<stat_cache::Stat>> = Vec::new();
        for name in names {
            check_cancel(cancel)?;
            if is_internal(&name) {
                continue;
            }
            let absolute = root.join(raw_path(&name)?);
            let metadata = match fs::symlink_metadata(&absolute) {
                Ok(metadata) => metadata,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("Reading revision file {}", absolute.display()));
                }
            };
            let (mode, input) = if metadata.is_symlink() {
                let target = fs::read_link(&absolute)?;
                let input = temporary.path().join(format!("symlink-{}", entries.len()));
                fs::write(&input, path_bytes(&target)?)?;
                ("120000", input)
            } else if metadata.is_file() {
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode() & 0o111 != 0
                };
                #[cfg(not(unix))]
                let executable = false;
                (if executable { "100755" } else { "100644" }, absolute)
            } else {
                bail!("unsupported revision file type: {}", absolute.display());
            };
            let stat = (mode != "120000").then(|| stat_cache::Stat::of(&metadata, mode));
            let reused = stat
                .as_ref()
                .and_then(|stat| cache.reuse(&name, stat))
                .map(str::to_owned);
            if reused.is_none() {
                quoted_path(&mut paths, path_bytes(&input)?)?;
            }
            known.push(reused);
            stats.push(stat);
            entries.push((mode, name));
        }
        if !entries.is_empty() {
            let mut missing = 0;
            for hash in &known {
                check_cancel(cancel)?;
                missing += usize::from(hash.is_none());
            }
            let computed = if missing > 0 {
                let mut paths = paths.into_inner().map_err(|error| error.into_error())?;
                paths.seek(SeekFrom::Start(0))?;
                run(git()
                    .args(["hash-object", "-w", "--no-filters", "--stdin-paths"])
                    .stdin(paths))?
            } else {
                Vec::new()
            };
            let mut computed = std::str::from_utf8(&computed)?.lines();
            let mut hashes = Vec::with_capacity(known.len());
            for known in known {
                check_cancel(cancel)?;
                hashes.push(known.or_else(|| computed.next().map(str::to_owned))
                    .context("Git returned too few revision blobs")?);
            }
            if computed.next().is_some() {
                bail!("Git returned an incorrect number of revision blobs");
            }
            stat_cache::store(&root, entries.iter().zip(&stats).zip(&hashes).filter_map(
                |(((_, name), stat), hash)| Some((name.as_slice(), stat.clone()?, hash.as_str())),
            ), cancel)?;
            let mut input = std::io::BufWriter::new(tempfile::tempfile()?);
            for ((mode, name), hash) in entries.iter().zip(&hashes) {
                check_cancel(cancel)?;
                Self::try_from(hash.to_owned()).map_err(anyhow::Error::msg)?;
                write!(input, "{mode} {hash}\t")?;
                input.write_all(name)?;
                input.write_all(b"\0")?;
            }
            let mut input = input.into_inner().map_err(|error| error.into_error())?;
            input.seek(SeekFrom::Start(0))?;
            run(git()
                .env("GIT_INDEX_FILE", &index)
                .args(["update-index", "-z", "--index-info"])
                .stdin(input))?;
        }
        let tree = run(git().env("GIT_INDEX_FILE", &index).arg("write-tree"))?;
        let revision = Self::try_from(std::str::from_utf8(&tree)?.trim().to_owned())
            .map_err(anyhow::Error::msg)?;
        // Keep the tree reachable for later overlays without changing HEAD or the real index.
        run(git().args([
            "update-ref",
            &format!("refs/loom/revisions/{revision}"),
            revision.as_str(),
        ]))?;
        check_cancel(cancel)?;
        Ok(revision)
    }

    /// Restore raw blobs, replacing captured paths and preserving all unseen files.
    /// The caller owns stale-file cleanup and `read-tree`; indexes and HEADs stay untouched.
    pub fn materialize(&self, root: &Path) -> Result<()> {
        self.materialize_inner(root, None)
    }

    pub fn materialize_cancellable(&self, root: &Path, cancel: &AtomicBool) -> Result<()> {
        self.materialize_inner(root, Some(cancel))
    }

    fn materialize_inner(&self, root: &Path, cancel: Option<&AtomicBool>) -> Result<()> {
        check_cancel(cancel)?;
        let root = dunce::canonicalize(root).context("Opening materialization worktree")?;
        let run = |command: &mut Command| -> Result<Vec<u8>> {
            check_cancel(cancel)?;
            let output = match cancel {
                Some(cancel) => super::cancellable_output(command, cancel),
                None => Ok(command.output()?),
            }.context("Running Git for materialization")?;
            check_cancel(cancel)?;
            if !output.status.success() {
                bail!(
                    "Git materialization: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(output.stdout)
        };
        let git = || {
            let mut command = Command::new("git");
            command.current_dir(&root).arg("--literal-pathspecs");
            command
        };
        if fs::symlink_metadata(root.join(".git")).is_err() {
            bail!("materialization requires an existing Git worktree");
        }
        let top = run(git().args(["rev-parse", "--show-toplevel"]))?;
        if dunce::canonicalize(raw_path(top.strip_suffix(b"\n").unwrap_or(&top))?)? != root {
            bail!("materialization must target the Git worktree root");
        }
        let tree = run(git().args(["ls-tree", "-r", "-z", self.as_str()]))?;
        let mut entries = Vec::new();
        let mut input = std::io::BufWriter::new(tempfile::tempfile()?);
        for entry in tree.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            check_cancel(cancel)?;
            let (metadata, name) = split_entry(entry)?;
            let words: Vec<_> = std::str::from_utf8(metadata)?.split_whitespace().collect();
            if words.first() == Some(&"160000") {
                bail!(
                    "submodule materialization is unsupported: {}",
                    raw_path(name)?.display()
                );
            }
            if words.len() != 3
                || words[1] != "blob"
                || !matches!(words[0], "100644" | "100755" | "120000")
            {
                bail!("unsupported revision tree entry");
            }
            let path = raw_path(name)?;
            if path.as_os_str().is_empty()
                || is_internal(name)
                || path.components().any(|c| {
                    !matches!(c, Component::Normal(_)) || c.as_os_str().eq_ignore_ascii_case(".git")
                })
            {
                bail!("unsafe revision tree path: {}", path.display());
            }
            let mut parent = root.clone();
            for component in path.parent().into_iter().flat_map(Path::components) {
                check_cancel(cancel)?;
                parent.push(component);
                match fs::symlink_metadata(&parent) {
                    Ok(metadata) if !metadata.is_dir() || metadata.is_symlink() => {
                        bail!(
                            "materialization parent must be a directory, not a symlink: {}",
                            parent.display()
                        );
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                    Err(error) => return Err(error.into()),
                }
            }
            match fs::symlink_metadata(root.join(path)) {
                Ok(metadata) if !metadata.is_file() && !metadata.is_symlink() => {
                    bail!(
                        "materialization will not replace a directory or special file: {}",
                        path.display()
                    );
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            #[cfg(not(unix))]
            if words[0] == "120000" {
                bail!("symlink materialization is unsupported on this platform");
            }
            Self::try_from(words[2].to_owned()).map_err(anyhow::Error::msg)?;
            writeln!(input, "{}", words[2])?;
            entries.push((words[0], words[2], path));
        }
        if !entries.is_empty() {
            let mut input = input.into_inner().map_err(|error| error.into_error())?;
            input.seek(SeekFrom::Start(0))?;
            let mut blobs = tempfile::tempfile()?;
            let mut stderr = tempfile::tempfile()?;
            let mut command = git();
            command
                .args(["cat-file", "--batch"])
                .stdin(input)
                .stdout(blobs.try_clone()?)
                .stderr(stderr.try_clone()?);
            check_cancel(cancel)?;
            // Output capture overrides stdout; preserve this potentially huge spool.
            let status = match cancel {
                Some(cancel) => super::cancellable_status(&mut command, cancel),
                None => Ok(command.status()?),
            }.context("Reading Git materialization blobs")?;
            check_cancel(cancel)?;
            if !status.success() {
                stderr.seek(SeekFrom::Start(0))?;
                let mut message = Vec::new();
                stderr.take(64 * 1024).read_to_end(&mut message)?;
                bail!("Git materialization: {}", String::from_utf8_lossy(&message));
            }
            blobs.seek(SeekFrom::Start(0))?;
            let mut reader = std::io::BufReader::new(blobs);
            let staging = tempfile::Builder::new()
                .prefix(".loom-materialize-")
                .tempdir_in(&root)?;
            let staged = staging.path().join("blob");
            for (mode, hash, path) in entries {
                check_cancel(cancel)?;
                let mut header = String::new();
                reader.read_line(&mut header)?;
                let words: Vec<_> = header.split_whitespace().collect();
                if words.len() != 3 || words[0] != hash || words[1] != "blob" {
                    bail!("invalid materialization blob header");
                }
                let size: u64 = words[2].parse()?;
                let absolute = root.join(path);
                // Create and recheck one parent at a time, polling cancellation.
                let mut parent = root.clone();
                for component in path.parent().into_iter().flat_map(Path::components) {
                    check_cancel(cancel)?;
                    parent.push(component);
                    match fs::create_dir(&parent) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error.into()),
                    }
                    let metadata = fs::symlink_metadata(&parent)?;
                    if !metadata.is_dir() || metadata.is_symlink() {
                        bail!("materialization parent must be a directory, not a symlink: {}", parent.display());
                    }
                }
                if mode == "120000" {
                    #[cfg(unix)]
                    {
                        let mut target = Vec::new();
                        copy_blob(&mut reader, &mut target, size, cancel)
                            .context("Reading symlink blob")?;
                        check_cancel(cancel)?;
                        std::os::unix::fs::symlink(raw_path(&target)?, &staged)?;
                    }
                } else {
                    let mut file = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&staged)?;
                    copy_blob(&mut reader, &mut file, size, cancel)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        file.set_permissions(fs::Permissions::from_mode(if mode == "100755" {
                            0o755
                        } else {
                            0o644
                        }))?;
                    }
                }
                let mut newline = [0];
                reader.read_exact(&mut newline)?;
                if newline != *b"\n" {
                    bail!("invalid materialization blob terminator");
                }
                check_cancel(cancel)?;
                fs::rename(&staged, &absolute)?;
            }
        }
        check_cancel(cancel)
    }
}

fn check_cancel(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|cancel| cancel.load(Ordering::Acquire)) {
        bail!("revision operation cancelled");
    }
    Ok(())
}

fn copy_blob(
    reader: &mut impl Read,
    output: &mut impl Write,
    mut remaining: u64,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    let mut buffer = [0; 64 * 1024];
    while remaining > 0 {
        check_cancel(cancel)?;
        let chunk = remaining.min(buffer.len() as u64) as usize;
        let read = match reader.read(&mut buffer[..chunk]) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if read == 0 { bail!("truncated materialization blob"); }
        check_cancel(cancel)?;
        output.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    check_cancel(cancel)
}

fn split_entry(entry: &[u8]) -> Result<(&[u8], &[u8])> {
    let tab = entry
        .iter()
        .position(|b| *b == b'\t')
        .context("invalid Git entry")?;
    Ok((&entry[..tab], &entry[tab + 1..]))
}

fn raw_path(bytes: &[u8]) -> Result<&Path> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(Path::new(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        Ok(Path::new(std::str::from_utf8(bytes)?))
    }
}

fn path_bytes(path: &Path) -> Result<&[u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        Ok(path.to_str().context("non-UTF8 revision path")?.as_bytes())
    }
}

fn quoted_path(output: &mut impl Write, path: &[u8]) -> Result<()> {
    output.write_all(b"\"")?;
    for byte in path {
        match byte {
            b'"' | b'\\' => {
                output.write_all(b"\\")?;
                output.write_all(&[*byte])?;
            }
            32..=126 => output.write_all(&[*byte])?,
            _ => write!(output, "\\{byte:03o}")?,
        }
    }
    output.write_all(b"\"\n")?;
    Ok(())
}

/// Blob ids a workspace's last capture recorded, by file stat.
pub(crate) struct KnownBlobs(stat_cache::Cache);

impl KnownBlobs {
    pub(crate) fn load(root: &Path, cancel: Option<&AtomicBool>) -> Result<Self> {
        Ok(Self(stat_cache::load(root, cancel)?))
    }
    /// The blob id of `name` when its stat still matches the recorded one.
    pub(crate) fn blob(&self, name: &[u8], metadata: &fs::Metadata, mode: &str) -> Option<&str> {
        self.0.reuse(name, &stat_cache::Stat::of(metadata, mode))
    }
}

/// Record blob ids for files whose content is already proven, such as files cloned
/// from verified sources, so the next capture of `root` does not hash them again.
pub(crate) fn remember_blobs(
    root: &Path,
    entries: &[(Vec<u8>, fs::Metadata, &str, String)],
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    stat_cache::store(
        root,
        entries.iter().map(|(name, metadata, mode, hash)| {
            (name.as_slice(), stat_cache::Stat::of(metadata, mode), hash.as_str())
        }),
        cancel,
    )
}

/// File stat → blob id from the previous capture, so unchanged files are not re-hashed.
/// A file modified within two seconds of the cache write is "racily clean" (its mtime
/// cannot tell a later edit apart) and is hashed again, as Git does with its index.
mod stat_cache {
    use super::{AtomicBool, Result, check_cancel};
    use std::{collections::HashMap, fs, io::{BufReader, BufWriter, Read, Write}, path::Path, time::UNIX_EPOCH};

    // Poll between buffered JSON chunks as well as between file entries.
    struct CancellableIo<'a, T> {
        inner: T,
        cancel: Option<&'a AtomicBool>,
    }
    impl<T: Read> Read for CancellableIo<'_, T> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            check_cancel(self.cancel).map_err(std::io::Error::other)?;
            self.inner.read(buffer)
        }
    }
    impl<T: Write> Write for CancellableIo<'_, T> {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            check_cancel(self.cancel).map_err(std::io::Error::other)?;
            self.inner.write(buffer)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            check_cancel(self.cancel).map_err(std::io::Error::other)?;
            self.inner.flush()
        }
    }

    #[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    pub struct Stat {
        len: u64,
        mtime_ns: u128,
        ctime_ns: i128,
        inode: u64,
        mode: String,
    }
    impl Stat {
        pub fn of(metadata: &fs::Metadata, mode: &str) -> Self {
            #[cfg(unix)]
            let inode = std::os::unix::fs::MetadataExt::ino(metadata);
            #[cfg(not(unix))]
            let inode = 0;
            #[cfg(unix)]
            let ctime_ns = i128::from(std::os::unix::fs::MetadataExt::ctime(metadata)) * 1_000_000_000
                + i128::from(std::os::unix::fs::MetadataExt::ctime_nsec(metadata));
            #[cfg(not(unix))]
            let ctime_ns = 0;
            Self {
                len: metadata.len(),
                mtime_ns: metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos()),
                inode,
                ctime_ns,
                mode: mode.to_owned(),
            }
        }
    }

    #[derive(Default, serde::Serialize, serde::Deserialize)]
    pub struct Cache {
        written_ns: u128,
        entries: HashMap<String, (Stat, String)>,
    }
    impl Cache {
        pub fn reuse(&self, name: &[u8], stat: &Stat) -> Option<&str> {
            let (cached, hash) = self.entries.get(std::str::from_utf8(name).ok()?)?;
            let settled = stat.mtime_ns + 2_000_000_000 < self.written_ns;
            (cached == stat && settled && stat.mtime_ns > 0 && stat.ctime_ns > 0).then_some(hash.as_str())
        }
    }

    fn file(root: &Path) -> std::path::PathBuf {
        root.join(".loom/revision-stat-cache.json")
    }

    pub fn load(root: &Path, cancel: Option<&AtomicBool>) -> Result<Cache> {
        check_cancel(cancel)?;
        let cache = fs::File::open(file(root))
            .ok()
            .and_then(|inner| serde_json::from_reader(BufReader::new(CancellableIo { inner, cancel })).ok())
            .unwrap_or_default();
        check_cancel(cancel)?;
        Ok(cache)
    }

    pub fn store<'a>(root: &Path, entries: impl Iterator<Item = (&'a [u8], Stat, &'a str)>, cancel: Option<&AtomicBool>) -> Result<()> {
        let mut cached = HashMap::new();
        for (name, stat, hash) in entries {
            check_cancel(cancel)?;
            if let Ok(name) = std::str::from_utf8(name) {
                cached.insert(name.to_owned(), (stat, hash.to_owned()));
            }
        }
        let cache = Cache {
            written_ns: std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
            entries: cached,
        };
        let path = file(root);
        if let Some(dir) = path.parent() {
            // Cache I/O remains best effort; cancellation must still propagate.
            let stored = (|| -> Result<()> {
                check_cancel(cancel)?;
                fs::create_dir_all(dir)?;
                let temporary = tempfile::NamedTempFile::new_in(dir)?;
                let mut output = BufWriter::new(CancellableIo { inner: temporary.as_file(), cancel });
                serde_json::to_writer(&mut output, &cache)?;
                output.flush()?;
                drop(output);
                check_cancel(cancel)?;
                temporary.persist(&path)?;
                Ok(())
            })();
            if stored.is_err() {
                check_cancel(cancel)?;
            }
        }
        check_cancel(cancel)
    }
}

/// Loom's own state: `.loom`, `.custom-tui` and the per-version dirs older cores left
/// behind (`.loom-v3`, ...). Capturing them copied hundreds of MB into every overlay.
fn is_internal(path: &[u8]) -> bool {
    let top = path.split(|byte| *byte == b'/').next().unwrap_or_default();
    top == b".loom"
        || top == b".custom-tui"
        || top
            .strip_prefix(b".loom-v")
            .is_some_and(|version| !version.is_empty() && version.iter().all(u8::is_ascii_digit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loom_state_of_every_version_is_internal_but_lookalikes_are_not() {
        for internal in [".loom", ".loom/x", ".custom-tui/a", ".loom-v3", ".loom-v3/runs/a.json", ".loom-v12/x"] {
            assert!(is_internal(internal.as_bytes()), "{internal}");
        }
        for source in [".loomrc", ".loom-config/x", ".loom-v/x", ".loom-vx/x", "src/.loom-v3/x", "loom-v3/x"] {
            assert!(!is_internal(source.as_bytes()), "{source}");
        }
    }

    #[test]
    fn blob_copy_stops_after_the_chunk_that_cancels() {
        struct CancellingReader<'a> {
            bytes: std::io::Cursor<Vec<u8>>,
            cancel: &'a AtomicBool,
            reads: usize,
        }
        impl Read for CancellingReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                let read = self.bytes.read(buffer)?;
                self.cancel.store(true, Ordering::Release);
                Ok(read)
            }
        }
        let cancel = AtomicBool::new(false);
        let mut reader = CancellingReader {
            bytes: std::io::Cursor::new(vec![0xff; 4 * 64 * 1024]),
            cancel: &cancel,
            reads: 0,
        };
        let mut copied = Vec::new();
        let error = copy_blob(&mut reader, &mut copied, 4 * 64 * 1024, Some(&cancel)).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(reader.reads, 1);
        assert!(reader.bytes.position() <= 64 * 1024);
        assert!(reader.bytes.position() < reader.bytes.get_ref().len() as u64);
        assert!(copied.len() <= 64 * 1024);
    }

    #[test]
    fn blob_copy_retries_interrupted_reads_and_polls_before_retry() {
        struct InterruptedReader<'a> {
            cancel: &'a AtomicBool,
            cancel_on_interrupt: bool,
            reads: usize,
        }
        impl Read for InterruptedReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.reads == 1 {
                    self.cancel.store(self.cancel_on_interrupt, Ordering::Release);
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                buffer[0] = 0xff;
                Ok(1)
            }
        }
        for cancel_on_interrupt in [false, true] {
            let cancel = AtomicBool::new(false);
            let mut reader = InterruptedReader { cancel: &cancel, cancel_on_interrupt, reads: 0 };
            let mut copied = Vec::new();
            let result = copy_blob(&mut reader, &mut copied, 1, Some(&cancel));
            if cancel_on_interrupt {
                assert!(result.unwrap_err().to_string().contains("cancelled"));
                assert_eq!(reader.reads, 1);
                assert!(copied.is_empty());
            } else {
                result.unwrap();
                assert_eq!(reader.reads, 2);
                assert_eq!(copied, [0xff]);
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn capture_cancellation_after_listing_never_retries_or_visits_entries() {
        use std::os::unix::process::ExitStatusExt;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let cancel = AtomicBool::new(false);
        let mut calls = Vec::new();
        let error = Revision::capture_using(&root, |command| {
            let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
            let stdout = if args.iter().any(|arg| arg == "rev-parse") {
                let mut top = path_bytes(&root).unwrap().to_vec();
                top.push(b'\n');
                top
            } else if args.iter().any(|arg| arg == "ls-files") {
                // Encountering this entry would fail for an entirely different reason.
                cancel.store(true, Ordering::Release);
                b"160000 0000000000000000000000000000000000000000 0\tunsupported\0".to_vec()
            } else {
                assert!(args.iter().any(|arg| arg == "read-tree"));
                Vec::new()
            };
            calls.push(args);
            Ok(Output { status: std::process::ExitStatus::from_raw(0), stdout, stderr: Vec::new() })
        }, Some(&cancel)).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(calls.len(), 3, "cancelled capture must not retry or update the index");
    }
}
