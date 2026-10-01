# Loom implementation rules

Read docs/orchestrator-implementation-plan.md and docs/tui-completion-plan.md for ownership, milestones and acceptance criteria.

- Core owns state, evidence, scheduling, context, costs and patch decisions. Workers return results and proposals.
- UI consumes typed AgentEvent and emits AgentCommand. Keep provider wire formats in backend adapters.
- Reuse shared local evidence and immutable repository snapshots. Do not broadcast identical exploration to remote workers.
- Preserve drafts, explicit user choice, opaque approval tokens and session scope. Never auto-submit a question default.
- Mark transitional Codex dependencies and planned features honestly. Do not display synthetic cost savings as measurements.
- Implement complete, focused product slices in TUI plan order; do not start with a router.

Checks (no remote inference):

    cargo fmt --manifest-path custom-tui/Cargo.toml -- --check
    cargo test --locked --manifest-path custom-tui/Cargo.toml
    cargo clippy --locked --manifest-path custom-tui/Cargo.toml --all-targets -- -D warnings
    cargo build --locked --manifest-path custom-tui/Cargo.toml
    python3 custom-tui/tests/terminal_smoke.py
