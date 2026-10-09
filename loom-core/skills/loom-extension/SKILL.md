---
name: loom-extension
description: Give Loom new tools through .loom/config.toml - MCP servers (stdio command or HTTP URL) and web search (SearxNG or Brave). Use when the user wants Loom to call an external service, API or local tool.
---
<!-- Bundled with Loom. Loom updates this file until you edit it. -->

# Adding tools to Loom

Loom reads `<workspace>/.loom/config.toml` when a task starts. Tools it declares appear
to the model next to Loom's own tools; no rebuild or restart is needed.

## MCP servers

```toml
[mcp_servers.github]            # the table name identifies the server
command = "github-mcp-server"   # stdio: a program speaking MCP on stdin/stdout
args = ["stdio"]
env = { GITHUB_TOKEN = "..." }  # only these variables are passed; nothing is inherited

[mcp_servers.docs]
url = "https://example.com/mcp" # HTTP transport instead of command
headers = { Authorization = "Bearer ..." }
# enabled = false               # keep the entry but do not start it
```

- Tool names come from the server. A read-only task only sees tools the server marks
  read-only (`readOnlyHint`); others are hidden there.
- Network access follows the task's execution policy; a stdio server runs inside the same
  sandbox as Loom's commands, in the task's isolated copy of the workspace.
- Credentials are used only where written (`env`, `headers`). Never commit real secrets:
  keep `.loom/config.toml` out of version control when it holds them.

## Web search

```toml
[web_search]
provider = "searxng"                      # or "brave" (needs api_key or headers)
endpoint = "https://search.example/search"
# format = "html"                         # SearxNG without the JSON API
# api_key = "..."
```

## Steps

1. Ask which service or program the user wants and how it authenticates.
2. Add or edit the table in `.loom/config.toml` with the file tool. Keep existing entries.
3. Verify in the next task: the server's tools are listed, and one harmless tool works.
   When a server fails to start or to list its tools, an `extensions_diagnostics` tool
   appears instead; call it for the reason.
4. Report the tool names the server added and anything still missing (a token, a binary).
