//! OS-specific window launching kept out of the renderer and runtime protocol.
use anyhow::{Context, Result, bail};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}
pub fn agent_command(executable: &Path, cwd: &Path, endpoint: &str, thread: &str) -> String {
    format!(
        "{} --cwd {} --endpoint {} --agent {}",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&cwd.to_string_lossy()),
        shell_quote(endpoint),
        shell_quote(thread)
    )
}
pub fn open_agent(executable: &Path, cwd: &Path, endpoint: &str, thread: &str) -> Result<()> {
    open_agent_with_demo(executable, cwd, endpoint, thread, false)
}
pub fn open_agent_with_demo(
    executable: &Path,
    cwd: &Path,
    endpoint: &str,
    thread: &str,
    demo: bool,
) -> Result<()> {
    let text = if demo {
        format!(
            "{} --cwd {} --agent {} --demo",
            shell_quote(&executable.to_string_lossy()),
            shell_quote(&cwd.to_string_lossy()),
            shell_quote(thread)
        )
    } else {
        agent_command(executable, cwd, endpoint, thread)
    };
    #[cfg(target_os = "macos")]
    {
        let launcher = write_launcher(&crate::runtime::state_dir(cwd).join("windows"), &text)?;
        let output = Command::new("open")
            .args(["-a", "Terminal"])
            .arg(&launcher)
            .output()?;
        if !output.status.success() {
            bail!(
                "Cannot open Terminal: {}. Run this in another terminal: {text}",
                String::from_utf8_lossy(&output.stderr),
            );
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        bail!(
            "Automatic window launching currently supports macOS Terminal.app. Run this in another terminal: {text}"
        )
    }
}
pub fn write_launcher(directory: &Path, command: &str) -> Result<std::path::PathBuf> {
    use std::fs::{self, OpenOptions};
    fs::create_dir_all(directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = directory.join(format!("agent-{}-{nonce}.command", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700);
    }
    let mut file = options.open(&path)?;
    writeln!(file, "#!/bin/sh\nexec {command}")?;
    Ok(path)
}
pub fn copy(text: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("pbcopy");
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new("xclip");
    #[cfg(not(target_os = "macos"))]
    command.args(["-selection", "clipboard"]);
    let mut child = command
        .stdin(Stdio::piped())
        .spawn()
        .context("Clipboard unavailable")?;
    child
        .stdin
        .take()
        .context("Clipboard input unavailable")?
        .write_all(text.as_bytes())?;
    if !child.wait()?.success() {
        bail!("Clipboard command failed");
    }
    Ok(())
}
pub fn paste() -> Result<String> {
    #[cfg(target_os = "macos")]
    let command = Command::new("pbpaste").output();
    #[cfg(not(target_os = "macos"))]
    let command = Command::new("xclip")
        .args(["-selection", "clipboard", "-o"])
        .output();
    let output = command.context("Clipboard unavailable")?;
    if !output.status.success() {
        bail!(
            "Clipboard read failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).context("Clipboard is not UTF-8 text")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launcher_treats_paths_and_thread_ids_as_data() {
        let text = agent_command(
            Path::new("/tmp/my app'$(touch bad)"),
            Path::new("/tmp/project with spaces"),
            "ws://127.0.0.1:4500",
            "thread'; rm -rf /;",
        );
        assert!(text.contains("'\"'\"'"));
        assert!(text.contains("--cwd '/tmp/project with spaces'"));
    }
    #[cfg(unix)]
    #[test]
    fn command_file_preserves_argv_and_private_permissions() {
        use std::fs::{self, Permissions};
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("app ' literal");
        fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        fs::set_permissions(&executable, Permissions::from_mode(0o700)).unwrap();
        let cwd = dir.path().join("project spaces");
        let id = "agent ' $(touch SHOULD_NOT_EXIST) ; test";
        let command = agent_command(&executable, &cwd, "ws://127.0.0.1:4500", id);
        let launcher = write_launcher(dir.path(), &command).unwrap();
        let output = Command::new("sh")
            .arg(&launcher)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        let expected = vec![
            "--cwd",
            cwd.to_str().unwrap(),
            "--endpoint",
            "ws://127.0.0.1:4500",
            "--agent",
            id,
        ];
        assert_eq!(text.lines().collect::<Vec<_>>(), expected);
        assert!(!dir.path().join("SHOULD_NOT_EXIST").exists());
        assert_eq!(
            fs::metadata(launcher).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
