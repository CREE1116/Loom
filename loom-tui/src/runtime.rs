//! Per-workspace UI state: the state directory, the last conversation pointer, and the
//! shutdown flag shared with running work.
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

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

/// Set when the window is closing (signal or Quit): running Native work cancels itself.
pub static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// Start a fresh Loom session by default; `--resume last` delegates to the
/// backend's stored-session lookup, and an explicit ID selects that session.
pub fn loom_initial_session(resume: Option<&str>, new: bool) -> Option<String> {
    if !new {
        match resume {
            Some("last") => return None,
            Some(id) => return Some(id.to_owned()),
            None => {}
        }
    }
    Some("new".to_owned())
}

pub fn exit_notice(session_id: Option<&str>) -> String {
    match session_id {
        Some(id) => format!("세션 ID: {id}"),
        None => "세션을 닫았습니다.".to_owned(),
    }
}

/// Clear only a stale pointer. Conversation data stays untouched, including when the
/// caller falls back to a new conversation.
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
    fn workspace_recent_thread_is_optional_and_can_be_cleared_without_deleting_history() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(recent_thread(dir.path()), None);
        remember_thread(dir.path(), "thread-one").unwrap();
        assert_eq!(recent_thread(dir.path()).as_deref(), Some("thread-one"));
        forget_recent_thread(dir.path()).unwrap();
        assert!(recent_thread(dir.path()).is_none());
    }
}
