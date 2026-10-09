# L4 baseline run, 2026-10-02 23:00 (invalid as a free-model gate)

Same conditions for both engines: 12 bench tasks, jobs 3, 600 s per task, 30 steps,
`--max-cost free`, remote first.

| engine | passed | input tokens | seconds | mean steps |
| --- | --- | --- | --- | --- |
| Native (`loom run`) | 12/12 | 420,778 | 368 | 6.3 |
| legacy (`custom-tui --run`, 18:15 build) | 12/12 | 226,135 | 334 | 6.1 |

Every call was answered by `claude-opus-4-6-thinking` through OmniRoute's
`antigravity` connection (a Google subscription login, still active in OmniRoute).
Native accepted it because OmniRoute omits `X-OmniRoute-Decision` on streamed replies,
so the route was unknown (ledger `route: null`) and the blocked-route check could not
fire. This is not a free-model result; rerun after the subscription connections
(antigravity, agy, grok-cli, xai-oauth) are deactivated.

Fixed after the run: OmniRoute is called without streaming so it reports the upstream;
a gateway reply without a reported upstream is refused (`reports_route`, fail closed);
the blocked list covers agy, grok-cli, xai-oauth, claude and codex. Verified live:
`last-n` passed with the antigravity reply refused and local Gemma answering.

Observed regardless of the model: Native sends ≈1.9× the input tokens of the legacy
engine at the same step count (≈5.0k vs ≈2.7k per call on `average-empty`). Likely
the larger tool set (terminal, agents, plan, skills, symbol edits, extensions) whose
schemas go with every call; to be measured before the L5 gate.
