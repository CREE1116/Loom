# Loom

**Loom — a coding agent runtime that weaves local compute and multiple model sessions into one coherent workflow.**

Loom is building a Core that owns task state, reusable evidence, context, scheduling, costs and patch decisions. LOCAL, SMALL, MEDIUM and FLAGSHIP are compute resources controlled by that Core. The target is lower API cost and latency while maintaining coding quality.

This first milestone delivers the terminal UI, a typed UI/Core boundary and a shared local repository explorer. Remote coding still runs through a transitional Codex adapter. Multiple remote worker sessions, durable task state, isolated patch merging and native OpenRouter routing are planned; they are **not implemented yet**. No measured API savings or baseline quality results are claimed for this milestone.

## Try it

Requirements: a current stable Rust toolchain, Git and ripgrep (`rg`). Interactive remote coding also requires an installed, authenticated Codex CLI; the adapter was checked against `codex-cli 0.151.0`. macOS is locally verified; Linux is included in CI. Opening separate Terminal.app windows is currently macOS only.

```sh
git clone https://github.com/CREE1116/Loom.git
cd Loom

# Explore the UI without calling a model or running coding tools.
./scripts/loom --demo

# Search local source evidence without Codex or API credentials.
./scripts/loom --cwd /path/to/project --explore refresh_session

# Connect to Codex in your project; existing authentication is reused.
./scripts/loom --cwd /path/to/project
```

The launcher builds an optimized release binary. Use `CUSTOM_TUI_PROFILE=debug` for faster development builds. The internal Rust crate and binary currently retain the `custom-tui` name.

## Available now

- A standalone terminal renderer with streaming conversation, Unicode editing, paste, keyboard/mouse navigation, compact layouts and resize support.
- Typed `AgentEvent` / `AgentCommand` contracts. UI state does not parse provider JSON; adapters own protocol and approval payloads.
- Question cards with explicit options, descriptions, custom text, multiple questions, hidden/reopened drafts, secret masking, validation and one-time submission. Blocking questions pause the working animation; nonblocking questions keep it running.
- Working animation during silent execution, approvals, trust/permission controls, queued messages, interruption, session switching/forking and tool/diff review.
- One Core-owned repository explorer: immutable snapshots, SHA-256 provenance, shared concurrent queries and bounded evidence output. Four concurrent consumers of the same query share one search. This is local lexical retrieval, not an AST graph or vector RAG.
- A 64 MiB hot source budget with overflow held in snapshot-owned temporary archives. Eligible files are not dropped because the hot budget is full. Individual files above 512 KiB, binary/invalid UTF-8 files and generated/dependency directories are excluded and reported.

In demo mode, process the example approval and enter `질문 데모` to try the question flow. `/questions` reopens a hidden question; `/explore QUERY` searches shared local evidence. The [TUI guide](custom-tui/README.md) covers commands and runtime behavior.

## Build and verify

```sh
cargo fmt --manifest-path custom-tui/Cargo.toml -- --check
cargo test --locked --manifest-path custom-tui/Cargo.toml
cargo clippy --locked --manifest-path custom-tui/Cargo.toml --all-targets -- -D warnings
cargo build --locked --manifest-path custom-tui/Cargo.toml
python3 custom-tui/tests/terminal_smoke.py
```

The default tests use local fixtures and PTYs; they do not call a model. Desktop window launching and manual profiling are separately ignored. `./scripts/loom --probe` checks a real installed Codex runtime without inference. Tests cover question reply validation, duplicate replies, session scoping, concurrent query reuse, source invalidation, archive overflow and terminal interaction.

## Next milestones

The [TUI completion plan](docs/tui-completion-plan.md) lists the required features, current status and acceptance conditions. After question/input interaction, the order is activity status and dependency explanations, reconnect/error recovery, then session/review improvements and real profiling data.

The [runtime implementation plan](docs/orchestrator-implementation-plan.md) fixes the broader architecture: TaskGraph and durable CanonicalState before routing, then persistent worker sessions, patch isolation, memory/context compilation and cost-aware optimization. Codex Core and Runtime will be replaced incrementally. Shared evidence and deterministic local work are native runtime capabilities, rather than mandatory external skills or gateways.

**Core remembers the world; workers solve problems.**

## License and origins

Apache-2.0. Development began in an OpenAI Codex checkout; this repository contains the independently built UI and initial Core rather than the upstream monorepo. OpenAI attribution and the original upstream notice are retained in [LICENSE](LICENSE) and [licenses/CODEX-NOTICE](licenses/CODEX-NOTICE). The optional Codex executable is installed separately and is not bundled. This project is not affiliated with OpenAI.
