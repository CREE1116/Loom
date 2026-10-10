# Loom

[English](README.md) · [한국어](README.ko.md)

Loom is a terminal coding agent. It works on an isolated copy of your repository,
proposes changes for your approval, and can use remote model providers, local models,
or both. Local models run in-process: GGUF through the bundled llama.cpp, MLX models on
Apple silicon, and Loom's own LMX format.

**Status: early.** Daily use happens on macOS (Apple silicon). Linux and Windows build
and pass the test suite in CI; CUDA builds are not yet verified on hardware.

## Install

Requirements: [Rust](https://rustup.rs), git, CMake and a C/C++ compiler
(Xcode Command Line Tools on macOS, `build-essential` on Linux, Visual Studio Build
Tools on Windows). On Apple silicon, `python3` is used to download the MLX runtime.

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

## Long conversations

When earlier turns no longer fit the model's context (local models report theirs), Loom
summarizes them before the next request and shows "compacting" while it does; `/compact`
does the same on demand.

## Skills

A skill is a folder with a `SKILL.md` (frontmatter `name` and `description`, then
instructions). Loom reads skills from `.loom/skills`, `.agents/skills`, `.codex/skills`
and `.claude/skills` in the repository and in your home directory, and from Claude Code
and Codex plugin caches. Mention one as `$name` in a request or pick it with `/skills`;
the model can also load one when it needs it.

Three skills ship with Loom: `loom-guide` (how Loom itself works and how to fix common
problems), `skill-builder` (writing skills) and `loom-extension` (adding tools).

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
