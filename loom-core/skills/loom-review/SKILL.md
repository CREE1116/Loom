---
name: loom-review
description: Review code changes for actionable bugs with source locations and evidence. Use for a diff, patch, PR review, 코드 리뷰 or 변경 검토.
---

# Review the changed behavior

Establish the comparison: the requested patch, working-tree diff or named base.
Use the available Git/file tools; distinguish staged and unstaged changes when
that affects the request. Inspect changed code together with relevant callers,
contracts and tests. Use symbol impact when available for shared behavior.

Prioritize introduced correctness, data-loss, compatibility and performance
problems that have a concrete trigger. A finding states its severity, file and
line, trigger, consequence and a practical correction. Separate demonstrated
failures from uncertain risks; do not manufacture findings to fill a quota.

Review alone does not authorize edits. Run focused checks only when the runtime
and request permit them. Otherwise state which behavior was not executed.
Report actionable findings first; if none are supported, say so and identify
material verification gaps. Do not claim full correctness from a clean diff.
