---
name: loom-debug
description: Diagnose an application failure from reproduction and evidence, then verify a narrow repair. Use for bugs, regressions, 버그 or 오류 원인 investigation.
---

# Reproduce, isolate, repair

Capture the observed failure: trigger, expected behavior, actual output and the
smallest available reproduction. Inspect the code responsible for that behavior
with scoped reads and symbol context. Rank plausible causes and choose a check
whose result distinguishes them; avoid editing on a hypothesis alone.

Use the command tool available in this runtime (`exec_command` or `run`) within
its execution permissions. A running PTY result is not a failure: poll its existing
session with `write_stdin` rather than starting the same command again. Keep the
artifact reference for long output, and read the relevant log range when offered.
If execution is unavailable, state what remains unverified and use source evidence.

When a repair is requested, change the narrowest responsible layer, run the
reproduction again and the checks that protect affected behavior. For diagnosis
only, report the supported cause and repair recommendation without editing.
Do not broaden a fix into unrelated cleanup. Finish with the cause, change,
observed check results and any remaining limitation; never infer success from
an empty log or a test that did not run.
