# Command hooks

Loom Native supports opt-in command hooks in the workspace's `.loom/config.toml`.
Without `hooks.enabled = true`, no hook commands run. Configuration is loaded once
at the start of each task; changes take effect on the next task.

```toml
[hooks]
enabled = true

[[hooks.PreToolUse]]
name = "protect-deletes"
matcher = "change"
command = "python3 scripts/hooks/protect_deletes.py"
timeout_ms = 1000

[[hooks.PostToolUse]]
name = "whitespace-check"
matcher = "change|run"
command = "git diff --check"
timeout_ms = 5000

[[hooks.TaskEnd]]
name = "result"
command = "python3 scripts/hooks/result.py"
timeout_ms = 1000
```

Create the referenced scripts in your project before enabling this example. On
Windows, use `python` if that is the installed Python command. Existing MCP and
web configuration can remain in the same file.

## Events

| Event | When | Failure behavior |
| --- | --- | --- |
| `TaskStart` | Once, after creating the isolated workspace and before the model runs | Stops startup; the task is unresolved |
| `PreToolUse` | Before each matching available tool, including MCP tools | Blocks the tool and returns the reason to the model |
| `PostToolUse` | After the tool returns, including a tool failure | Records the hook failure and tells the model the tool already ran; no automatic replay or rollback |
| `TaskEnd` | Once when the turn produces its final task result, including errors and cancellation | Records a notification failure without changing the task's result status |

Matching hooks run in configuration order. `matcher` defaults to `*`; tool hooks
accept glob patterns or alternatives such as `change|run|mcp_*`. Names are exact
and case-sensitive. Task events do not use tool matchers. Each hook needs a unique
`name` within its event, a `command`, and an optional `timeout_ms` (default 10000,
range 1–60000). At most 32 hooks can be configured per event.

`TaskEnd` reports statuses such as `Answered`, `Unresolved`, `Cancelled`,
`AwaitingApproval` or `Applied`. A later manual approval does not run the hook
again. Hook commands execute directly through the runtime, without model calls
or recursive tool hooks. A nonzero exit code blocks `TaskStart`/`PreToolUse`;
timeouts, cancellation and launch failures also count as hook failures.

## JSON input and output

Every command receives JSON on stdin. Command source is never constructed from
the prompt, tool arguments or tool output. For example:

```json
{
  "hook_event_name": "PreToolUse",
  "task_id": "task-...",
  "run_id": "...",
  "worker_id": "main",
  "workspace": "/path/to/project",
  "cwd": "/path/to/isolated-workspace",
  "sandbox": "WorkspaceWrite",
  "network": false,
  "tool_name": "change",
  "tool_input": {"op": "delete", "target": "src/example.py"}
}
```

`PostToolUse` additionally receives `tool_response` and `tool_failed`. Task events
receive `prompt`; `TaskEnd` also receives `status` and `summary`.

An example `scripts/hooks/protect_deletes.py`:

```python
import json
import sys

event = json.load(sys.stdin.buffer)
if event.get("tool_input", {}).get("op") in {"delete", "move"}:
    print("File deletion and moves are disabled by protect-deletes.", file=sys.stderr)
    sys.exit(2)
```

An example `scripts/hooks/result.py`:

```python
import json
import sys

event = json.load(sys.stdin.buffer)
print(f"{event['task_id']}: {event['status']}")
```

Stdout and stderr are recorded as artifacts, with at most 64 KiB from each stream.
Successful output is not injected as model instructions. Failed blocking hooks
return a short diagnostic to the model. Hook names, execution state and failures
appear in the terminal/browser activity trace and saved task events.

## Workspaces, permissions and stopping

Start and tool hooks run in the isolated working copy. Put scripts in ordinary
project paths, such as `scripts/hooks/`; `.loom` is runtime metadata and its
contents are not copied into the isolated workspace. Formatter changes become
part of the proposal, refresh the code index, and pass normal validation and
approval before affecting the real workspace.

Hooks inherit the task's network and filesystem policy. Read-only tasks and
scoped child workers execute hooks with read-only filesystem access. For
formatting delegated changes, match the parent's `agent` tool instead of a
child's `change` tool. Hooks cannot grant additional execution permissions.
If Loom itself is already inside a macOS sandbox, a read-only hook is rejected
when the runtime cannot enforce that narrower policy.

`TaskEnd` runs in the real workspace with read-only filesystem access, so a
notification cannot invalidate a validated or applied result. It has one shared
three-second budget across all matching end hooks, including on cancellation.
Other hooks share the task deadline; stopping the task terminates running hook
processes and their descendants. Set `enabled = false` to disable hooks on the
next task.
