//! Explicit workspace command hooks. Configuration is snapshotted before model
//! execution; hooks never evaluate model text as shell source.
use crate::{
    agent::events::{Event, Observer, Stage},
    base::FileArtifactStore,
    effects::{ExecutionPolicy, RunStatus, Runner},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
    Interrupt,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    SessionStart,
    SessionEnd,
}
impl HookEvent {
    pub fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::Interrupt => "Interrupt",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
        }
    }
    fn blocking(self) -> bool {
        matches!(self, Self::PreToolUse | Self::UserPromptSubmit)
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
    #[test]
    fn compatible_groups_use_regex_seconds_and_reject_unsupported_handlers() {
        let root = tempfile::tempdir().unwrap();
        config(
            root.path(),
            "[[hooks.PreToolUse]]\nmatcher='^Bash$'\n[[hooks.PreToolUse.hooks]]\ntype='command'\ncommand='echo test'\ntimeout=3\n",
        );
        let hooks = Hooks::open(root.path()).unwrap();
        assert!(hooks.has(HookEvent::PreToolUse, "run"));
        assert!(!hooks.has(HookEvent::PreToolUse, "run_else"));
        assert_eq!(hooks.entries[0].config.timeout_ms, 3000);
        assert!(hooks.guidance().contains("built-in runtime knowledge"));
        assert!(hooks.guidance().contains("PreToolUse /"));
        config(
            root.path(),
            "[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype='command'\ncommand='echo test'\nasync=true\n",
        );
        assert!(
            Hooks::open(root.path())
                .err()
                .unwrap()
                .to_string()
                .contains("async")
        );
    }
    #[test]
    fn compatible_decisions_rewrite_context_and_stop_have_distinct_meanings() {
        let mut outcome = HookOutcome::default();
        decode_output(HookEvent::PreToolUse,r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"safe"},"additionalContext":"policy"}}"#,16,&mut outcome).unwrap();
        assert_eq!(outcome.updated_input.unwrap()["command"], "safe");
        assert_eq!(outcome.context, vec!["policy"]);
        let mut outcome = HookOutcome::default();
        decode_output(
            HookEvent::PostToolUse,
            r#"{"decision":"block","reason":"review"}"#,
            100,
            &mut outcome,
        )
        .unwrap();
        assert_eq!(outcome.feedback.as_deref(), Some("review"));
        assert!(outcome.halt.is_none());
        let mut outcome = HookOutcome::default();
        decode_output(
            HookEvent::Stop,
            r#"{"decision":"block","reason":"test again"}"#,
            100,
            &mut outcome,
        )
        .unwrap();
        assert!(outcome.continue_work);
        decode_output(
            HookEvent::Stop,
            r#"{"continue":false,"stopReason":"enough"}"#,
            100,
            &mut outcome,
        )
        .unwrap();
        assert!(!outcome.continue_work && outcome.halt.as_deref() == Some("enough"));
        assert!(
            decode_output(
                HookEvent::PreToolUse,
                r#"{"continue":false}"#,
                100,
                &mut outcome
            )
            .is_err()
        );
    }

    #[test]
    fn old_flat_format_and_old_lifecycle_events_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        config(
            root.path(),
            "[[hooks.PreToolUse]]\nname='old'\ncommand='echo old'\n",
        );
        assert!(Hooks::open(root.path()).is_err());
        config(root.path(), "[[hooks.TaskEnd]]\n");
        assert!(Hooks::open(root.path()).is_err());
    }
    #[cfg(unix)]
    fn fire(root: &Path, hooks: &Hooks, cancel: &AtomicBool) -> (Result<HookOutcome>, Vec<Event>) {
        let events = Mutex::new(Vec::new());
        let result = hooks.run(
            Invocation {
                event: HookEvent::PostToolUse,
                worker: "main",
                directory: root,
                input: json!({"tool_name":"open","tool_input":{"target":"$(touch injected)"}}),
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
    fn command_stdin_is_data_and_stdout_and_stderr_are_not_confused() {
        let root = tempfile::tempdir().unwrap();
        config(
            root.path(),
            "[[hooks.PostToolUse]]\nmatcher='^open$'\n[[hooks.PostToolUse.hooks]]\ntype='command'\ncommand=\"cat; printf 'test result: FAILED. 1 failed' >&2\"\n",
        );
        let hooks = Hooks::open(root.path()).unwrap();
        let (result, events) = fire(root.path(), &hooks, &AtomicBool::new(false));
        result.unwrap();
        assert!(!events[1].failed);
        assert_eq!(events[1].detail["result"]["status"], "Passed");
        assert!(
            events[1].detail["result"]["output"]
                .as_str()
                .unwrap()
                .contains("$(touch injected)")
        );
        assert!(!root.path().join("injected").exists());
    }
    #[cfg(unix)]
    #[test]
    fn failures_timeouts_cancellation_and_symlinks_are_observable_and_bounded() {
        let root = tempfile::tempdir().unwrap();
        config(
            root.path(),
            "[[hooks.PostToolUse]]\n[[hooks.PostToolUse.hooks]]\ntype='command'\ncommand='exit 1'\n[[hooks.PostToolUse.hooks]]\ntype='command'\ncommand='echo {}'\n",
        );
        let (_, events) = fire(
            root.path(),
            &Hooks::open(root.path()).unwrap(),
            &AtomicBool::new(false),
        );
        assert_eq!(events.len(), 4);
        assert!(events[1].failed && !events[3].failed);
        config(
            root.path(),
            "[[hooks.PostToolUse]]\n[[hooks.PostToolUse.hooks]]\ntype='command'\ncommand='sleep 5'\ntimeout=1\n",
        );
        let started = Instant::now();
        let (_, events) = fire(
            root.path(),
            &Hooks::open(root.path()).unwrap(),
            &AtomicBool::new(false),
        );
        assert!(events[1].failed && started.elapsed() < Duration::from_secs(2));
        config(
            root.path(),
            "[[hooks.PostToolUse]]\n[[hooks.PostToolUse.hooks]]\ntype='command'\ncommand='touch injected'\n",
        );
        let (_, events) = fire(
            root.path(),
            &Hooks::open(root.path()).unwrap(),
            &AtomicBool::new(true),
        );
        assert!(events[1].failed && !root.path().join("injected").exists());
        fs::create_dir_all(root.path().join("src")).unwrap();
        fs::remove_dir_all(root.path().join(".loom/tmp")).unwrap();
        std::os::unix::fs::symlink(root.path().join("src"), root.path().join(".loom/tmp")).unwrap();
        let (_, events) = fire(
            root.path(),
            &Hooks::open(root.path()).unwrap(),
            &AtomicBool::new(false),
        );
        assert!(events[1].failed && !root.path().join("src/injected").exists());
    }
    #[test]
    fn configuration_is_snapshotted_and_global_disable_applies_to_json_too() {
        let root = tempfile::tempdir().unwrap();
        config(
            root.path(),
            "[[hooks.PreToolUse]]\nmatcher='^Bash$'\n[[hooks.PreToolUse.hooks]]\ntype='command'\ncommand='echo {}'\n",
        );
        let hooks = Hooks::open(root.path()).unwrap();
        config(root.path(), "[hooks]\nenabled=false\n");
        fs::write(
            root.path().join(".loom/hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo {}"}]}]}}"#,
        )
        .unwrap();
        assert!(hooks.has(HookEvent::PreToolUse, "run"));
        assert!(!Hooks::open(root.path()).unwrap().has(HookEvent::Stop, ""));
    }
}

struct CommandHook {
    name: String,
    command: String,
    matcher: String,
    timeout_ms: u64,
    context_limit: usize,
}
struct Hook {
    event: HookEvent,
    config: CommandHook,
    matcher: Option<regex::Regex>,
}
#[derive(Default)]
pub struct Hooks {
    entries: Vec<Hook>,
}
#[derive(Default, Debug, Clone)]
pub struct HookOutcome {
    pub context: Vec<String>,
    pub updated_input: Option<Value>,
    pub feedback: Option<String>,
    pub continue_work: bool,
    pub halt: Option<String>,
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
fn event_name(name: &str) -> Result<HookEvent> {
    Ok(match name {
        "PreToolUse" => HookEvent::PreToolUse,
        "PostToolUse" => HookEvent::PostToolUse,
        "UserPromptSubmit" => HookEvent::UserPromptSubmit,
        "Stop" => HookEvent::Stop,
        "Interrupt" => HookEvent::Interrupt,
        "SubagentStart" => HookEvent::SubagentStart,
        "SubagentStop" => HookEvent::SubagentStop,
        "PreCompact" => HookEvent::PreCompact,
        "PostCompact" => HookEvent::PostCompact,
        "SessionStart" => HookEvent::SessionStart,
        "SessionEnd" => HookEvent::SessionEnd,
        other => anyhow::bail!(
            "unsupported hook event {other}; PermissionRequest and MCP/async handlers are not supported"
        ),
    })
}
fn alias(tool: &str) -> &str {
    match tool {
        "run" => "Bash",
        "agent" => "Agent",
        _ => tool,
    }
}
impl Hook {
    fn matches(&self, event: HookEvent, value: &str) -> bool {
        self.event == event
            && (matches!(
                event,
                HookEvent::Stop | HookEvent::UserPromptSubmit | HookEvent::Interrupt
            ) || self
                .matcher
                .as_ref()
                .is_none_or(|m| m.is_match(value) || m.is_match(alias(value))))
    }
}
impl Hooks {
    pub fn end_session(root: &Path, session_id: &str, mut policy: ExecutionPolicy) -> Result<()> {
        let hooks = Self::open(root)?;
        if !hooks.has(HookEvent::SessionEnd, "other") {
            return Ok(());
        }
        policy.sandbox = crate::effects::SandboxMode::ReadOnly;
        let artifacts = FileArtifactStore::open(root)?;
        let events = std::sync::Mutex::new(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(root.join(".loom/hooks-session-events.jsonl"))?,
        );
        hooks.run(Invocation {event:HookEvent::SessionEnd,worker:"main",directory:root,
            input:json!({"session_id":session_id,"reason":"other","workspace":root,"model":"auto"}),policy,
            cancel:&AtomicBool::new(false),remaining:Duration::from_secs(3),observe:&|event| {
                use std::io::Write;
                if let Ok(mut file)=events.lock() { let _=writeln!(file,"{}",serde_json::to_string(&event).unwrap_or_default()); }
            }},&artifacts)?;
        Ok(())
    }

    /// Knowledge is embedded in the binary, not dependent on project documents.
    pub fn guidance(&self) -> String {
        let mut text = include_str!("knowledge/hooks.md").to_owned();
        text.push_str("\nActive hooks for this task:");
        if self.entries.is_empty() {
            text.push_str(" none (disabled or unconfigured).");
        }
        for hook in &self.entries {
            text.push_str(&format!(
                "\n- {} / {} (matcher: {}, timeout: {} ms)",
                hook.event.name(),
                hook.config.name,
                hook.config.matcher,
                hook.config.timeout_ms
            ));
        }
        text
    }
    pub fn open(root: &Path) -> Result<Self> {
        let mut hooks = Self::default();
        let mut disabled = false;
        for (file, is_json) in [
            (root.join(".loom/config.toml"), false),
            (root.join(".loom/hooks.json"), true),
        ] {
            let text = match fs::read_to_string(&file) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e).context("reading hook configuration"),
            };
            ensure!(
                text.len() <= 1024 * 1024,
                "hook configuration exceeds 1 MiB"
            );
            let document: Value = if is_json {
                serde_json::from_str(&text)?
            } else {
                serde_json::to_value(toml::from_str::<toml::Value>(&text)?)?
            };
            let Some(value) = document.get("hooks") else {
                continue;
            };
            let config = value.as_object().context("hooks must be an object")?;
            if !is_json && config.get("enabled") == Some(&json!(false)) {
                disabled = true;
            }
            for (key, groups) in config {
                if key == "enabled" {
                    ensure!(groups.is_boolean(), "hooks.enabled must be boolean");
                    continue;
                }
                let event = event_name(key)?;
                let groups = groups
                    .as_array()
                    .context("hook event must contain an array")?;
                ensure!(groups.len() <= 32, "at most 32 matcher groups per event");
                for (index, group) in groups.iter().enumerate() {
                    let object = group
                        .as_object()
                        .context("hook matcher group must be an object")?;
                    ensure!(
                        object
                            .keys()
                            .all(|key| matches!(key.as_str(), "matcher" | "hooks")),
                        "only Codex-style matcher groups are supported; flat legacy hooks are not supported"
                    );
                    let matcher = group
                        .get("matcher")
                        .map(|v| v.as_str().context("matcher must be a string"))
                        .transpose()?
                        .unwrap_or("*");
                    let handlers = group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .context("matcher group requires hooks array")?;
                    ensure!(handlers.len() <= 32, "at most 32 handlers per group");
                    for (handler_index, handler) in handlers.iter().enumerate() {
                        let object = handler
                            .as_object()
                            .context("hook handler must be an object")?;
                        ensure!(
                            object.keys().all(|key| matches!(
                                key.as_str(),
                                "name"
                                    | "command"
                                    | "type"
                                    | "timeout"
                                    | "command_windows"
                                    | "additionalContextLimit"
                                    | "statusMessage"
                                    | "async"
                            )),
                            "unknown hook handler field (timeout is in seconds; timeout_ms is unsupported)"
                        );
                        ensure!(
                            handler["type"] == "command",
                            "only command hooks are supported; MCP/prompt/agent handlers are unsupported"
                        );
                        ensure!(
                            handler.get("async").is_none_or(|v| v == &json!(false)),
                            "async hooks are unsupported; use synchronous command hooks"
                        );
                        let command = if cfg!(windows) {
                            handler.get("command_windows").or(handler.get("command"))
                        } else {
                            handler.get("command")
                        };
                        let command = command
                            .and_then(Value::as_str)
                            .context("hook command required")?
                            .to_owned();
                        let name = handler
                            .get("name")
                            .map(|v| v.as_str().context("name must be a string"))
                            .transpose()?
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                format!(
                                    "{}:{key}:{index}:{handler_index}",
                                    file.file_name().unwrap().to_string_lossy()
                                )
                            });
                        ensure!(
                            !name.trim().is_empty() && !command.trim().is_empty(),
                            "hook name and command required"
                        );
                        ensure!(
                            !hooks
                                .entries
                                .iter()
                                .any(|h| h.event == event && h.config.name == name),
                            "duplicate hook {name}"
                        );
                        let terminal =
                            matches!(event, HookEvent::Interrupt | HookEvent::SessionEnd);
                        let seconds = handler
                            .get("timeout")
                            .map(|v| v.as_u64().context("timeout must be integer seconds"))
                            .transpose()?
                            .unwrap_or(if terminal { 1 } else { 600 });
                        ensure!(
                            (1..=if terminal { 3 } else { 600 }).contains(&seconds),
                            "hook {name} timeout must be 1..{} seconds",
                            if terminal { 3 } else { 600 }
                        );
                        let compiled = if matcher == "*" || matcher.is_empty() {
                            None
                        } else {
                            Some(regex::Regex::new(matcher).context("invalid regex matcher")?)
                        };
                        let context_limit = handler
                            .get("additionalContextLimit")
                            .map(|v| {
                                v.as_u64()
                                    .context("additionalContextLimit must be a positive integer")
                            })
                            .transpose()?
                            .unwrap_or(2500)
                            .min(16384) as usize
                            * 4;
                        ensure!(
                            context_limit > 0,
                            "Loom requires a bounded positive additionalContextLimit"
                        );
                        hooks.entries.push(Hook {
                            event,
                            config: CommandHook {
                                name,
                                command,
                                matcher: matcher.into(),
                                timeout_ms: seconds * 1000,
                                context_limit,
                            },
                            matcher: compiled,
                        });
                        ensure!(
                            hooks.entries.len() <= 256,
                            "at most 256 command handlers per workspace"
                        );
                    }
                }
            }
        }
        if disabled {
            hooks.entries.clear();
        }
        Ok(hooks)
    }
    pub fn has(&self, event: HookEvent, value: &str) -> bool {
        self.entries.iter().any(|h| h.matches(event, value))
    }
    pub fn run(
        &self,
        invocation: Invocation<'_>,
        artifacts: &FileArtifactStore,
    ) -> Result<HookOutcome> {
        let match_value = invocation.input["tool_name"]
            .as_str()
            .or(invocation.input["source"].as_str())
            .or(invocation.input["trigger"].as_str())
            .or(invocation.input["agent_type"].as_str())
            .or(invocation.input["reason"].as_str())
            .unwrap_or("")
            .to_owned();
        let mut outcome = HookOutcome::default();
        let runner = Runner::new(artifacts.clone());
        let started = Instant::now();
        let mut input = invocation.input.clone();
        input["hook_event_name"] = json!(invocation.event.name());
        input["worker_id"] = json!(invocation.worker);
        input["cwd"] = json!(invocation.directory);
        input["sandbox"] = json!(invocation.policy.sandbox);
        input["network"] = json!(invocation.policy.network);
        input["permission_mode"] = json!(if invocation.policy.sandbox
            == crate::effects::SandboxMode::ReadOnly
        {
            "plan"
        } else {
            "default"
        });
        input["transcript_path"] = json!(null); // Event log is not a stable chat transcript.
        input["session_id"] = input
            .get("session_id")
            .cloned()
            .unwrap_or_else(|| input["run_id"].clone());
        input["turn_id"] = input["task_id"].clone();
        for hook in self
            .entries
            .iter()
            .filter(|h| h.matches(invocation.event, &match_value))
        {
            let target = format!("hook · {} · {}", invocation.event.name(), hook.config.name);
            let mut detail = json!({"tool":"hook","hook_event":invocation.event.name(),"name":hook.config.name,"started":true});
            (invocation.observe)(Event::new(
                invocation.worker,
                Stage::Run,
                &target,
                detail.clone(),
            ));
            let mut payload = input.clone();
            {
                if let Some(tool) = payload["tool_name"].as_str() {
                    let tool = tool.to_owned();
                    payload["loom_tool_name"] = json!(tool);
                    payload["tool_name"] = json!(alias(&tool));
                }
                if let Some(args) = &outcome.updated_input {
                    payload["tool_input"] = args.clone();
                }
            }
            let bytes = serde_json::to_vec(&payload)?;
            ensure!(bytes.len() <= 1024 * 1024, "hook input exceeds 1 MiB");
            let result = runner.run_hook(
                invocation.directory,
                &hook.config.command,
                &bytes,
                Duration::from_millis(hook.config.timeout_ms)
                    .min(invocation.remaining.saturating_sub(started.elapsed())),
                invocation.cancel,
                invocation.policy,
            );
            detail.as_object_mut().unwrap().remove("started");
            detail["completed"] = json!(true);
            let parsed = match result {
                Ok(output) => {
                    detail["result"] = serde_json::to_value(&output.result)?;
                    detail["result"]["output"] =
                        json!(format!("{}{}", output.stdout, output.stderr));
                    if let Ok(value) = serde_json::from_str::<Value>(&output.stdout) {
                        if let Some(message) = value["systemMessage"].as_str() {
                            detail["system_message"] = json!(clip(message, 4096));
                        }
                    }
                    detail["result"]["artifact_target"] =
                        json!(format!("artifact:{}", output.result.artifact.id));
                    if matches!(
                        output.result.status,
                        RunStatus::Cancelled | RunStatus::TimedOut
                    ) {
                        Err(anyhow::anyhow!(
                            "hook {} {:?}",
                            hook.config.name,
                            output.result.status
                        ))
                    } else if output.exit_code == Some(2) {
                        let reason = clip(&output.stderr, 8000);
                        let reason = if reason.trim().is_empty() {
                            format!("blocked by hook {}", hook.config.name)
                        } else {
                            reason
                        };
                        apply_block(invocation.event, reason, &mut outcome);
                        Ok(())
                    } else if output.exit_code != Some(0) {
                        Err(anyhow::anyhow!(
                            "hook {} failed: {}",
                            hook.config.name,
                            clip(&output.stderr, 2048)
                        ))
                    } else {
                        let mut decoded = outcome.clone();
                        decode_output(
                            invocation.event,
                            &output.stdout,
                            hook.config.context_limit,
                            &mut decoded,
                        )
                        .map(|()| {
                            outcome = decoded;
                        })
                    }
                }
                Err(e) => Err(e),
            };
            if let Err(error) = parsed {
                let error = clip(
                    &format!(
                        "hook {}/{}: {error}",
                        invocation.event.name(),
                        hook.config.name
                    ),
                    4096,
                );
                detail["error"] = json!(error);
                // Failure is observable; only explicit policy decisions block calls.
            }
            let mut event = Event::new(invocation.worker, Stage::Run, target, detail);
            event.failed = event.detail.get("error").is_some()
                || outcome.halt.is_some()
                || outcome.continue_work
                || outcome.feedback.is_some()
                || event.detail["result"]["status"] == "Failed";
            (invocation.observe)(event);
            if invocation.event.blocking() && outcome.halt.is_some() {
                break;
            }
            if outcome.halt.is_some() {
                break;
            }
        }
        Ok(outcome)
    }
}
fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
fn apply_block(event: HookEvent, reason: String, outcome: &mut HookOutcome) {
    match event {
        HookEvent::PreToolUse | HookEvent::UserPromptSubmit => outcome.halt = Some(reason),
        HookEvent::PostToolUse => outcome.feedback = Some(reason),
        HookEvent::Stop | HookEvent::SubagentStop => {
            outcome.continue_work = true;
            outcome.feedback = Some(reason);
        }
        _ => {}
    }
}
fn decode_output(
    event: HookEvent,
    stdout: &str,
    limit: usize,
    outcome: &mut HookOutcome,
) -> Result<()> {
    if stdout.trim().is_empty() {
        return Ok(());
    }
    let output: Value = match serde_json::from_str(stdout) {
        Ok(value) => value,
        Err(_) if event == HookEvent::SessionStart && !stdout.trim_start().starts_with('{') => {
            outcome.context.push(clip(stdout, limit));
            return Ok(());
        }
        Err(e) => return Err(e).context("hook stdout must be one JSON object"),
    };
    ensure!(output.is_object(), "hook stdout must be one JSON object");
    let specific = &output["hookSpecificOutput"];
    if let Some(name) = specific["hookEventName"].as_str() {
        ensure!(name == event.name(), "hook output event mismatch");
    }
    if let Some(context) = specific["additionalContext"].as_str() {
        outcome.context.push(clip(context, limit));
    }
    if event == HookEvent::PreToolUse {
        ensure!(
            output.get("continue").is_none()
                && output.get("stopReason").is_none()
                && output.get("suppressOutput").is_none(),
            "unsupported PreToolUse output fields"
        );
        match specific["permissionDecision"].as_str() {
            Some("deny") => apply_block(
                event,
                specific["permissionDecisionReason"]
                    .as_str()
                    .unwrap_or("denied by hook")
                    .into(),
                outcome,
            ),
            Some("allow") => {
                if let Some(args) = specific.get("updatedInput") {
                    ensure!(args.is_object(), "updatedInput must be an object");
                    outcome.updated_input = Some(args.clone());
                }
            }
            None => ensure!(
                specific.get("updatedInput").is_none(),
                "updatedInput requires permissionDecision=allow"
            ),
            _ => anyhow::bail!("unsupported permissionDecision"),
        }
    }
    if output["decision"] == "block" {
        apply_block(
            event,
            output["reason"]
                .as_str()
                .unwrap_or("blocked by hook")
                .into(),
            outcome,
        );
    }
    if output["continue"] == false
        && !matches!(event, HookEvent::PreToolUse | HookEvent::SubagentStart)
    {
        let reason = output["stopReason"]
            .as_str()
            .unwrap_or("stopped by hook")
            .to_owned();
        if event == HookEvent::PostToolUse {
            outcome.feedback = Some(reason);
        } else {
            outcome.halt = Some(reason);
            outcome.continue_work = false;
        }
    }
    Ok(())
}
