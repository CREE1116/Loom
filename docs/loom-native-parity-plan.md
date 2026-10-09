# Native parity implementation

User scope: all gaps enumerated on 2026-10-02. Preserve independent Core and verified
application; do not route Native work back through the Codex worker.

Acceptance checklist:
- [x] durable multi-turn sessions; previous turns actually reach the model — `sessions_gate::sessions_pass_previous_answers_and_project_guidance_to_the_next_model_after_restart`
- [x] hierarchical AGENTS.md, explicit/discovered SKILL.md, scoped instruction loading — `instructions_gate` (hierarchy, scope, skills); scoped guidance enforced before edits in `worker/tools.rs`
- [x] real PTY exec/write_stdin and child cleanup — `terminal_gate` (interactive PTY, bounded output, timeout/cancel/drop kill descendants)
- [x] MCP stdio/HTTP and bounded web search/fetch with attribution — `extensions_gate` (16)
- [x] image inputs over every supported model protocol — wire bodies for Chat, Responses, Anthropic, Ollama in `models_stream_gate`; TUI attachments reach Native (`backend/native.rs`)
- [x] actual provider streaming projected to TUI — `models_stream_gate` (4 protocols); `EntryDelta` projection in `backend/native.rs`
- [x] manual/automatic context compaction with retrievable original evidence — `Sessions::compact` (manual, `/compression`), `worker::compact_messages` (automatic at the context limit, originals as artifacts)
- [x] sessions create/switch/fork and guarded code+conversation rewind — `sessions_gate::accepted_turns_can_fork_and_rewind_code_without_overwriting_unrelated_user_changes`
- [x] child agents spawn/send/wait/interrupt with isolated edits and conflict-safe merge — `worker/agents.rs` (`spawn_agent`/`send_agent`/`wait_agent`/`interrupt_agent`/`merge_agent`), `worker_gate`
- [x] persistent goal lifecycle and structured plan tool — `sessions.rs` Goal/PlanStep, `worker_gate::plan_and_live_terminal_tools_work_in_the_worker_loop`
- [x] OS sandbox, network policy, trust and approval settings — `effects/sandbox.rs` + `terminal_gate` sandbox tests; Native settings persist approval/trust/policy
- [x] actual model profiles, effort propagation and observed quota/health — `reasoning_profiles_are_explicit_and_unknown_models_stay_unknown`, `router_falls_back_only_before_events_and_exposes_real_health`; OmniRoute account quotas in `models/quota.rs`
- [x] expose Core rename and symbol editing tools to Worker — `rename_symbol`, `replace_symbol`, `insert_after_symbol` in `worker/tools.rs`; `effects_gate::rename_preserves_comments_and_literals_across_files`
- [x] CLI/TUI integration and regression gates, documentation — `cli_gate`, `custom-tui/tests/native_backend.rs`, PTY suite 27/27, `loom-core/README.md`

Implementation ownership:
- main: instructions, sessions, control/worker/CLI/Native integration, agents/plans/goals
- Socrates: model protocol streaming/images/effort/profiles
- Sagan: terminal manager and platform sandbox
- Planck: MCP/web extensions

Completion requires Core tests+Clippy, TUI build+serial tests+self-check/screens, PTY.
Feature tests use deterministic models/mock servers; real-model quality remains a
separate measured gate and is not inferred from functionality tests.

## Status (2026-10-02 22:50)

All functional items are implemented behind deterministic gates:
loom-core tests pass serially and Clippy is clean; custom-tui build, serial tests,
self-check (362) and the PTY suite (27) pass.

Fixed while closing out: `examples/model_probe.rs` lacked the new `Request` fields
(`reasoning_effort`, `cancel`). The PTY Native-shutdown fixture wrote its pid outside
the repository, which the new workspace-write sandbox correctly denies during
validation (validation failed and the scripted model ran out); the test now opts
into `--native-sandbox full-access`, since it checks cancellation, not the sandbox.

Open, by design not inferred from functionality tests:
- real-model quality: the L4 baseline gate (engine bench with free models equal to
  the previous engine) has not passed;
- the L5 efficiency gate (remote input −50%, pass drop ≤2) is unmeasured, so
  `--with-context` stays opt-in and the Split scheduler is not built.
- `extensions_gate::search_and_fetch_…` can fail under a fully parallel `cargo test`
  (mock HTTP server under load); it passes alone and with `--test-threads=1`.
