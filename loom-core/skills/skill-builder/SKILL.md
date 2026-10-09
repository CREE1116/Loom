---
name: skill-builder
description: Create or fix a skill (SKILL.md) that Loom, Claude Code and Codex can all load. Use when the user asks to make, package, rename or repair a skill.
---
<!-- Bundled with Loom. Loom updates this file until you edit it. -->

# Building a skill

A skill is a folder with a `SKILL.md`: frontmatter that says what it is, then the
instructions an agent follows once it loads the skill.

## Where it goes

| Scope | Folder | Read by |
| --- | --- | --- |
| This repository, every agent | `.agents/skills/<name>/SKILL.md` | Loom, Codex |
| This repository, Loom only | `.loom/skills/<name>/SKILL.md` | Loom |
| This repository, Claude Code | `.claude/skills/<name>/SKILL.md` | Claude Code, Loom |
| Every repository | the same folders under the home directory (`~/.agents/skills`, ...) | as above |

Prefer `.agents/skills` for a skill the whole team should share. Loom also reads skills
that Claude Code and Codex plugins install.

## SKILL.md format

```markdown
---
name: release-notes
description: Write release notes from merged changes since the last tag. Use when the user asks for release notes or a changelog entry.
---

# Release notes
1. ...
```

- `name`: lowercase words joined by hyphens, unique. The user calls it as `$name`.
- `description`: what the skill does, then when to use it. Project skills show the
  first 160 characters to the model; skills from the home directory are listed by name
  only, so the name must say what it does.
- Body: numbered steps in the imperative, what to read first, what to produce, and how
  to check the result. State inputs the skill needs from the user.
- At most 64 KB. Put long references, templates and scripts next to `SKILL.md`
  (`references/`, `scripts/`) and name them by relative path; they resolve against the
  skill folder and are read only when needed.

## Creating one

1. Ask, if unclear: the task it covers, the words that should trigger it, and whether it
   belongs to this repository or to every repository.
2. Look for an existing skill with the same purpose (`use_skill` with its name, or the
   skill list in the system prompt) and extend it instead of adding a near-duplicate.
3. Create the folder and `SKILL.md` with the file tool (create), inside the repository for
   project skills. Keep frontmatter on the first lines.
4. Check it: the frontmatter has `name` and `description`; every relative path in the
   body exists; nothing in it depends on a particular model or machine.
5. Tell the user how to call it (`$name` in a request, or pick it in the skill list).
