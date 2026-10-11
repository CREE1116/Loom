//! Explicit workspace command hooks. Configuration is snapshotted before model
//! execution; hooks never evaluate model text as shell source.
use crate::{
    agent::events::{Event, Observer, Stage},
    base::{ArtifactStore, FileArtifactStore},
    effects::{ExecutionPolicy, RunStatus, Runner},
};
use anyhow::{Context, Result, ensure};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    TaskStart,
    PreToolUse,
    PostToolUse,
    TaskEnd,
}
impl HookEvent {
    pub fn name(self) -> &'static str {
        match self {
            Self::TaskStart => "TaskStart",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::TaskEnd => "TaskEnd",
        }
    }
    fn blocking(self) -> bool {
        matches!(self, Self::TaskStart | Self::PreToolUse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn config(root: &Path, text: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom/config.toml"), text).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn temporary_symlinks_cannot_make_real_source_files_writable_to_hooks() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("src")).unwrap();
        config(
            root.path(),
            "[hooks]\nenabled=true\n[[hooks.PostToolUse]]\nname='write'\ncommand='touch src/changed'\n",
        );
        std::os::unix::fs::symlink(root.path().join("src"), root.path().join(".loom/tmp")).unwrap();
        let (result, _) = fire(
            root.path(),
            &Hooks::open(root.path()).unwrap(),
            &AtomicBool::new(false),
        );
        assert!(result.err().unwrap().to_string().contains("symlinks"));
        assert!(!root.path().join("src/changed").exists());
    }
    #[test]
    fn hooks_require_opt_in_validate_names_and_keep_a_configuration_snapshot() {
        let root = tempfile::tempdir().unwrap();
        config(
            root.path(),
            "[[hooks.PreToolUse]]\nname='test'\ncommand='exit 2'\n",
        );
        assert!(
            !Hooks::open(root.path())
                .unwrap()
                .has(HookEvent::PreToolUse, "find")
        );
        config(
            root.path(),
            "[hooks]\nenabled=true\n[[hooks.PreToolUse]]\nname='test'\ncommand='exit 2'\nmatcher='find|mcp_*'\n",
        );
        let hooks = Hooks::open(root.path()).unwrap();
        assert!(hooks.has(HookEvent::PreToolUse, "find"));
        assert!(hooks.has(HookEvent::PreToolUse, "mcp_test"));
        assert!(!hooks.has(HookEvent::PreToolUse, "find_other"));
        config(root.path(), "[hooks]\nenabled=false\n");
        assert!(hooks.has(HookEvent::PreToolUse, "find"));
        config(
            root.path(),
            "[hooks]\nenabled=true\n[[hooks.PreToolUse]]\nname='test'\ncommand='exit 2'\ntimeout_ms=0\n",
        );
        assert!(
            Hooks::open(root.path())
                .err()
                .unwrap()
                .to_string()
                .contains("timeout_ms")
        );
        config(
            root.path(),
            "[hooks]\nenabled=true\n[[hooks.PreToolUes]]\nname='test'\ncommand='exit 2'\n",
        );
        assert!(Hooks::open(root.path()).is_err());
    }
    #[cfg(unix)]
    fn fire(root: &Path, hooks: &Hooks, cancel: &AtomicBool) -> (Result<()>, Vec<Event>) {
        let events = Mutex::new(Vec::new());
        let result = hooks.run(
            Invocation {
                event: HookEvent::PostToolUse,
                worker: "main",
                directory: root,
                input: json!({"tool_name":"open", "tool_input":{"target":"$(touch injected)"}}),
                policy: ExecutionPolicy {
                    sandbox: crate::effects::SandboxMode::FullAccess,
                    network: true,
                },
                cancel,
                remaining: Duration::from_secs(2),
                observe: &|event| events.lock().unwrap().push(event),
            },
            &FileArtifactStore::open(root).unwrap(),
        );
        (result, events.into_inner().unwrap())
    }
    #[cfg(unix)]
    #[test]
    fn hook_stdin_is_data_and_exit_status_is_not_inferred_from_output() {
        let root = tempfile::tempdir().unwrap();
        config(
            root.path(),
            "[hooks]\nenabled=true\n[[hooks.PostToolUse]]\nname='stdin'\ncommand=\"cat; printf 'test result: FAILED. 1 failed\\\\n'\"\nmatcher='open'\n",
        );
        let (result, events) = fire(
            root.path(),
            &Hooks::open(root.path()).unwrap(),
            &AtomicBool::new(false),
        );
        result.unwrap();
        let reference =
            serde_json::from_value(events[1].detail["result"]["artifact"].clone()).unwrap();
        let bytes = FileArtifactStore::open(root.path())
            .unwrap()
            .get(&reference, None)
            .unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("\"hook_event_name\":\"PostToolUse\""));
        assert!(output.contains("$(touch injected)"));
        assert!(!root.path().join("injected").exists());
        assert!(!events[1].failed);
    }
    #[cfg(unix)]
    #[test]
    fn post_hooks_continue_after_failure_and_timeouts_or_cancel_cannot_run_forever() {
        let root = tempfile::tempdir().unwrap();
        config(
            root.path(),
            "[hooks]\nenabled=true\n[[hooks.PostToolUse]]\nname='failed'\ncommand='exit 2'\n[[hooks.PostToolUse]]\nname='next'\ncommand='echo next'\n",
        );
        let (result, events) = fire(
            root.path(),
            &Hooks::open(root.path()).unwrap(),
            &AtomicBool::new(false),
        );
        assert!(result.is_err());
        assert_eq!(events.len(), 4);
        assert!(events[1].failed && !events[3].failed);
        config(
            root.path(),
            "[hooks]\nenabled=true\n[[hooks.PostToolUse]]\nname='slow'\ncommand='sleep 5'\ntimeout_ms=20\n",
        );
        let started = Instant::now();
        let (result, _) = fire(
            root.path(),
            &Hooks::open(root.path()).unwrap(),
            &AtomicBool::new(false),
        );
        assert!(result.err().unwrap().to_string().contains("TimedOut"));
        assert!(started.elapsed() < Duration::from_secs(2));
        config(
            root.path(),
            "[hooks]\nenabled=true\n[[hooks.PostToolUse]]\nname='cancelled'\ncommand='touch injected'\n",
        );
        assert!(
            fire(
                root.path(),
                &Hooks::open(root.path()).unwrap(),
                &AtomicBool::new(true)
            )
            .0
            .is_err()
        );
        assert!(!root.path().join("injected").exists());
    }
}

#[derive(Default, Deserialize)]
struct Settings {
    #[serde(default)]
    hooks: Config,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    enabled: bool,
    #[serde(rename = "TaskStart")]
    task_start: Vec<CommandHook>,
    #[serde(rename = "PreToolUse")]
    pre_tool: Vec<CommandHook>,
    #[serde(rename = "PostToolUse")]
    post_tool: Vec<CommandHook>,
    #[serde(rename = "TaskEnd")]
    task_end: Vec<CommandHook>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandHook {
    name: String,
    command: String,
    #[serde(default = "wildcard")]
    matcher: String,
    #[serde(default = "timeout")]
    timeout_ms: u64,
}
fn wildcard() -> String {
    "*".into()
}
fn timeout() -> u64 {
    10_000
}
struct Hook {
    event: HookEvent,
    config: CommandHook,
    matcher: GlobSet,
}
#[derive(Default)]
pub struct Hooks {
    entries: Vec<Hook>,
}

pub struct Invocation<'a> {
    pub event: HookEvent,
    pub worker: &'a str,
    pub directory: &'a Path,
    pub input: Value,
    pub policy: ExecutionPolicy,
    pub cancel: &'a AtomicBool,
    pub remaining: Duration,
    pub observe: &'a Observer<'a>,
}
impl Hooks {
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join(".loom/config.toml");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).context("reading hook configuration"),
        };
        ensure!(
            text.len() <= 1024 * 1024,
            "hook configuration exceeds 1 MiB"
        );
        let settings: Settings = toml::from_str(&text)
            .with_context(|| format!("parsing hooks in {}", path.display()))?;
        if !settings.hooks.enabled {
            return Ok(Self::default());
        }
        let mut entries = Vec::new();
        for (event, configs) in [
            (HookEvent::TaskStart, settings.hooks.task_start),
            (HookEvent::PreToolUse, settings.hooks.pre_tool),
            (HookEvent::PostToolUse, settings.hooks.post_tool),
            (HookEvent::TaskEnd, settings.hooks.task_end),
        ] {
            ensure!(configs.len() <= 32, "at most 32 hooks per event");
            for config in configs {
                ensure!(
                    !config.name.trim().is_empty() && !config.command.trim().is_empty(),
                    "hook name and command required"
                );
                ensure!(
                    (1..=60_000).contains(&config.timeout_ms),
                    "hook {} timeout_ms must be 1..60000",
                    config.name
                );
                ensure!(
                    !entries
                        .iter()
                        .any(|hook: &Hook| hook.event == event && hook.config.name == config.name),
                    "duplicate hook {} for {}",
                    config.name,
                    event.name()
                );
                ensure!(
                    matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse)
                        || config.matcher == "*",
                    "tool matcher only supported for tool hooks"
                );
                let mut matcher = GlobSetBuilder::new();
                for pattern in config.matcher.split('|') {
                    matcher.add(Glob::new(pattern.trim()).context("invalid hook matcher")?);
                }
                entries.push(Hook {
                    event,
                    config,
                    matcher: matcher.build()?,
                });
            }
        }
        Ok(Self { entries })
    }
    pub fn has(&self, event: HookEvent, tool: &str) -> bool {
        self.entries
            .iter()
            .any(|hook| hook.event == event && hook.matcher.is_match(tool))
    }
    pub fn run(&self, invocation: Invocation<'_>, artifacts: &FileArtifactStore) -> Result<()> {
        let tool = invocation.input["tool_name"].as_str().unwrap_or("");
        if !self.has(invocation.event, tool) {
            return Ok(());
        }
        let mut input = invocation.input.clone();
        input["hook_event_name"] = json!(invocation.event.name());
        input["worker_id"] = json!(invocation.worker);
        input["cwd"] = json!(invocation.directory);
        input["sandbox"] = json!(invocation.policy.sandbox);
        input["network"] = json!(invocation.policy.network);
        let input = serde_json::to_vec(&input)?;
        ensure!(input.len() <= 1024 * 1024, "hook input exceeds 1 MiB");
        let runner = Runner::new(artifacts.clone());
        let started = Instant::now();
        let mut errors = Vec::new();
        for hook in self
            .entries
            .iter()
            .filter(|hook| hook.event == invocation.event && hook.matcher.is_match(tool))
        {
            let target = format!("hook · {} · {}", invocation.event.name(), hook.config.name);
            let mut detail = json!({"tool":"hook", "hook_event":invocation.event.name(), "name":hook.config.name});
            detail["started"] = json!(true);
            (invocation.observe)(Event::new(
                invocation.worker,
                Stage::Run,
                &target,
                detail.clone(),
            ));
            let result = runner.run_hook(
                invocation.directory,
                &hook.config.command,
                &input,
                Duration::from_millis(hook.config.timeout_ms)
                    .min(invocation.remaining.saturating_sub(started.elapsed())),
                invocation.cancel,
                invocation.policy,
            );
            detail.as_object_mut().unwrap().remove("started");
            detail["completed"] = json!(true);
            let error = match result {
                Ok(result) => {
                    detail["result"] = serde_json::to_value(&result)?;
                    detail["result"]["artifact_target"] =
                        json!(format!("artifact:{}", result.artifact.id));
                    (result.status != RunStatus::Passed).then(|| {
                        let output = artifacts
                            .get(
                                &result.artifact,
                                Some(0..2048.min(result.artifact.bytes as usize)),
                            )
                            .unwrap_or_default();
                        format!(
                            "hook {}/{} {:?}: {}",
                            invocation.event.name(),
                            hook.config.name,
                            result.status,
                            String::from_utf8_lossy(&output)
                        )
                    })
                }
                Err(error) => Some(format!(
                    "hook {}/{}: {error}",
                    invocation.event.name(),
                    hook.config.name
                )),
            };
            if let Some(error) = &error {
                detail["error"] = json!(error);
            }
            let mut event = Event::new(invocation.worker, Stage::Run, target, detail);
            event.failed = error.is_some();
            (invocation.observe)(event);
            if let Some(error) = error {
                errors.push(error);
                if invocation.event.blocking() {
                    break;
                }
            }
        }
        ensure!(errors.is_empty(), "{}", errors.join("\n"));
        Ok(())
    }
}
