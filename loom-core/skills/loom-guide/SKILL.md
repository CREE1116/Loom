---
name: loom-guide
description: How Loom itself works - files, commands, models, skills, extensions - and how to diagnose and fix Loom problems. Load it for any question or problem about Loom rather than about the user's code.
---
<!-- Bundled with Loom. Loom updates this file until you edit it. -->

# Loom guide

You are running inside Loom, a terminal coding agent. Use this guide when the user asks
about Loom or when something in Loom does not work (a model, a skill, an extension, a
setting). Answer from facts you checked, not from memory of other tools.

## What you can and cannot touch

- Your file tools work on the workspace (an isolated copy; changes are proposed to the
  user for approval).
- Loom's own files live outside the workspace (`~/.loom`, `~/.agents/skills`, ...). You can
  read them with the `run` tool (`cat`, `ls`, `head`). You cannot write there: give the
  user the exact command or file content to apply, and say why.
- Never print or copy secrets from `keys.toml`, `provider-credentials.json` or extension
  headers; refer to them by name.

## Where things are

| What | Where |
| --- | --- |
| Models | `~/.loom/models/` (`$LOOM_MODELS` overrides). GGUF files under `<owner>/<repo>/` |
| LMX models | `~/.loom/models/lmx/<id>.lmx` (v1), `~/.loom/models/lmx-native/*.lmx` (v2, Loom's engine) |
| Saved prompt prefixes | `~/.loom/models/lmx-native/.prefix/` (safe to delete; rebuilt on use) |
| Providers, routes, keys | `~/.loom/providers.toml`, `routes.toml`, `keys.toml`, `provider-credentials.json` |
| Request log | `~/.loom/model-usage.jsonl` (one line per model call: model, ok, error, tokens, latency) |
| Skills | `~/.loom/skills`, `~/.agents/skills`, `~/.codex/skills`, `~/.claude/skills`, the same under the workspace, and Claude Code/Codex plugin caches |
| Extensions (MCP, web search) | `<workspace>/.loom/config.toml` |
| Workspace state | `<workspace>/.loom/` (`tasks/`, `sessions/`, `overlays/`, `target/`) |
| UI settings | `<workspace>/.custom-tui/settings.json` |

## Commands

In the app: `/model` (choose model), `/runtime` (download, convert to LMX, remove local
models, inference device), `/skills`, `/provider`, `/routing`, `/doctor` (environment and
local model checks), `/settings`, `/status`, `/cost`, `/context`, `/work`, `/diff`,
`/approvals`, `/compact`, `/new`, `/resume`, `/help`.

From a shell (`loom` is the installed binary; in the source tree `scripts/loom`):
`loom models` (everything the router can use), `loom model import <file.gguf>`
(LMX v1), `loom model import <file.gguf> --native --output <file.lmx>` (LMX v2),
`loom model inspect <path|lmx/ID> --verify`, `loom model bench <ID> --prompt N --gen N`,
`loom model remove <ID>`.

## Models

- A downloaded GGUF is converted to LMX when "받은 GGUF를 LMX로 변환" is on in `/runtime`;
  "변환 후 원본 GGUF 보관" decides whether the GGUF stays. Each model is listed once.
- LMX v2 files run on Loom's own engine (Qwen3.5/3.6 and LFM2/LFM2.5, dense or MoE); others run
  through the bundled llama.cpp (Metal or CUDA when present) or MLX.
- Native engine settings are automatic; overrides: `LOOM_LMX_EXPERT_MB`, `LOOM_LMX_CONTEXT`,
  `LOOM_LMX_KV` (`q8_0`, `q4_0`, `f16`), `LOOM_LMX_GPU=0`.

## Skills

A skill is `<skills folder>/<name>/SKILL.md` with frontmatter `name:` and `description:`
(see the `skill-builder` skill). The user calls one with `$name` or `/skills`; you load one
with `use_skill`. Loom lists skills at the start of each task.

## Troubleshooting

1. Find the evidence first: the last lines of `~/.loom/model-usage.jsonl` for model
   errors; `/doctor` output; the file in question.
2. Common causes:
   - Model missing from `/model`: the file is not under `~/.loom/models`, the conversion
     failed (its GGUF is still there), or no engine supports its architecture.
   - "모델이 요청을 받아들이지 못했습니다" (BadRequest): read the matching error line in
     `model-usage.jsonl`; context too long, unsupported model, or a provider rejection.
   - Skill not offered: wrong folder depth (must be `<folder>/<name>/SKILL.md`), missing
     or malformed frontmatter, another skill with the same name earlier in the search
     order (workspace before home), or the file is over 64 KB.
   - Extension tools missing: `.loom/config.toml` syntax, the server command not on PATH,
     a missing token; an `extensions_diagnostics` tool appears with the reason.
   - Slow first answer on a local model: the model is still loading or warming up;
     later turns reuse cached prompt state.
3. Fix what is in the workspace with your tools; for anything outside, give the user the
   exact command or content and how to verify it.
