# Repository notes for agents

- `loom-core/` is the Core: agent loop, tools, workspace isolation, model routing and
  local inference (including the native LMX engine in `loom-core/native/lmx`).
- `loom-tui/` is the terminal UI and the `loom` binary. Before changing anything there,
  read `loom-tui/AGENTS.md`: every UI change needs a self-check
  (`cargo run --manifest-path loom-tui/Cargo.toml -- --self-check`), and a running Loom
  window does not show a change until it is restarted.
- `docs/` holds design notes and measurements; `bench/` holds benchmark fixtures used by
  the `scripts/loom-*` tools.
- Run `cargo test` for both crates before proposing a change.
