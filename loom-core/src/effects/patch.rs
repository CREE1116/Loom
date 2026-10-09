use crate::base::Revision;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilePatch {
    pub path: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Patch {
    pub base: Revision,
    pub files: Vec<FilePatch>,
    pub diff: Vec<u8>,
}

impl Patch {
    /// Apply a moved-base proposal only in an isolated worktree. Conflicts never
    /// reach the user's workspace; Core validates the merged worktree again.
    pub fn merge_into(&self, overlay: &super::Overlay) -> Result<()> {
        if self.diff.is_empty() {
            return Ok(());
        }
        // Git's index-based three-way apply runs clean filters on merged blobs.
        // Merge the exact working bytes instead, with all conflicts kept private.
        let proposal = super::Overlay::create(overlay.workspace(), &self.base)?;
        let mut bases = std::collections::BTreeMap::new();
        for file in &self.files {
            bases.insert(
                file.path.clone(),
                FileState::read(&proposal.dir().join(&file.path))?,
            );
        }
        self.apply(proposal.dir())?;
        let mut merged = Vec::new();
        for file in &self.files {
            let path = overlay.resolve(&file.path)?;
            let base = bases.remove(&file.path).context("missing merge base")?;
            let ours = FileState::read(&path)?;
            let theirs = FileState::read(&proposal.dir().join(&file.path))?;
            let next = if ours == base {
                theirs
            } else if theirs == base || ours == theirs {
                ours
            } else {
                let (Some(base), Some(ours), Some(theirs)) = (base, ours, theirs) else {
                    bail!("three-way add/delete conflict: {}", file.path.display());
                };
                if [base.mode, ours.mode, theirs.mode].contains(&0o120000)
                    || [&base.bytes, &ours.bytes, &theirs.bytes]
                        .iter()
                        .any(|b| b.contains(&0))
                {
                    bail!(
                        "three-way binary or symlink conflict: {}",
                        file.path.display()
                    );
                }
                let scratch = tempfile::tempdir()?;
                let a = scratch.path().join("ours");
                let b = scratch.path().join("base");
                let c = scratch.path().join("theirs");
                fs::write(&a, &ours.bytes)?;
                fs::write(&b, &base.bytes)?;
                fs::write(&c, &theirs.bytes)?;
                let result = Command::new("git")
                    .args(["merge-file", "--stdout"])
                    .arg(a)
                    .arg(b)
                    .arg(c)
                    .output()?;
                if !result.status.success() {
                    bail!("three-way content conflict: {}", file.path.display());
                }
                let mode = if ours.mode == base.mode {
                    theirs.mode
                } else if theirs.mode == base.mode || ours.mode == theirs.mode {
                    ours.mode
                } else {
                    bail!("three-way mode conflict: {}", file.path.display());
                };
                Some(FileState {
                    mode,
                    bytes: result.stdout,
                })
            };
            merged.push((path, next));
        }
        for (path, state) in merged {
            FileState::replace(&path, state)?;
        }
        Ok(())
    }
    pub fn between(root: &Path, base: &Revision, tree: &Revision) -> Result<Self> {
        let names = Command::new("git")
            .current_dir(root)
            .args([
                "diff",
                "--no-renames",
                "--name-only",
                "-z",
                base.as_str(),
                tree.as_str(),
            ])
            .output()?;
        if !names.status.success() {
            bail!("patch file listing failed");
        }
        let files = names
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| {
                Ok(FilePatch {
                    path: PathBuf::from(std::str::from_utf8(p)?),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let diff = Command::new("git")
            .current_dir(root)
            .args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--binary",
                "--full-index",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "--no-color",
                base.as_str(),
                tree.as_str(),
            ])
            .output()?;
        if !diff.status.success() {
            bail!("patch generation failed");
        }
        Ok(Self {
            base: base.clone(),
            files,
            diff: diff.stdout,
        })
    }
    /// Only Core calls this after validation. A moved base is a conflict, never overwrite.
    /// L6 owns three-way reconciliation; this primitive applies an exact-base patch.
    pub fn apply(&self, workspace: &Path) -> Result<()> {
        if Revision::capture(workspace)? != self.base {
            bail!("patch base moved; preserve user changes and reconcile in Core");
        }
        if self.diff.is_empty() {
            return Ok(());
        }
        // Apply outside any Git repository so attribute clean/smudge transforms
        // cannot rewrite raw working bytes. Only declared, validated paths return.
        let scratch = tempfile::tempdir()?;
        for file in &self.files {
            let source = checked_path(workspace, &file.path)?;
            if let Some(state) = FileState::read(&source)? {
                let copy = scratch.path().join(&file.path);
                fs::create_dir_all(copy.parent().context("patch parent missing")?)?;
                state.write(&copy)?;
            }
        }
        let apply = |check: bool| -> Result<()> {
            let mut input = tempfile::tempfile()?;
            input.write_all(&self.diff)?;
            input.seek(SeekFrom::Start(0))?;
            let mut command = Command::new("git");
            command
                .current_dir(scratch.path())
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env("GIT_ATTR_NOSYSTEM", "1")
                .args([
                    "-c",
                    "core.attributesFile=/dev/null",
                    "-c",
                    "core.autocrlf=false",
                    "apply",
                    "--no-index",
                    "--binary",
                    "--whitespace=nowarn",
                ]);
            if check {
                command.arg("--check");
            }
            let result = command
                .stdin(input)
                .output()
                .context("Applying raw patch")?;
            if !result.status.success() {
                bail!(
                    "patch conflict: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
            Ok(())
        };
        apply(true)?;
        apply(false)?;
        let next = self
            .files
            .iter()
            .map(|file| {
                Ok((
                    checked_path(workspace, &file.path)?,
                    FileState::read(&scratch.path().join(&file.path))?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        if Revision::capture(workspace)? != self.base {
            bail!("workspace changed during patch validation");
        }
        for (path, state) in next {
            FileState::replace(&path, state)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileState {
    mode: u32,
    bytes: Vec<u8>,
}
impl FileState {
    fn read(path: &Path) -> Result<Option<Self>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if metadata.is_symlink() {
            let target = fs::read_link(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                return Ok(Some(Self {
                    mode: 0o120000,
                    bytes: target.as_os_str().as_bytes().to_vec(),
                }));
            }
            #[cfg(not(unix))]
            {
                return Ok(Some(Self {
                    mode: 0o120000,
                    bytes: target
                        .to_str()
                        .context("non-UTF8 symlink")?
                        .as_bytes()
                        .to_vec(),
                }));
            }
        }
        if !metadata.is_file() {
            bail!("merge path is not a file: {}", path.display());
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        Ok(Some(Self {
            mode: if executable { 0o100755 } else { 0o100644 },
            bytes: fs::read(path)?,
        }))
    }
    fn replace(path: &Path, state: Option<Self>) -> Result<()> {
        // Git records only the executable bit. Keep the destination's existing
        // access permissions when applying a content change to a regular file.
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            fs::symlink_metadata(path)
                .ok()
                .filter(|m| m.is_file())
                .and_then(|metadata| {
                    let next = state.as_ref().filter(|s| s.mode != 0o120000)?;
                    let mut mode = metadata.permissions().mode() & 0o777;
                    if next.mode & 0o111 == 0 {
                        mode &= !0o111;
                    } else if mode & 0o111 == 0 {
                        mode |= (mode & 0o444) >> 2;
                    }
                    Some(mode)
                })
        };
        #[cfg(not(unix))]
        let mode = None;
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if let Some(state) = state {
            fs::create_dir_all(path.parent().context("patch parent missing")?)?;
            state.write_with_mode(path, mode)?;
        }
        Ok(())
    }
    fn write(self, path: &Path) -> Result<()> {
        self.write_with_mode(path, None)
    }
    fn write_with_mode(self, path: &Path, permissions: Option<u32>) -> Result<()> {
        if self.mode == 0o120000 {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                std::os::unix::fs::symlink(
                    Path::new(std::ffi::OsStr::from_bytes(&self.bytes)),
                    path,
                )?;
            }
            #[cfg(not(unix))]
            {
                bail!("symlink merge unsupported on this platform");
            }
        } else {
            let mode = permissions.unwrap_or(self.mode & 0o777);
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(mode);
            }
            options.open(path)?.write_all(&self.bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            }
        }
        Ok(())
    }
}

fn checked_path(root: &Path, path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    if path.as_os_str().is_empty()
        || path.components().any(|c| {
            !matches!(c, Component::Normal(_))
                || matches!(
                    c.as_os_str().to_str(),
                    Some(".git" | ".loom" | ".custom-tui")
                )
        })
    {
        bail!("unsafe patch path: {}", path.display());
    }
    let root = root.canonicalize()?;
    let absolute = root.join(path);
    let mut parent = absolute.parent();
    while let Some(dir) = parent {
        if dir.exists() {
            if !dir.canonicalize()?.starts_with(&root) {
                bail!("patch parent escapes workspace");
            }
            break;
        }
        parent = dir.parent();
    }
    Ok(absolute)
}
