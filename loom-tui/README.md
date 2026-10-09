# loom-tui

The terminal UI and the `loom` executable. See the [repository README](../README.md) for
installation and use.

It draws its own character-cell screen (input, focus, click areas, panels) on top of
Crossterm and talks to `loom-core` through typed `AgentCommand` / `AgentEvent` values.

```sh
cargo run --release -- --cwd /path/to/repository   # from this directory
cargo run -- --demo                                 # fixture data, no model
cargo run -- --self-check [--screens]               # headless UI scenarios
cargo test
```

Rules for UI changes are in [AGENTS.md](AGENTS.md) and [UI_INVARIANTS.md](UI_INVARIANTS.md).
