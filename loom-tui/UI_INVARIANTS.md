# Loom UI invariants

Keep the text, lines, whitespace and restrained color palette. Every self-improvement
pass must preserve the rules below; add a self-check when a new screen introduces
another instance of one of these states.

- **Current state:** a small, persistent indicator such as `●` for the connected
  session or active tab. It must remain distinguishable from keyboard selection.
- **Keyboard selection:** `Tone::Selected` belongs to the current actionable
  focus or cursor. Hover must not overwrite it. Current tabs use `Tone::Accent`.
- **Execution and alerts:** working state is Accent; completed state is Success;
  failed state is Danger. Warning is for conditions requiring attention (limits,
  blocked actions, pending approvals), not neutral settings or an ordinary queue.
- **Emergency stop:** Esc sends an interrupt immediately during an active turn,
  including before the runtime returns a turn ID and while menus, text selection,
  questions or approvals are visible. Preserve the draft; send no duplicate
  interrupt while one is pending. Without an active turn, Esc navigates normally.
  Replace the working phrase with an explicit stopping indicator until the
  runtime confirms interruption or reports a failure.
- **Accent presets:** changing the accent also changes execution, alert and selection
  colors immediately. Keep the semantic tones and status symbols, distinguish progress,
  success, warning and danger, and preserve the difference between the current setting
  (`●`, Accent) and keyboard focus (Selected). Save the palette with workspace settings.
- **Settings:** group preferences by purpose. Inline choices each keep their own
  click and keyboard target, wrap in narrow panes, and scroll into view on selection.
  Changing a display preference invalidates cached transcript layout. Paste confirmation
  and reset confirmation preserve the draft and session until the user confirms.
  At wide widths, settings retains its side pane independently of overview visibility.
  Opening another tab never rewrites the saved overview visibility, and returning to
  Chat restores that choice. Hidden overview takes precedence over startup expansion.
- **Responsiveness:** recording code checkpoints runs on a serial background worker.
  Git cannot block terminal input or Ctrl+Q. Rewind waits for pending records; shutdown
  cancels the worker's private Git process without changing the user's Git index.
- **Conversation backgrounds:** alternate `#12181e` and `#141a20` by conversation
  turn. A user request, assistant commentary, tool output and final reply share a
  band across the full transcript pane, including gutters. Scrolling and wrapping
  preserve the band's order. Code, quotes, selection and adjacent panels retain
  their own backgrounds.
- **Overview information:** show observed context use, reported quota, queue and
  agent counts first. Gauges are thin text bars. Exact token, cache, reasoning and
  quota windows belong behind the `/usage` detail action. The footer shows only
  the numeric cumulative token count and reported remaining quota; speaker dividers
  use color without text labels. Missing observations
  display `—`; never derive a precise value from cumulative token totals.
  In `/usage`, keep context separate from cumulative tokens, show cache beneath input
  and reasoning beneath output (both are included in their parent counts), and align
  related values at readable widths. Native runtimes with no context observation
  must display an unreported state even when cumulative token counts are available.
- **Sessions:** show connected `●` separately from available `○`. Other session
  rows remain keyboard- and mouse-selectable. Time and status share the title row
  when space allows; narrow panes put metadata beneath the row and preserve its
  click target. Color execution status even when the row is focused.
- **Navigation:** keep Chat, Work, Sessions, Environment and Settings as roots. Work and Environment own their child actions. Chat overview shows compact status and links; menu details replace it in the same side pane. Do not repeat command output in both transcript and overview. A waiting indicator opens the operational trace without interrupting execution.
- **Command search:** Ctrl+P searches command names and descriptions independently
  of the message draft. Closing, immediate execution and cancelling argument entry
  preserve the original draft, cursor and selection, including when the picker is
  reopened during argument entry. Empty results never submit a message. Show the
  search cursor in its own field and advertise Esc as stop while a turn is active.
- **Remote control:** browser sends preserve the local draft, cursor, selection,
  attachments, skills and paste state. Questions require explicit validated answers;
  stale approvals and unsupported settings report a rejection. A failed tunnel clears
  the preparing state and leaves the local address usable. Failed browser sends and
  replies remain recoverable, reconnection restores status even at the same version,
  and retries reuse the action ID rather than submitting the same operation twice.
- **Layouts:** the composer and conversation retain priority as the terminal
  narrows. Do not force the right pane into an unreadable width. Use lines for
  main partitions and whitespace inside sections; avoid adding card containers.

Enforced examples live in `src/selfcheck.rs` (`ui_hierarchy`), plus the
session/nav tests in `src/app.rs`. Inspect `--self-check --screens` after changing
text or styling, and restart a running Loom to see rebuilt UI changes.
