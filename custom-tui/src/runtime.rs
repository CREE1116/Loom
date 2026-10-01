//! Launch or attach to one detached Codex runtime per workspace.
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize, Debug)]
pub struct RuntimeInfo {
    pub endpoint: String,
    pub pid: u32,
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub version: String,
}
/// Prefer this checkout's installed runtime; explicit --codex-bin always wins.
pub fn default_binary() -> String {
    let local = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../.custom-tui/codex-runtime/node_modules/.bin/codex");
    if local.is_file() {
        local.to_string_lossy().into_owned()
    } else {
        "codex".into()
    }
}
fn executable_path(binary: &str) -> Result<PathBuf> {
    let path = Path::new(binary);
    if path.components().count() > 1 {
        return path
            .canonicalize()
            .with_context(|| format!("Cannot find {binary}"));
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(binary))
        .find(|path| path.is_file())
        .context("Cannot find Codex; install it or pass --codex-bin")?
        .canonicalize()
        .context("Cannot resolve Codex executable")
}
fn matches_runtime(info: &RuntimeInfo, executable: &str, version: &str) -> bool {
    info.executable == executable && info.version == version
}
pub fn state_dir(cwd: &Path) -> PathBuf {
    cwd.join(".custom-tui")
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn ensure(cwd: &Path, codex_bin: &str) -> Result<RuntimeInfo> {
    let executable = executable_path(codex_bin)?;
    let output = Command::new(&executable).arg("--version").output()?;
    if !output.status.success() {
        bail!("Cannot read Codex version from {}", executable.display());
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let executable = executable.to_string_lossy().into_owned();
    let dir = state_dir(cwd);
    private_dir(&dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("runtime.lock"))?;
    lock.lock_exclusive()?;
    let info_path = dir.join("runtime.json");
    if let Ok(text) = fs::read_to_string(&info_path)
        && let Ok(info) = serde_json::from_str::<RuntimeInfo>(&text)
        && matches_runtime(&info, &executable, &version)
        && crate::transport::connect_socket(&info.endpoint).is_ok()
    {
        return Ok(info);
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let endpoint = format!("ws://{address}");
    // Keep older servers alive for windows already attached to them.
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("runtime.log"))?;
    let mut command = Command::new(&executable);
    command
        .args(["app-server", "--listen", &endpoint])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("Cannot start {codex_bin}; install Codex or pass --codex-bin"))?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            let info = RuntimeInfo {
                endpoint,
                pid: child.id(),
                executable,
                version,
            };
            fs::write(&info_path, serde_json::to_vec(&info)?)?;
            return Ok(info);
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "Codex exited with {status}; see {}",
                dir.join("runtime.log").display()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    bail!(
        "Runtime startup timed out; see {}",
        dir.join("runtime.log").display()
    )
}

pub fn remember_thread(cwd: &Path, id: &str) -> Result<()> {
    let dir = state_dir(cwd);
    private_dir(&dir)?;
    let temp = dir.join(format!("last-thread-{}.tmp", std::process::id()));
    fs::write(&temp, id)?;
    fs::rename(temp, dir.join("last-thread"))?;
    Ok(())
}
pub fn last_thread(cwd: &Path) -> Result<String> {
    Ok(fs::read_to_string(state_dir(cwd).join("last-thread"))
        .context("No saved session; start without --resume first")?
        .trim()
        .into())
}

/// A workspace can have no persisted conversation yet. That is an ordinary
/// first launch, not an error. Explicit `--resume last` still uses last_thread.
pub fn recent_thread(cwd: &Path) -> Option<String> {
    fs::read_to_string(state_dir(cwd).join("last-thread"))
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
}

/// Clear only a stale pointer. Conversation data belongs to Codex and stays
/// untouched, including when the caller falls back to a new conversation.
pub fn forget_recent_thread(cwd: &Path) -> Result<()> {
    let path = state_dir(cwd).join("last-thread");
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_reuse_requires_same_binary_and_version() {
        let legacy: RuntimeInfo =
            serde_json::from_str(r#"{"endpoint":"ws://127.0.0.1:1","pid":1}"#).unwrap();
        assert!(!matches_runtime(&legacy, "/codex", "0.159.3"));
        let info = RuntimeInfo {
            executable: "/codex".into(),
            version: "0.159.3".into(),
            ..legacy
        };
        assert!(matches_runtime(&info, "/codex", "0.159.3"));
        assert!(!matches_runtime(&info, "/other/codex", "0.159.3"));
        assert!(!matches_runtime(&info, "/codex", "0.160.0"));
    }
    #[test]
    fn workspace_recent_thread_is_optional_and_can_be_cleared_without_deleting_history() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(recent_thread(dir.path()), None);
        remember_thread(dir.path(), "thread-one").unwrap();
        assert_eq!(recent_thread(dir.path()).as_deref(), Some("thread-one"));
        forget_recent_thread(dir.path()).unwrap();
        assert!(recent_thread(dir.path()).is_none());
    }
}
