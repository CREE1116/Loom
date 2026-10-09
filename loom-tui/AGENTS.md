# Loom TUI — working rules for agents

These rules apply to every change in this crate, whether a person asked for it in a Loom
session or `scripts/loom-selfimprove` is running unattended.

For UI changes, preserve `UI_INVARIANTS.md` and extend its self-check coverage
when introducing another way to represent state, focus or execution progress.

## See what you changed

The terminal UI cannot be judged from code. Look at it:

```sh
cargo run -q -- --self-check            # pass/fail per scenario
cargo run -q -- --self-check --screens  # plus rendered screens; [Tone:text] marks styled runs
cargo run -q -- --self-check --json     # machine-readable
python3 tests/terminal_smoke.py         # real terminal (PTY) in demo mode
```

`--screens` shows the same cells the terminal paints: Strong=bold, Emphasis=italic,
Strike, InlineCode, Math, Accent, Muted/Faint=dim, Selected=active tab. Read the screen
before and after a UI change.

## Every UI change carries a check

- A fix or feature is done when a check proves it: add a scenario or assertion to
  `src/selfcheck.rs` (headless) or `tests/terminal_smoke.py` (real terminal).
- Write the check first and confirm it fails; then make it pass.
- Never delete or weaken a check to get green. If a check is wrong, fix the check and
  say why.

## Before you finish

```sh
cargo build
cargo test -- --test-threads=1
cargo run -q -- --self-check
```

All three must pass. `--test-threads=1` avoids a known flaky file-lock test.

## A running Loom does not see your change

The window you are talking in runs the binary it started with. After a rebuild it shows
"Loom이 다시 빌드되었습니다…"; tell the user to restart (Ctrl+Q, then run again) before
they judge the result. Do not claim a UI change is visible until the binary is rebuilt
and restarted.

## Other sessions may be editing the same files

Several Loom windows and loops can work in this crate at once. Keep changes small and
local, re-read a file right before editing it, and do not reformat files you did not
change (`rustfmt <file>`, not `cargo fmt` over the crate).

## Make it better, not only correct

When everything passes, look at `--screens` as someone who uses Loom all day. Missing
information at the moment it is needed, noise, repetition, what a narrow terminal loses,
click targets and keyboard paths are all fair game. Improvements follow the same rule:
a failing check first, then the change.
