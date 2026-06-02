---
title: "Vibe · Skills"
kind: research-finding
corpus: blackbox-research
track: harness
harness: vibe
axis: skills
version: "2.9.6"
last_verified: "2.9.6"
status: enriched
confidence: high
topic:
  - harness
  - vibe
  - skills
brief: "Vibe skills: SKILL.md files (YAML frontmatter: name/description/allowed_tools/user_invocable/compatibility) discovered from .vibe/skills/, ~/.vibe/skills/, AGENTS_HOME; invoked via /skillname slash commands; the skill tool loads the body as a user message; allowed_tools is a per-skill pre-approval (a lightweight permission layer)."
---

# Vibe · Skills

> Mined from open source (GLM-5.1 bro, 2026-06-02). **confidence: high.** See axis: [Skills](../skills.md) · snapshot: [Vibe 2.9.6](vibe-2.9.6.md).

**Finding.** Skills = `SKILL.md` files in subdirs of skill search paths (project `.vibe/skills/` walked cwd-upward, user `~/.vibe/skills/`, `AGENTS_HOME`). Frontmatter: `name`, `description`, `allowed_tools`, `user_invocable`, `compatibility`, `license`, `metadata`; body = the prompt. Invoked via `/skillname` slash commands (`parse_skill_command`); the `skill` tool loads the full body and injects it as a user message (progressive disclosure). `enabled_skills`/`disabled_skills` filter. Built-in `vibe` skill = self-docs.

**Evidence.**
- `vibe/core/skills/manager.py:57` — `_discover_skills_in_dir` (SKILL.md scan)
- `vibe/core/skills/models.py:12` — `SkillMetadata` with `allowed_tools`, `user_invocable`
- `vibe/core/skills/manager.py:111` — `parse_skill_command` (/skillname)

**Vs the axis.** Confirms progressive disclosure (name+desc always; body on invoke). **Extends:** per-skill `allowed_tools` is a skill-scoped pre-approval — a crossover with [privilege-approvals](../privilege-approvals.md).

## Open
<!-- How allowed_tools pre-approval composes with the main PermissionStore. -->
