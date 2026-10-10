---
name: loom-plan
description: Plan a feature or refactor from the repository with acceptance criteria, scope and checks. Use for implementation planning, 설계 or 구현 계획.
---

# Repository-grounded planning

Trace the requested behavior through the existing code before proposing new layers.
Use the available file and symbol tools; when a filename is unknown, use `glob` or
`find` with a narrow path/file filter. Prefer existing seams over a new framework.

Produce a plan containing the observable outcome, relevant files and ownership,
ordered changes, verification and the conditions that mean the work is complete.
Separate observed behavior from assumptions and proposed performance targets.
Name consequential tradeoffs and remaining uncertainty. Ask only questions whose
answers change scope or implementation; continue independent investigation.

For a planning-only request, deliver the plan without editing code or claiming
checks ran. If the user also requested implementation, proceed within that scope;
this skill does not introduce a separate approval gate. Use `update_plan` when it
is offered, otherwise keep a concise textual plan. Split work only where file
ownership and dependencies permit it; worker changes require merge before review.
