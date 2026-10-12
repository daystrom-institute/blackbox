# Agent Skills

Multi-step Blackbox procedures are packaged as agent skills: a directory with
a `SKILL.md` and any supporting files. A provider lists each skill's name and
description in every session and loads the body when the task matches, so a
procedure reaches the agent at the point of use.

Each procedure has one canonical copy, the skill. Tool reference stays in the
tool descriptions and the generated task guides, and those point at the skill
for the procedure instead of restating it.

## The skills

Global skills apply in every project and are installed into provider homes:

| Skill | Use |
|---|---|
| `bbox-onboard-project` | Register a checkout, commit its identity files, verify publication |
| `bbox-bro-dispatch` | Dispatch, resume, supervise and review bro tasks |
| `bbox-gap` | Dedupe and file a substrate gap note; amend or resolve one |
| `bbox-health` | Read-only health triage, then operator-gated maintenance |
| `bbox-recall` | Answer from stored rules, sessions and indexed code |
| `bbox-persist` | Propose durable knowledge and save only approved text and scope |

Project skills apply to work in this repository and are read from the
checkout:

| Skill | Use |
|---|---|
| `bbox-install` | Stand up or extend a Blackbox deployment |
| `bbox-dev-lanes` | Choose where builds, tests and verification run |
| `bbox-retro` | Debrief a session into gap notes, thread notes and proposed issues |

## Layout

| Path | Contents |
|---|---|
| `system-defaults/skills/<name>/` | Global skills, the install source |
| `.agents/skills/<name>/` | Project skills, discovered by Codex from the working directory up to the repository root |
| `.claude/skills/<name>` | Relative symlinks to `.agents/skills/<name>` for Claude Code |

## Installing the global skills

Run on every host where agents work, and again after the skills change:

```bash
scripts/sync-skills.sh --check    # report pending changes, write nothing
scripts/sync-skills.sh
```

The script copies each global skill once into `~/.agents/skills`, which Codex
reads whatever `CODEX_HOME` is, and links that copy into every Claude Code home
(`~/.claude` and `~/.claude-*`). It replaces or removes only what it installed
and reports anything else at a target path as a conflict.

Install a skill name in exactly one place. Providers do not merge same-name
skills from two scanned directories, so a second copy shows up as a duplicate.

The daemon also serves the onboarding procedure as the MCP resource
`blackbox://skills/onboard-project/SKILL.md`, built from the same skill body
with the deployment's current checkout hosts and producer policy appended.

## Writing a skill

- Front matter is `name` and `description` only. The name matches the
  directory and carries the `bbox-` prefix.
- The description is the trigger. Lead with what the skill does, then the
  situations and phrasings that should invoke it. Keep it near 350 characters:
  every installed skill's description shares one context budget.
- `SKILL.md` carries everything an agent must follow on every invocation. Put
  rules agents tend to skip first, each with its reason.
- A supporting file holds material only some invocations need, and `SKILL.md`
  says exactly when to read it. Link it with a relative Markdown link.
- Global skills are deployment-agnostic: no operator hostnames or local paths.
  Project skills link to the repository's docs for commands and config keys
  instead of restating them.
- Refer to tools by bare name (`bbox_gap`, `bro_exec`). Operator tools are
  called as `bro mcp call <tool> '<json>' --surface ops`.
- State the current procedure, not its history.

A behaviour that must hold in every session without being invoked is not a
skill. It belongs in rendered knowledge, as one line that names the skill
carrying the procedure.

## System memories

`system-defaults/memories/` holds the daemon-loaded `sm-*` references that
`bbox_knowledge` returns by exact id: retirement notes and cold reference that
is not a procedure. Procedures do not go there.
