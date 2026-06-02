---
title: "Claude · Skills"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: skills
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - skills
brief: "Claude Code 2.1.160 exposes skills through deferred one-line manifests plus Skill tool or /name invocation. Current CLI adds --disable-slash-commands as all-skills disable, --plugin-dir/--plugin-url for session-only plugins, plugin init/list/details/validate surfaces, SessionStart reloadSkills support, and hooks/agents/MCP/plugins as skill-adjacent extension bundles."
---

# Claude · Skills

> **Provenance.** Direct observation of the running 2.1.160 harness — the skills
> list and the Skill tool contract as the model receives them.
> **confidence: high.** See [snapshot](claude-2.1.160.md).

See the axis: [Skills](../skills.md).

## Discovery (high)

Available skills are advertised as a **name + one-line description** list in
context (e.g. `deep-research`, `code-review`, `crucible`, `cosession`, `verify`,
`update-config`, …). Only the one-liner rides in context; the full skill body is
**not** loaded until invocation — progressive disclosure. The list explicitly
warns: *"Only invoke a skill that appears in that list … Never guess or invent a
skill name."*

## Invocation (high)

- **Tool path.** The `Skill` tool takes `skill` (exact name, or
  `plugin:skill` for plugin-namespaced) + optional `args`.
- **User path.** A user-typed `/<name>` maps to the same skill; the harness
  surfaces the invocation as a `<command-name>`/`<command-args>` block. (Observed
  this session via `/remote-control`.)
- **Blocking-requirement framing.** When a skill matches a request, invoking it
  is a *"BLOCKING REQUIREMENT … BEFORE generating any other response"*, and a
  skill already loaded (a `<command-name>` tag present) must not be re-invoked.

## Progressive disclosure (high)

This is the skills analogue of [MCP deferred tiering](claude-mcp.md): procedural
knowledge is named-but-not-loaded, expanded to full instructions only on
invocation. The context cost of having N skills available is N one-liners.

## Plugin And Reload Deltas (2026-06-02 local pass)

Current CLI help and changelog deepen the skill/plugin surface:

- --disable-slash-commands is described as disabling all skills.
- --plugin-dir and --plugin-url load plugins for the current session only; claude agents has matching --plugin-dir dispatch support.
- claude plugin init scaffolds a plugin under ~/.claude/skills/<name>/ and auto-loads it next session as <name>@skills-dir.
- claude plugin details reports a plugin component inventory and projected token cost; Discover/Browse screens show commands, agents, skills, hooks, and MCP/LSP servers before install.
- SessionStart hooks can return reloadSkills: true so skills installed by the hook are available in the same session.
- Changelog entries confirm hooks support in skill/slash-command frontmatter, forked sub-agent skill execution via context: fork, an agent field in skill frontmatter, and skill frontmatter for auto-loading skills in subagents.

This reinforces the earlier anti-bloat finding: Claude treats skills as part of a broader plugin component graph, but the model still gets a cheap manifest until a skill is invoked or explicitly loaded.

## Open

<!-- TODO(mine): the skill-authoring frontmatter schema; per-skill allowed-tools
restriction; how a skill body is delivered on invoke (injected instructions vs
subprocess); built-in vs plugin vs user-authored discovery paths; arg parsing. -->

## Feeds

bro-harness skill/command surface (currently minimal) — informs whether the
harness adopts a progressive-disclosure skill system and how.
