# Free-model measurements, 2026-10-02 23:30

Subscription connections in OmniRoute deactivated (antigravity, agy, grok-cli,
xai-oauth; claude and codex were already off). `auto` then routed every call to
`z-ai/glm-5.2` through the `cl` connection (a Cline account login, also OAuth — see
open question). 12 tasks, jobs 3, 600 s, 30 steps, `--max-cost free`.

Native tool disclosure fix applied before these runs: core tools plus `enable_tools`
up front, other groups on demand; user-level skills named only. First request
15.2 KB → 5.2 KB (legacy 4.5 KB).

| engine (same model) | passed | input tokens | seconds | mean steps | notes |
| --- | --- | --- | --- | --- | --- |
| legacy `custom-tui --run` (18:15) | 12/12 | 180,943 | 276 | 6.8 | no Core validation |
| Native `loom run` | 12/12 | 162,011 | 244 | 6.2 | validated 12, false finish 0 |
| Native `--with-context` | 12/12 | 162,076 | 221 | 5.5 | slice rereads 22 (vs 10) |

Reading:
- L4 quality gate on a free model: passed (12/12, equal to legacy), with 10% fewer
  input tokens than legacy after the disclosure fix (was 1.9× more).
- L5 gate (remote input −50%): not met. The context compiler changes nothing in total
  on this bench: the tasks are single small files, so orientation is already cheap,
  and the model re-reads supplied code anyway (rereads 22). Keep it opt-in; this bench
  cannot show its benefit. Needs larger multi-file tasks.
- Contract recall 0.47 with 13 scope events: the derived contract misses about half
  of the touched files (mostly Cargo.lock / tests). Worth fixing before Split work.

Open: the `cl` (Cline) connection is an account login used outside the Cline client.
Same risk class as the subscription logins; not blocked by Loom yet — user decision.

Pending: vanilla Codex baseline (`codex exec`, gpt-5.6-sol) scheduled after the Codex
usage limit resets at 01:27 → `codex.txt`, `codex.json` (run-codex-baseline.sh).
