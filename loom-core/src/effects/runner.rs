use crate::base::resources::{AdmissionError, ResourceGovernor, ResourceKind, local_resources};
use crate::base::{ArtifactRef, ArtifactStore, FileArtifactStore, Phase};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStatus {
    Passed,
    Failed,
    Cancelled,
    TimedOut,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TestResult {
    pub status: RunStatus,
    pub passed: u64,
    pub failed: u64,
    pub failing: Vec<String>,
    pub first_error: Option<Value>,
    pub artifact: ArtifactRef,
    pub ms: u64,
}
pub struct Runner {
    artifacts: FileArtifactStore,
    resources: Arc<ResourceGovernor>,
}

// The reservation must outlive cleanup on I/O errors as well as normal exits.
struct RunningChild {
    child: std::process::Child,
    cleaned: bool,
}
impl RunningChild {
    fn finish(&mut self) -> std::io::Result<()> {
        if !self.cleaned {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &self.child.id().to_string(), "/T", "/F"])
                    .output();
            }
            let _ = self.child.kill();
            self.child.wait()?;
            self.cleaned = true;
        }
        Ok(())
    }
}
impl Drop for RunningChild {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
impl Runner {
    pub fn new(artifacts: FileArtifactStore) -> Self {
        Self::with_resources(artifacts, local_resources())
    }
    pub fn with_resources(artifacts: FileArtifactStore, resources: Arc<ResourceGovernor>) -> Self {
        Self {
            artifacts,
            resources,
        }
    }
    pub fn artifacts(&self) -> &FileArtifactStore {
        &self.artifacts
    }
    pub fn run(
        &self,
        directory: &Path,
        command: &str,
        timeout: Duration,
        cancel: &AtomicBool,
        target_dir: Option<&Path>,
    ) -> Result<TestResult> {
        self.run_with_policy(
            directory,
            command,
            timeout,
            cancel,
            target_dir,
            super::ExecutionPolicy {
                sandbox: super::SandboxMode::FullAccess,
                network: true,
            },
        )
    }
    pub fn run_with_policy(
        &self,
        directory: &Path,
        command: &str,
        timeout: Duration,
        cancel: &AtomicBool,
        target_dir: Option<&Path>,
        policy: super::ExecutionPolicy,
    ) -> Result<TestResult> {
        let started = Instant::now();
        let stdout = tempfile::NamedTempFile::new()?;
        let stderr = tempfile::NamedTempFile::new()?;
        #[cfg(unix)]
        let mut process = {
            let mut c = Command::new("sh");
            c.args(["-c", command]);
            c
        };
        #[cfg(not(unix))]
        let mut process = {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        };
        process.current_dir(directory);
        let temporary = directory.join(".loom/tmp");
        fs::create_dir_all(&temporary)?;
        process
            .env("TMPDIR", &temporary)
            .env("TMP", &temporary)
            .env("TEMP", &temporary);
        let mut writable = vec![
            stdout.path().to_owned(),
            stderr.path().to_owned(),
            temporary,
        ];
        if let Some(target) = target_dir {
            fs::create_dir_all(target)?;
            writable.push(target.to_owned());
        }
        policy.configure_command(&mut process, directory, &writable)?;
        process
            .current_dir(directory)
            .stdin(Stdio::null())
            .stdout(stdout.reopen()?)
            .stderr(stderr.reopen()?);
        if let Some(target) = target_dir {
            process.env("CARGO_TARGET_DIR", target);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.process_group(0);
        }
        let reservation =
            self.resources
                .acquire(ResourceKind::Tool, self.resources.tool_bytes(), || {
                    if cancel.load(Ordering::SeqCst) {
                        Err(AdmissionError::Cancelled)
                    } else if started.elapsed() >= timeout {
                        Err(AdmissionError::TimedOut)
                    } else {
                        Ok(())
                    }
                });
        let status = match reservation {
            Err(AdmissionError::Cancelled) => RunStatus::Cancelled,
            Err(AdmissionError::TimedOut) => RunStatus::TimedOut,
            Err(error) => return Err(error.into()),
            Ok(_reservation) => {
                let mut child = RunningChild {
                    child: process.spawn()?,
                    cleaned: false,
                };
                let status = loop {
                    if cancel.load(Ordering::SeqCst) {
                        break RunStatus::Cancelled;
                    }
                    if started.elapsed() >= timeout {
                        break RunStatus::TimedOut;
                    }
                    if let Some(exit) = child.child.try_wait()? {
                        break if exit.success() {
                            RunStatus::Passed
                        } else {
                            RunStatus::Failed
                        };
                    }
                    std::thread::sleep(Duration::from_millis(10));
                };
                child.finish()?;
                status
            }
        };
        let mut raw = Vec::new();
        writeln!(raw, "$ {command}")?;
        raw.extend_from_slice(&fs::read(stdout.path())?);
        raw.extend_from_slice(&fs::read(stderr.path())?);
        let mut passed = 0;
        let mut failed = 0;
        let mut failing = BTreeSet::new();
        let mut first_error = None;
        parse_check_output(
            &String::from_utf8_lossy(&raw),
            &mut passed,
            &mut failed,
            &mut failing,
            &mut first_error,
        );
        let status = if status == RunStatus::Passed && failed > 0 {
            RunStatus::Failed
        } else {
            status
        };
        Ok(TestResult {
            status,
            passed,
            failed,
            failing: failing.into_iter().collect(),
            first_error,
            artifact: self.artifacts.put(&raw)?,
            ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        })
    }
}

/// Extract only actionable diagnostics; the full stdout/stderr lives in the artifact.
fn parse_check_output(
    output: &str,
    passed: &mut u64,
    failed: &mut u64,
    failing_tests: &mut BTreeSet<String>,
    first_compiler_error: &mut Option<Value>,
) {
    let lines: Vec<&str> = output.lines().collect();
    let mut in_failures = false;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("test result:") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            for pair in parts.windows(2) {
                if pair[1] == "passed;" {
                    *passed += pair[0].parse::<u64>().unwrap_or(0);
                }
                if pair[1] == "failed;" {
                    *failed += pair[0].parse::<u64>().unwrap_or(0);
                }
            }
        }
        if let Some(name) = t
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            failing_tests.insert(name.to_owned());
        }
        if t == "failures:" {
            in_failures = true;
            continue;
        }
        if in_failures {
            if t.is_empty() {
                in_failures = false;
            } else if !t.contains(char::is_whitespace) && !t.starts_with("----") {
                failing_tests.insert(t.to_owned());
            }
        }
        if first_compiler_error.is_none() && (t.starts_with("error[") || t.starts_with("error:")) {
            for loc in lines.iter().skip(i + 1).take(8) {
                let loc = loc.trim();
                if loc.starts_with("error") {
                    break;
                }
                if let Some(where_) = loc.strip_prefix("--> ") {
                    let mut pieces = where_.rsplitn(3, ':');
                    let _column = pieces.next();
                    let row = pieces.next().and_then(|n| n.parse::<u64>().ok());
                    let file = pieces.next();
                    if let (Some(file), Some(line)) = (file, row) {
                        *first_compiler_error =
                            Some(json!({ "message": t, "file": file, "line": line }));
                        break;
                    }
                }
            }
        }
    }
}

const VALIDATE: &[&str] = &[
    "cargo test",
    "cargo build",
    "cargo check",
    "cargo clippy",
    "cargo fmt",
    "cargo run",
    "cargo nextest",
    "npm test",
    "npm run test",
    "npm run build",
    "npm run lint",
    "pnpm test",
    "pnpm run",
    "yarn test",
    "pytest",
    "python -m pytest",
    "go test",
    "go build",
    "go vet",
    "make",
    "tsc",
    "eslint",
    "ruff",
    "mypy",
    "git diff",
];
const EDIT: &[&str] = &["apply_patch", "sed -i", "perl -pi", "perl -i", "tee "];

/// Classify a shell command. Compound commands count as Validate if any part validates,
/// else Edit if any part writes; reads are the remainder.
pub fn classify_command(command: &str) -> Phase {
    let body = unwrap_shell(command);
    let parts: Vec<&str> = body
        .split([';', '|', '&'])
        .map(|p| strip_env(p.trim()))
        .filter(|p| !p.is_empty())
        .collect();
    let starts = |part: &str, list: &[&str]| list.iter().any(|tool| part.starts_with(tool));
    if parts
        .iter()
        .any(|p| starts(p, VALIDATE) || runs_test_script(p))
    {
        Phase::Validate
    } else if parts.iter().any(|p| starts(p, EDIT)) || redirects_to_file(body) {
        Phase::Edit
    } else {
        Phase::Orient
    }
}

/// `PYTHONPATH=x RUST_LOG=y cmd` → `cmd`.
fn strip_env(part: &str) -> &str {
    let mut rest = part;
    while let Some((word, tail)) = rest.split_once(char::is_whitespace) {
        let assignment = word.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if !assignment {
            break;
        }
        rest = tail.trim_start();
    }
    rest
}

/// An interpreter running a test, smoke or spec script (`python3 -m unittest`, `node x.test.js`).
fn runs_test_script(part: &str) -> bool {
    const INTERPRETERS: &[&str] = &[
        "python", "python3", "node", "bash", "sh", "uv", "npx", "deno", "bun",
    ];
    let mut words = part.split_whitespace();
    let Some(head) = words.next() else {
        return false;
    };
    let head = head.rsplit('/').next().unwrap_or(head);
    INTERPRETERS.contains(&head)
        && words.any(|w| {
            let w = w.to_ascii_lowercase();
            ["test", "smoke", "spec", "unittest"]
                .iter()
                .any(|k| w.contains(k))
        })
}

/// `> path` or `>> path` to anything but /dev/null; `2>` and `>&` are stream plumbing.
fn redirects_to_file(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'>' {
            let prev = if i > 0 { bytes[i - 1] } else { b' ' };
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b'>' {
                j += 1;
            }
            let target = body[j..].trim_start();
            let stream =
                prev.is_ascii_digit() || prev == b'&' || prev == b'-' || target.starts_with('&');
            if !stream && !target.is_empty() && !target.starts_with("/dev/null") {
                return true;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// `/bin/zsh -lc "cmd"` → `cmd`.
fn unwrap_shell(command: &str) -> &str {
    let trimmed = command.trim();
    for flag in [" -lc ", " -c "] {
        if let Some(index) = trimmed.find(flag) {
            let head = &trimmed[..index];
            if head.ends_with("sh") {
                return trimmed[index + flag.len()..].trim_matches(|c| c == '"' || c == '\'');
            }
        }
    }
    trimmed
}
