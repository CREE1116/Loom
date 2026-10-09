//! Git trees for completed turns. The user's index and HEAD are never changed.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Serialize, Deserialize)]
struct Point {
    turn: String,
    tree: String,
}
pub struct Checkpoints {
    root: PathBuf,
    file: PathBuf,
    sessions: HashMap<String, Vec<Point>>,
    cancel: Option<Arc<AtomicBool>>,
}
pub struct RestorePlan {
    target: String,
    expected: String,
    pub paths: Vec<String>,
    pub added: usize,
    pub removed: usize,
}

impl Checkpoints {
    pub fn new(root: &Path) -> Self {
        let file = crate::runtime::state_dir(root).join("checkpoints.json");
        let sessions = fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            root: root.to_owned(),
            file,
            sessions,
            cancel: None,
        }
    }
    pub fn background(root: &Path, cancel: Arc<AtomicBool>) -> Self {
        let mut checkpoints = Self::new(root);
        checkpoints.cancel = Some(cancel);
        checkpoints
    }
    pub fn reload(&mut self) {
        if let Ok(bytes) = fs::read(&self.file)
            && let Ok(sessions) = serde_json::from_slice(&bytes)
        {
            self.sessions = sessions;
        }
    }
    fn output(&self, command: &mut Command) -> Result<std::process::Output> {
        let Some(cancel) = &self.cancel else {
            return Ok(command.output()?);
        };
        if cancel.load(Ordering::Relaxed) {
            bail!("Checkpoint cancelled");
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        // Drain both pipes while Git runs; ls-files can exceed a pipe's capacity.
        let read = |mut pipe: Box<dyn std::io::Read + Send>| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                pipe.read_to_end(&mut bytes).map(|_| bytes)
            })
        };
        let stdout = read(Box::new(child.stdout.take().unwrap()));
        let stderr = read(Box::new(child.stderr.take().unwrap()));
        let status = loop {
            if cancel.load(Ordering::Relaxed) {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                break child.wait()?;
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        Ok(std::process::Output {
            status,
            stdout: stdout
                .join()
                .map_err(|_| anyhow::anyhow!("Git stdout reader failed"))??,
            stderr: stderr
                .join()
                .map_err(|_| anyhow::anyhow!("Git stderr reader failed"))??,
        })
    }
    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = self.output(
            Command::new("git")
                .current_dir(&self.root)
                .arg("--literal-pathspecs")
                .args(args),
        )?;
        if !output.status.success() {
            bail!("Git: {}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(output.stdout)
    }
    fn snapshot(&self) -> Result<String> {
        fs::create_dir_all(self.file.parent().unwrap())?;
        let revision =
            loom_core::base::Revision::capture_with(&self.root, |command| self.output(command))?;
        Ok(revision.to_string())
    }
    pub fn record(&mut self, session: &str, turn: &str) -> Result<()> {
        let tree = self.snapshot()?;
        let points = self.sessions.entry(session.into()).or_default();
        if let Some(point) = points.iter_mut().find(|p| p.turn == turn) {
            point.tree = tree;
        } else {
            points.push(Point {
                turn: turn.into(),
                tree,
            });
        }
        self.save(session)
    }
    fn save(&self, session: &str) -> Result<()> {
        use fs2::FileExt;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.file.with_extension("lock"))?;
        lock.lock_exclusive()?;
        let mut sessions: HashMap<String, Vec<Point>> = match fs::read(&self.file) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error.into()),
        };
        let points = sessions.entry(session.into()).or_default();
        for point in self.sessions.get(session).into_iter().flatten() {
            if let Some(existing) = points.iter_mut().find(|old| old.turn == point.turn) {
                *existing = point.clone();
            } else {
                points.push(point.clone());
            }
        }
        let temporary = self
            .file
            .with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temporary, serde_json::to_vec(&sessions)?)?;
        fs::rename(temporary, &self.file)?;
        Ok(())
    }
    pub fn plan(&self, session: &str, turn: &str) -> Result<RestorePlan> {
        let points = self
            .sessions
            .get(session)
            .context("이 대화에는 코드 복원 기록이 없습니다.")?;
        let target = points
            .iter()
            .find(|p| p.turn == turn)
            .context("선택한 시점의 코드 복원 기록이 없습니다.")?
            .tree
            .clone();
        let expected = points.last().unwrap().tree.clone();
        let paths = self
            .git(&[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                "-z",
                &target,
                &expected,
            ])?
            .split(|byte| *byte == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8(p.to_vec()))
            .collect::<Result<Vec<_>, _>>()?;
        let diff = self.git(&["diff", "--numstat", "--no-renames", "--no-ext-diff", "--no-textconv", &expected, &target, "--"])?;
        let (mut added, mut removed) = (0, 0);
        for line in String::from_utf8_lossy(&diff).lines() {
            let mut parts = line.split('\t');
            added += parts.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
            removed += parts.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
        }
        let plan = RestorePlan {
            target,
            expected,
            paths,
            added,
            removed,
        };
        self.validate(&plan)?;
        Ok(plan)
    }
    fn blob(&self, tree: &str, path: &str) -> Result<Option<(Vec<u8>, bool)>> {
        let listing = self.git(&["ls-tree", "-z", tree, "--", path])?;
        if listing.is_empty() {
            return Ok(None);
        }
        if !listing.starts_with(b"100644 ") && !listing.starts_with(b"100755 ") {
            bail!("특수 파일은 자동 복원할 수 없습니다: {path}");
        }
        Ok(Some((
            self.git(&["show", &format!("{tree}:{path}")])?,
            listing.starts_with(b"100755 "),
        )))
    }
    fn validate(&self, plan: &RestorePlan) -> Result<()> {
        for path in &plan.paths {
            let relative = Path::new(path);
            if relative
                .components()
                .any(|p| !matches!(p, std::path::Component::Normal(_)))
            {
                bail!("복원 경로가 올바르지 않습니다: {path}");
            }
            let mut parent = self.root.clone();
            for part in relative.components() {
                parent.push(part);
                if let Ok(meta) = fs::symlink_metadata(&parent) {
                    if meta.file_type().is_symlink() {
                        bail!("심볼릭 링크 경로는 복원할 수 없습니다: {path}");
                    }
                }
            }
            let expected = self.blob(&plan.expected, path)?;
            self.blob(&plan.target, path)?;
            let current = match fs::read(self.root.join(path)) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            #[cfg(unix)]
            if let Some((_, executable)) = &expected {
                use std::os::unix::fs::PermissionsExt;
                if fs::metadata(self.root.join(path))?.permissions().mode() & 0o111
                    != if *executable { 0o111 } else { 0 }
                {
                    bail!("이후에 실행 권한이 변경되어 복원을 중단합니다: {path}");
                }
            }
            if current.as_ref() != expected.as_ref().map(|(bytes, _)| bytes) {
                bail!("이후에 직접 수정된 파일이 있어 복원을 중단합니다: {path}");
            }
        }
        Ok(())
    }
    pub fn restore(&self, plan: &RestorePlan) -> Result<()> {
        self.validate(plan)?;
        let files = plan
            .paths
            .iter()
            .map(|path| Ok((path, self.blob(&plan.target, path)?)))
            .collect::<Result<Vec<_>>>()?;
        for (path, blob) in files {
            let file = self.root.join(path);
            if let Some((bytes, executable)) = blob {
                fs::create_dir_all(file.parent().unwrap())?;
                fs::write(&file, bytes)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(
                        file,
                        fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
                    )?;
                }
                #[cfg(not(unix))]
                let _ = executable;
            } else {
                fs::remove_file(file)?;
            }
        }
        Ok(())
    }
    pub fn inherit(&mut self, old: &str, new: &str, turn: &str) -> Result<()> {
        if let Some(points) = self.sessions.get(old) {
            if let Some(index) = points.iter().position(|p| p.turn == turn) {
                self.sessions.insert(new.into(), points[..=index].to_vec());
                self.save(new)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_snapshots_preserve_revision_commands_with_piped_input() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(dir.path().join("code"), "before").unwrap();
        let mut checkpoints = Checkpoints::background(dir.path(), Arc::new(AtomicBool::new(false)));
        checkpoints.record("main", "one").unwrap();
        fs::write(dir.path().join("code"), "after").unwrap();
        checkpoints.record("main", "two").unwrap();
        let plan = checkpoints.plan("main", "one").unwrap();
        checkpoints.restore(&plan).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("code")).unwrap(),
            "before"
        );
    }
    #[test]
    fn restore_handles_created_deleted_files_and_preserves_index_and_manual_changes() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        fs::write(dir.path().join(".gitignore"), ".custom-tui/\n").unwrap();
        fs::write(dir.path().join("code"), "before").unwrap();
        fs::write(dir.path().join("deleted"), "keep").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "code"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let user_index = fs::read(dir.path().join(".git/index")).unwrap();
        let mut checkpoints = Checkpoints::new(dir.path());
        checkpoints.record("main", "one").unwrap();
        fs::write(dir.path().join("code"), "after").unwrap();
        fs::write(dir.path().join("created"), "new").unwrap();
        fs::remove_file(dir.path().join("deleted")).unwrap();
        checkpoints.record("main", "two").unwrap();
        let plan = checkpoints.plan("main", "one").unwrap();
        fs::write(dir.path().join("code"), "manual").unwrap();
        assert!(checkpoints.restore(&plan).is_err());
        assert!(dir.path().join("created").exists());
        fs::write(dir.path().join("code"), "after").unwrap();
        fs::write(dir.path().join("unrelated"), "manual").unwrap();
        Checkpoints::new(dir.path()).restore(&plan).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("code")).unwrap(),
            "before"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("deleted")).unwrap(),
            "keep"
        );
        assert!(!dir.path().join("created").exists());
        assert!(dir.path().join("unrelated").exists());
        assert_eq!(fs::read(dir.path().join(".git/index")).unwrap(), user_index);
    }
}
