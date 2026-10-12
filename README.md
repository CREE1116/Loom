# Loom

[English](README.md) · [한국어](README.ko.md)

Loom is a terminal coding agent. It works on an isolated copy of your repository,
proposes changes for your approval, and can use remote model providers, local models,
or both. Local models run in-process: GGUF through the bundled llama.cpp, MLX models on
Apple silicon, and Loom's own LMX format.

**Status: early.** Daily use happens on macOS (Apple silicon). Linux and Windows build
and pass the test suite in CI; CUDA builds are not yet verified on hardware.

## Install

One command (Apple silicon Macs and Linux x86_64 get the prebuilt release; other systems build from
source with the requirements below):

```sh
curl -fsSL https://raw.githubusercontent.com/CREE1116/Loom/main/scripts/get.sh | sh
```

Options: `LOOM_PREFIX` (default `~/.local`), `LOOM_VERSION`, `LOOM_FROM_SOURCE=1`,
`LOOM_CUDA=1`. Run it again to update. Linux binaries require glibc 2.39 or newer;
older distributions can use `LOOM_FROM_SOURCE=1`.

Windows x86_64: download [get.ps1](https://raw.githubusercontent.com/CREE1116/Loom/main/scripts/get.ps1)
and run `powershell -File .\get.ps1`. Close existing Loom windows before reinstalling.

Installed releases check for updates in the background once a day. Downloads are
verified with SHA-256 before installation. Restart Loom after the update notice;
Windows applies the verified update after Loom exits. Use `loom update` to update
immediately, `loom update --check` to check without installing, or set
`LOOM_AUTO_UPDATE=0` to disable automatic updates. Development builds outside
`PREFIX/bin`, custom CUDA builds, and unsupported architectures are not replaced. Existing versions
before v0.1.3 need one manual reinstall to receive the updater.

### From a checkout

Requirements: [Rust](https://rustup.rs), git, CMake and a C/C++ compiler
(Xcode Command Line Tools on macOS, `build-essential` on Linux, Visual Studio Build
Tools on Windows). On Apple silicon, `python3` is used to download the MLX runtime.
On Linux, Loom runs commands inside [bubblewrap](https://github.com/containers/bubblewrap)
(`apt install bubblewrap`).

```sh
git clone https://github.com/CREE1116/Loom.git
cd Loom
scripts/install.sh            # installs ~/.local/bin/loom
```

Options: `--prefix DIR`, `--cuda` (NVIDIA, needs the CUDA toolkit), `--no-mlx`.

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install.ps1   # -Prefix DIR, -Cuda
```

The first build compiles llama.cpp and takes several minutes. Your data — models,
provider settings, keys — lives in `~/.loom` and is kept when Loom is reinstalled or
removed. To uninstall, delete the `loom` executable (and `~/.local/lib/loom` on macOS).

## Quick start

```sh
loom --cwd /path/to/your/repository
loom --demo                   # try the interface with fixture data, no model needed
```

Inside Loom:

- `/provider` — connect a provider (OpenAI, Anthropic, Gemini, Groq, OpenRouter,
  DeepSeek, any OpenAI-compatible server) or sign in with ChatGPT.
- `/model` — choose a model, or keep **auto** and let Loom pick by cost and
  availability within the limit set in `/routing`.
- `/runtime` — download a local model (`hf:owner/repo` or a GGUF link), choose the
  inference device, remove models.
- `/help` lists every command.

Type a request. Loom reads the code, makes changes in its isolated copy, runs checks,
and shows the diff; nothing touches your working tree until you approve.

## Command environment

Task commands write inside the isolated workspace by default, with network access
turned off. Start tasks that need package downloads with:

```sh
loom --native-network --cwd /path/to/project
```

Restricted commands use a writable temporary `uv` cache unless a cache location is
explicitly supplied. With network access disabled, `UV_OFFLINE=1` avoids repeated
network resolution attempts. Global Python packages and a `uv` virtual environment
are separate; `uv add --frozen` alone does not prove installation. Standalone Python
file changes without configured project checks receive an independent syntax check.
This checks syntax, not successful execution or model accuracy.

## Local models

- Downloaded GGUF files are converted to **LMX**, Loom's single-file model format, by
  default. `/runtime` has two switches: convert after download, and keep the original
  GGUF. Each model appears once in `/model`.
- Models the native engine supports run on Loom's own engine: Qwen3.5/3.6 (dense and
  MoE) and LFM2/LFM2.5 (dense and MoE) GGUFs. For mixture-of-experts models only the
  experts a token uses are loaded, into a memory budget chosen from the machine's RAM and
  returned to the system under memory pressure. Other models run through llama.cpp
  (Metal, CUDA or CPU) or MLX.
- The prompt prefix shared by every session (system prompt and tool definitions) is
  cached on disk, so a new session's first answer does not recompute it.

Command-line equivalents:

```sh
loom model list
loom model pull hf:owner/repo[/file.gguf]
loom model import model.gguf                       # LMX file that wraps the GGUF
loom model import model.gguf --native --output m.lmx   # native LMX (v2)
loom model inspect lmx/<id> --verify
loom model bench <id> --prompt 512 --gen 64
```

## Remote control

Type `/remote` and scan the QR code to control the open Loom conversation in a phone
browser. With `cloudflared` installed, Loom prepares a public HTTPS address; until it
is ready, or if it fails, the local network address remains available. Use
`/remote local` for local-only HTTP and `/remote off` to close the server and tunnel.
Closing the QR window keeps remote control on; `/remote` opens it again.

The page shows recent conversation entries, grouped tool activity, approvals and
questions. Send or queue messages, stop work, answer choices or custom questions,
choose a model and reasoning level, compact, or start a new conversation. Open the
queue to send an item immediately, edit its model/reasoning options, remove it, or
resume paused work. Immediate sends wait for interruption acknowledgement; queue
option edits affect only the selected message. Failed
sends stay recoverable; they are never retried automatically. Local drafts, images
and selected skills stay on the computer. The page can be added to a phone's home
screen; it requires a live connection to Loom. Model failures explain how to recover;
the terminal and phone offer an explicit continuation button that asks the model to
check existing work before continuing.

Conversation and control APIs require the random token in the QR address. Anyone
with that address can control the session. Local HTTP is unencrypted: use a trusted
network or VPN. Restarting remote control changes the token (and the quick-tunnel
address); scan the new QR code. See [remote verification](docs/loom-remote-release-2026-10-10.md).

## Long conversations

When earlier turns no longer fit the model's context (local models report theirs), Loom
summarizes them before the next request and shows "compacting" while it does; `/compact`
does the same on demand.

## Hooks

Native command hooks can validate or block tool calls, run checks or formatters
after tools, and handle session, prompt, stop, subagent and compaction events. Configure Codex-style command groups in `.loom/hooks.json` or `[hooks]` in
`.loom/config.toml`, use JSON stdin, and follow the task's execution policy.
See [hook configuration and examples](docs/hooks.md).

## Skills

A skill is a folder with a `SKILL.md` (frontmatter `name` and `description`, then
instructions). Loom reads skills from `.loom/skills`, `.agents/skills`, `.codex/skills`
and `.claude/skills` in the repository and in your home directory. Set `LOOM_PLUGIN_SKILLS`
to also include Claude Code and Codex plugin caches. Mention skills anywhere in a request, for example
`$loom-review $loom-debug investigate this change`. `$` completion follows the cursor;
selecting a suggestion adds a skill without erasing the surrounding draft. Multiple
skills apply together, in selection/invocation order, with duplicates removed. Names
match exactly (ignoring ASCII case); escaped mentions and mentions in code stay literal.
You can also invoke `/loom-review investigate this change`, or pick skills with `/skills`.
The picker stays open while you add or remove skills; press Esc to return to your draft.
Built-in slash commands take priority over skill names. The model can load each relevant
skill as needed; independent delegated work follows the user's authorization.

Six skills ship with Loom: `loom-guide` (how Loom itself works and how to fix common
problems), `skill-builder` (writing skills), `loom-extension` (adding tools),
`loom-plan` (implementation planning), `loom-debug` (reproduction and diagnosis),
and `loom-review` (evidence-based code review). Press `Ctrl+P` to search commands
by name or description while keeping your message draft.

## Extensions

Add tools through `<repository>/.loom/config.toml`: MCP servers (stdio command or HTTP
URL) and web search (SearxNG or Brave).

```toml
[mcp_servers.github]
command = "github-mcp-server"
args = ["stdio"]
env = { GITHUB_TOKEN = "..." }

[web_search]
provider = "searxng"
endpoint = "https://search.example/search"
```

## Where things are

| | |
| --- | --- |
| Models | `~/.loom/models/` (`$LOOM_MODELS` overrides) |
| Providers and keys | `~/.loom/providers.toml`, `~/.loom/provider-credentials.json` (mode 0600) |
| Request log | `~/.loom/model-usage.jsonl` |
| Per-repository state | `<repository>/.loom/`, UI settings in `<repository>/.custom-tui/` |

## Repository layout

| Path | |
| --- | --- |
| `loom-core/` | Agent loop, tools, workspace isolation, routing, providers, local inference, LMX |
| `loom-core/native/lmx/` | Native LMX engine (C++ on ggml) |
| `loom-tui/` | Terminal UI and the `loom` binary |
| `scripts/` | Install, packaging and benchmark tools |
| `docs/` | Design notes and measurements |

## Development

```sh
cargo test --manifest-path loom-core/Cargo.toml
cargo test --manifest-path loom-tui/Cargo.toml
cargo run --manifest-path loom-tui/Cargo.toml -- --self-check   # headless UI scenarios
scripts/loom --cwd .                                              # run from source
```

UI changes need a self-check scenario; see `loom-tui/AGENTS.md`.

## License

Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Development began in
an OpenAI Codex source checkout; Loom does not depend on or bundle Codex.
