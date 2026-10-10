//! OS-enforced execution policy. Restricted execution never falls back to an
//! unsandboxed process. Call configure_command before installing stdio/pre_exec.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    FullAccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionPolicy {
    pub sandbox: SandboxMode,
    pub network: bool,
}
impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            sandbox: SandboxMode::WorkspaceWrite,
            network: false,
        }
    }
}

impl ExecutionPolicy {
    /// Wrap the executable, retaining arguments, cwd, and environment overrides.
    /// Set stdio and Unix process hooks *after* this call: std::process::Command
    /// does not expose those settings for transfer to the sandbox executable.
    /// It also hides env_clear: remove inherited keys explicitly when needed.
    /// Writable exceptions must exist and are canonicalized, including symlinks.
    /// FullAccess controls filesystem access; `network` remains independent.
    pub fn configure_command(
        &self,
        command: &mut Command,
        workspace: &Path,
        writable: &[PathBuf],
    ) -> Result<()> {
        if self.sandbox == SandboxMode::FullAccess && self.network {
            return Ok(());
        }
        // macOS refuses to apply a sandbox inside a sandbox, so a Loom Core started under
        // Loom's own validation (Loom's tests of itself) could run nothing. The outer
        // sandbox still confines everything this process starts; nesting adds nothing.
        if already_sandboxed() {
            return Ok(());
        }
        let workspace = dunce::canonicalize(workspace)
            .context("canonicalizing sandbox workspace")?;
        let writable: Vec<_> = writable
            .iter()
            .map(|path| {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    workspace.join(path)
                };
                dunce::canonicalize(&path).with_context(|| {
                    format!("canonicalizing sandbox writable path {}", path.display())
                })
            })
            .collect::<Result<_>>()?;
        let protected = protected_paths(&workspace)?;
        for path in &writable {
            if protected.iter().any(|config| path.starts_with(config)) {
                bail!(
                    "sandbox writable exception includes protected configuration: {}",
                    path.display()
                );
            }
            if self.sandbox != SandboxMode::FullAccess && path.parent().is_none() {
                bail!("sandbox writable exception cannot grant the filesystem root");
            }
        }

        #[cfg(target_os = "macos")]
        let mut wrapper = {
            let executable = Path::new("/usr/bin/sandbox-exec");
            if !executable.is_file() {
                bail!("macOS sandbox-exec unavailable; refusing unsandboxed execution");
            }
            let mut profile = String::from(
                "(version 1)\n(deny default)\n(allow process*)\n(allow file-read*)\n(allow sysctl-read)\n(allow mach-lookup)\n(allow signal (target same-sandbox))\n",
            );
            if self.network {
                profile.push_str("(allow network*)\n");
            } else if loopback_allowed() {
                // Opt-in (LOOM_SANDBOX_LOOPBACK=1): tests that open sockets on localhost
                // can run. Off by default because localhost also serves keyed gateways
                // and runtimes that sandboxed code must not reach.
                profile.push_str(
                    "(allow network-bind (local ip \"localhost:*\"))\n(allow network-inbound (local ip \"localhost:*\"))\n(allow network-outbound (remote ip \"localhost:*\"))\n",
                );
            }
            if self.sandbox == SandboxMode::FullAccess {
                profile.push_str("(allow file-write*)\n");
            } else {
                profile.push_str(
                    "(allow file-write* (literal \"/dev/null\") (literal \"/dev/tty\"))\n",
                );
                if self.sandbox == SandboxMode::WorkspaceWrite {
                    profile.push_str(&format!(
                        "(allow file-write* (subpath {}))\n",
                        seatbelt_path(&workspace)?
                    ));
                }
                for path in &writable {
                    profile.push_str(&format!(
                        "(allow file-write* (subpath {}))\n",
                        seatbelt_path(path)?
                    ));
                }
                for path in &protected {
                    profile.push_str(&format!(
                        "(deny file-write* (subpath {}))\n",
                        seatbelt_path(path)?
                    ));
                }
            }
            let mut wrapper = Command::new(executable);
            wrapper.args(["-p", &profile, "--"]);
            wrapper
        };
        #[cfg(target_os = "linux")]
        let mut wrapper = {
            // Fixed system paths avoid accepting a workspace-supplied fake bwrap.
            let executable = ["/usr/bin/bwrap", "/bin/bwrap"]
                .into_iter()
                .map(Path::new)
                .find(|path| path.is_file())
                .context("Linux sandbox bwrap unavailable; refusing unsandboxed execution")?;
            let mut wrapper = Command::new(executable);
            wrapper.args([
                "--die-with-parent",
                "--new-session",
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts",
            ]);
            if !self.network {
                wrapper.arg("--unshare-net");
            }
            if self.sandbox == SandboxMode::FullAccess {
                wrapper.args(["--bind", "/", "/"]);
            } else {
                wrapper.args(["--ro-bind", "/", "/"]);
                if self.sandbox == SandboxMode::WorkspaceWrite {
                    wrapper.arg("--bind").arg(&workspace).arg(&workspace);
                }
                for path in &writable {
                    wrapper.arg("--bind").arg(path).arg(path);
                }
                for path in &protected {
                    if path.exists() {
                        wrapper.arg("--ro-bind").arg(path).arg(path);
                    }
                }
            }
            // No host /proc handles or writable host device nodes inside the jail.
            wrapper.args(["--proc", "/proc", "--dev", "/dev", "--"]);
            wrapper
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (command, workspace, writable, protected);
            Ok(())
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            wrapper.arg(command.get_program()).args(command.get_args());
            if let Some(cwd) = command.get_current_dir() {
                wrapper.current_dir(cwd);
            }
            // Explicit removals are observable; env_clear itself is not.
            wrapper.env_clear();
            // get_envs only exposes overrides, so callers needing an entirely
            // empty environment should remove inherited keys explicitly.
            wrapper.envs(std::env::vars_os());
            for (name, value) in command.get_envs() {
                match value {
                    Some(value) => {
                        wrapper.env(name, value);
                    }
                    None => {
                        wrapper.env_remove(name);
                    }
                }
            }
            *command = wrapper;
            Ok(())
        }
    }
}

fn protected_paths(workspace: &Path) -> Result<Vec<PathBuf>> {
    // Protect both existing symlink targets and their lexical directory entries.
    let mut result = Vec::new();
    for name in [
        ".git",
        ".codex",
        ".claude",
        ".agents",
        ".loom/config",
        ".loom/config.toml",
    ] {
        let path = workspace.join(name);
        if path.exists() {
            result.push(dunce::canonicalize(&path)?);
        }
        result.push(path);
    }
    Ok(result)
}

#[cfg(target_os = "macos")]
fn seatbelt_path(path: &Path) -> Result<String> {
    let path = path.to_str().context("sandbox paths must be UTF-8")?;
    if path.chars().any(char::is_control) {
        bail!("sandbox path contains control characters");
    }
    Ok(format!(
        "\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Whether sandboxed commands may use localhost sockets (see the macOS profile).
pub fn loopback_allowed() -> bool {
    std::env::var("LOOM_SANDBOX_LOOPBACK").is_ok_and(|v| v == "1")
}

/// Output of a sandboxed run that failed on a denied socket or network call: the
/// environment, not the change, caused it.
pub fn denied_by_sandbox(output: &str) -> bool {
    ["Operation not permitted (os error 1)", "PermissionDenied", "EPERM"]
        .iter()
        .any(|needle| output.contains(needle))
}

/// Whether this process already runs inside an OS sandbox (macOS seatbelt).
pub fn already_sandboxed() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn sandbox_check(pid: libc::pid_t, operation: *const libc::c_char, filter: libc::c_int, ...) -> libc::c_int;
        }
        // SANDBOX_FILTER_NONE with no operation: nonzero when the process is sandboxed.
        unsafe { sandbox_check(libc::getpid(), std::ptr::null(), 0) != 0 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}
