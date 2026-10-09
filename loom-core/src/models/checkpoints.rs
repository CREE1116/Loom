//! GGUF checkpoints owned by Loom. IDs are paths relative to `LOOM_MODELS`.
use super::local;
use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Checkpoint {
    pub id: String,
    pub path: PathBuf,
    pub bytes: u64,
}

/// List regular GGUF files beneath the managed root, skipping symlinks and partials.
pub fn installed() -> Result<Vec<Checkpoint>> {
    installed_in(&local::models_dir())
}

fn installed_in(root: &Path) -> Result<Vec<Checkpoint>> {
    let root = match fs::canonicalize(root) {
        Ok(root) => root,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    fn walk(root: &Path, directory: &Path, out: &mut Vec<Checkpoint>) -> Result<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else if kind.is_file() {
                let Some(id) = path
                    .strip_prefix(root)?
                    .to_str()
                    .map(|s| s.replace(std::path::MAIN_SEPARATOR, "/"))
                else {
                    continue;
                };
                if local::validate_gguf_path(&id).is_err() {
                    continue;
                }
                let path = local::managed_path(root, &id, false)?;
                out.push(Checkpoint {
                    id,
                    bytes: fs::symlink_metadata(&path)?.len(),
                    path,
                });
            }
        }
        Ok(())
    }
    let mut checkpoints = Vec::new();
    walk(&root, &root, &mut checkpoints)?;
    checkpoints.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(checkpoints)
}

/// Remove exactly one managed GGUF file. Parent directories and other files remain.
pub fn remove(id: &str) -> Result<()> {
    remove_in(&local::models_dir(), id)
}

fn remove_in(root: &Path, id: &str) -> Result<()> {
    let path = local::managed_path(root, id, false)?;
    let location = local::CheckpointLocation::checked(&path)?;
    let _file = location.open(false, false)?;
    location.remove(false)
}

/// Pull only HF GGUF weights. Returning false cancels and retains the resumable `.part`.
pub fn pull(spec: &str, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<PathBuf> {
    local::pull_progress(spec, progress)
}

/// Validate pasted repo names, GGUF labels or links before starting a transfer.
pub fn validate_spec(spec: &str) -> Result<()> {
    local::parse_spec(spec).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_returns_sorted_relative_ids_sizes_and_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("models");
        assert!(installed_in(&root).unwrap().is_empty());
        fs::create_dir_all(root.join("owner/repo/nested")).unwrap();
        fs::write(root.join("owner/repo/nested/b.GGUF"), b"weights").unwrap();
        fs::write(root.join("a.gguf"), b"a").unwrap();
        fs::write(root.join("unfinished.part"), b"partial").unwrap();
        fs::write(root.join("README.md"), b"docs").unwrap();
        fs::create_dir(root.join("directory.gguf")).unwrap();
        let files = installed_in(&root).unwrap();
        assert_eq!(
            files
                .iter()
                .map(|c| (c.id.as_str(), c.bytes))
                .collect::<Vec<_>>(),
            vec![("a.gguf", 1), ("owner/repo/nested/b.GGUF", 7)]
        );
        assert!(files.iter().all(
            |c| c.path.is_absolute() && c.path == fs::canonicalize(&root).unwrap().join(&c.id)
        ));
    }

    #[test]
    fn remove_is_bounded_to_one_regular_gguf() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("models");
        fs::create_dir_all(root.join("owner/repo")).unwrap();
        fs::write(dir.path().join("outside.gguf"), b"keep").unwrap();
        fs::write(root.join("owner/repo/a.gguf"), b"remove").unwrap();
        fs::write(root.join("owner/repo/README.md"), b"keep").unwrap();
        fs::create_dir(root.join("directory.gguf")).unwrap();
        for id in [
            "../outside.gguf",
            "/outside.gguf",
            "owner//repo/a.gguf",
            "owner/./repo/a.gguf",
            "owner/repo/../a.gguf",
            "owner\\repo\\a.gguf",
            "owner/repo/README.md",
            "directory.gguf",
            "owner/repo",
        ] {
            assert!(remove_in(&root, id).is_err(), "accepted {id}");
        }
        remove_in(&root, "owner/repo/a.gguf").unwrap();
        assert!(!root.join("owner/repo/a.gguf").exists());
        assert!(root.join("owner/repo/README.md").exists());
        assert!(root.join("owner/repo").is_dir());
        assert_eq!(fs::read(dir.path().join("outside.gguf")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn list_and_remove_never_follow_symlink_files_directories_or_cycles() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("models");
        fs::create_dir(&root).unwrap();
        fs::write(dir.path().join("outside.gguf"), b"keep").unwrap();
        fs::write(root.join("inside.gguf"), b"ok").unwrap();
        symlink(dir.path(), root.join("escape")).unwrap();
        symlink(dir.path().join("outside.gguf"), root.join("link.gguf")).unwrap();
        symlink(&root, root.join("cycle")).unwrap();
        let files = installed_in(&root).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, "inside.gguf");
        assert!(remove_in(&root, "escape/outside.gguf").is_err());
        assert!(remove_in(&root, "link.gguf").is_err());
        assert!(remove_in(&root, "cycle/inside.gguf").is_err());
        assert_eq!(fs::read(dir.path().join("outside.gguf")).unwrap(), b"keep");
        assert!(local::managed_path(&root, "escape/new.gguf", true).is_err());
    }
}
