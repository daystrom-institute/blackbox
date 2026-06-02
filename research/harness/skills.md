---
title: "Axis: Skills"
kind: research-axis
corpus: blackbox-research
track: harness
axis: skills
topic:
  - harness
  - skills
brief: "Cross-harness invariant model for the skills axis: the harness's skill / slash-command system — how skills are discovered and listed to the agent, how they're invoked, how progressive disclosure keeps their full instructions out of context until needed, and how arguments are passed. The progressive-disclosure pattern is a context-management win. Synthesis of the per-subject skill cells."
---

# Axis: Skills

> **Scope.** Agent-invocable named capabilities — skills, slash-commands, and the
> like. How they are advertised, invoked, and (critically) how their full
> instruction text is deferred until invocation. Distinct from [hooks](hooks.md)
> (harness-invoked) — skills are *agent- or user-invoked*.

## The dimension

Skills are a progressive-disclosure mechanism: a one-line name+description rides
in context always, but the skill's full (often large) instruction body loads
only when invoked. This is the same anti-bloat lever as MCP deferred tiering,
applied to procedural knowledge. The design questions: how skills are discovered,
the invocation contract, and how arguments thread through.

## Questions a finding must answer

- **Discovery.** How are available skills listed to the agent — name + one-line
  description? Where (system prompt, a reminder)? Plugin-namespaced?
- **Invocation.** What is the invocation contract (a tool call? a `/name` token
  the user types? both)? Can the agent invoke, or only the user?
- **Progressive disclosure.** Is the full skill body deferred until invocation?
  How does it load — injected as instructions, run as a subprocess, expanded
  inline?
- **Arguments.** How are args passed and parsed?
- **Authoring surface.** Where do skills live (files, catalog)? Frontmatter
  schema? Allowed tools per skill?
- **Built-in vs user.** Which ship with the harness vs user-authored?

## Convergence / divergence

| Subject | Discovery | Who invokes | Progressive disclosure | Args | Cell |
|---|---|---|---|---|---|
| Claude | listed w/ descriptions | agent + user (`/name`) | yes (body on invoke) | yes | [claude](claude/claude-skills.md) |
| Codex | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [codex](codex/codex-skills.md) |
| Antigravity | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-skills.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-skills.md) |

## Open invariants

<!-- TODO(synthesis): -->
- Is "name+description always; body on invoke" the convergent progressive-
  disclosure contract?
- Do all harnesses let the *agent* invoke skills, or only the user?

## Codex-lens extensions

- **Plugin bundling** — skills may ship inside plugins (namespaced
  `plugin:skill`); a plugin `@mention` can activate a whole capability bundle
  (skills + MCP + apps + hooks) under one namespace.
- **Mention-triggered provisioning** — mentioning a skill with unmet MCP
  dependencies can auto-install + auth them before the next turn (the tool
  surface expands mid-session). Cross-ref [mcp](mcp.md).

## Feeds

bro-harness skill/command surface (currently minimal) — this axis informs whether
and how the harness should adopt a progressive-disclosure skill system.
