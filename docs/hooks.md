# Command hooks

Loom supports synchronous command hooks with Codex-style matcher groups and JSON
input/output. Configuration is workspace-local and snapshotted before each task.
The agent receives built-in hook instructions and the active hook list automatically.

Create `.loom/hooks.json`:

```json
{"hooks":{"PreToolUse":[{"matcher":"^Bash$","hooks":[{
  "type":"command","command":"python3 scripts/hooks/policy.py","timeout":10
}]}]}}
```

Equivalent `.loom/config.toml`:

```toml
[[hooks.PreToolUse]]
matcher = "^Bash$"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "python3 scripts/hooks/policy.py"
timeout = 10
```

Both files are combined, TOML first. `[hooks] enabled = false` in TOML disables
all hooks. Flat named handlers and `TaskStart`/`TaskEnd` are rejected.
Put scripts in ordinary project directories: isolated workspaces exclude `.loom`.

## Events and decisions

| Event | Timing | Exit 2 / blocking output |
| --- | --- | --- |
| SessionStart | Before the first submitted turn after loading a session; source `startup` or `resume` | Notification |
| UserPromptSubmit | Before the main worker calls the model | Rejects the prompt |
| PreToolUse | Before a real tool call | Prevents execution |
| PostToolUse | After a real tool call, including failures | Sends feedback; never replays the tool |
| Stop | Before a normal main-worker answer or finish completes | Requests another model step |
| SubagentStart | Before a delegated worker starts | Notification |
| SubagentStop | Before a delegated worker normally finishes | Requests another worker step |
| PreCompact / PostCompact | Around worker compaction and automatic/manual conversation summary | Notification |
| Interrupt | After a task is cancelled | Notification |
| SessionEnd | Actual backend shutdown, once | Notification |

Matchers are regular expressions. Missing, empty or `*` matches everything.
Tool matchers accept Loom names and aliases: `run` → `Bash`, `agent` → `Agent`.
Other tools keep their names and arguments; `change` is not Codex `apply_patch`.
Session events match `source`/`reason`, subagent events match `agent_type`, and
compaction events match `trigger`. Stop, prompt and interrupt ignore matchers.

Commands receive one JSON object on stdin, including `hook_event_name`, `session_id`,
`turn_id`, `cwd`, `worker_id`, `model`, `permission_mode`, `sandbox`, and `network`.
Tool events include `tool_name`, `loom_tool_name`, `tool_input`, and `tool_use_id`;
PostToolUse also includes `tool_response` and `tool_failed`.
Lifecycle events include `prompt`, `source`, `reason`, `trigger`, `agent_id`,
`agent_type`, or `last_assistant_message` as applicable. `transcript_path` is null.

Exit 0 succeeds. Exit 2 uses stderr as explicit rejection/feedback for the events
listed above. Other exit codes, invalid JSON, timeouts and launch failures are
reported as hook failures without silently denying a tool. Output and artifacts
remain available for diagnosis. Shell commands come only from configuration;
tool arguments are JSON data on stdin, never interpolated into shell source.

Stdout should contain one JSON object. SessionStart also accepts plain context.
A policy can rewrite a command:

```json
{"hookSpecificOutput":{
  "hookEventName":"PreToolUse",
  "permissionDecision":"allow",
  "updatedInput":{"command":"cargo test"},
  "additionalContext":"Run project checks before reporting success."
}}
```

`permissionDecision: "deny"` with `permissionDecisionReason` blocks a tool.
`updatedInput` requires `allow` and an object; subsequent hooks see updated arguments.
An allow decision cannot grant sandbox or network permissions.
`hookSpecificOutput.additionalContext` becomes model context.

Stop/SubagentStop can return `{"decision":"block","reason":"Check the result"}`
to continue. `stop_hook_active` identifies a continuation. Loom permits at most
three continuations and still enforces task step/time limits.
`continue: false` with `stopReason` ends the worker. PostToolUse blocking feedback
replaces the model-visible result with a reminder that the tool already ran.

## Limits

Only synchronous `type: "command"` handlers are supported. PermissionRequest,
async, MCP, prompt and agent handlers fail configuration explicitly.
This is a supported subset of Codex hooks, not full compatibility.

`timeout` uses integer seconds: 1–600, default 600. SessionEnd/Interrupt default
to 1 second, at most 3, with a shared 3-second event budget. Task deadlines may
shorten these limits. Cancellation terminates hook process groups.
`additionalContextLimit` defaults to 2500; Loom bounds positive values to 16384
approximate tokens (four characters each). Unlimited context is unsupported.
Diagnostic streams are independently bounded to 64 KiB.

Start/tool hooks use the isolated worker copy. Delegated lifecycle hooks and
terminal notifications run read-only; they cannot change an applied result.
Hooks never recursively invoke tool hooks. SessionEnd diagnostics are appended
to `.loom/hooks-session-events.jsonl`. Configuration edits affect the next task.
