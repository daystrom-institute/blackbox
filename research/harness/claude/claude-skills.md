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
brief: "How Claude Code 2.1.160 exposes skills: a name + one-line-description manifest injected into context (always present, body deferred), invoked via the Skill tool or a user-typed /name, with plugin namespacing (plugin:skill) and argument passing. Progressive disclosure keeps large skill bodies out of context until invocation — the same anti-bloat lever as MCP deferred tiering. Backfilled from direct observation of the running harness."
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

## Open

<!-- TODO(mine): the skill-authoring frontmatter schema; per-skill allowed-tools
restriction; how a skill body is delivered on invoke (injected instructions vs
subprocess); built-in vs plugin vs user-authored discovery paths; arg parsing. -->

## Feeds

bro-harness skill/command surface (currently minimal) — informs whether the
harness adopts a progressive-disclosure skill system and how.
