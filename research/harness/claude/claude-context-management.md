---
title: "Claude · Context Management"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: context-management
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high
topic:
  - harness
  - claude
  - context-management
brief: "How Claude Code 2.1.160 assembles and maintains the model's context window: the markdown overlay system (CLAUDE.md/PROJECT.md with @-imports, cwd-walk discovery), the first-turn env/context block, and the trigger-gated <system-reminder> injection mechanism (todo-nag, deferred-tool disclosure, MCP server instructions). The cadence model — trigger-gated, not every-turn — is the steer-without-bloat lever. Backfilled from direct observation of the running harness."
---

# Claude · Context Management

> **Provenance.** Direct observation of the running 2.1.160 harness — these
> injections are observed in-band this session. **confidence: high** for what is
> visible in context; the harness-side trigger logic is inferred from observed
> cadence (medium where noted). See [snapshot](claude-2.1.160.md).

See the axis: [Context Management](../context-management.md).

## Markdown overlays (high)

- **Discovery & merge.** Project `CLAUDE.md` + `PROJECT.md` are injected, plus the
  user's global `~/.claude/CLAUDE.md`. Overlays support **`@path` imports** that
  inline other files; in symlinked setups imports resolve relative to the symlink
  *target* (a documented footgun). Project memory is presented as overriding
  default behavior (*"These instructions OVERRIDE any default behavior"*).
- **Positioning.** Overlays arrive as a leading system-reminder-wrapped block
  ("Codebase and user instructions are shown below"), ahead of the conversation.

## First-turn / env block (high)

The harness injects an **environment block** on session start: primary working
directory, is-git-repo flag, platform, OS version, today's date, and the running
**model id** (`claude-opus-4-8[1m]`). A git status snapshot (branch, main branch,
recent commits, status) is included and explicitly stamped *"snapshot in time,
will not update."*

## `<system-reminder>` injections (high content / medium trigger)

The keystone mechanism: small reminders wrapped in `<system-reminder>` tags,
injected by the harness (not the user), **trigger-gated rather than every-turn** —
this is the anti-bloat cadence bro-harness should match:

- **Todo-nag.** When task tools have been idle, a reminder suggests
  `TaskCreate`/`TaskUpdate` — fires on a usage-gap trigger, with *"ignore if not
  applicable"* framing, and re-emits the current task list.
- **Deferred-tool disclosure.** *"The following deferred tools are now available
  via ToolSearch. Their schemas are NOT loaded…"* — the names-only manifest (see
  [claude-mcp](claude-mcp.md)).
- **MCP server instructions.** Server-provided instruction blocks injected under
  an "MCP Server Instructions" heading.
- **Skill availability.** The skills list (name + one-line description) is
  injected (see [claude-skills](claude-skills.md)).
- **Provenance framing.** Reminders self-identify as harness-injected: *"`<system-reminder>`
  tags … are injected by the harness, not the user."*

## Token discipline (high)

Context-compression note tells the agent that long conversations get summarized
and re-presented (*"you don't need to wrap up early"*) — i.e. assembly hands off
to [compaction](../compaction.md) without the agent managing it.

## Open

<!-- TODO(mine): string-mine for the exact <system-reminder> templates and their
trigger predicates (what usage-gap fires the todo-nag; cadence rules). Confirm
the cache-stable-prefix vs volatile-tail split at the message-assembly level
(the api-robustness mine confirms it at the system-prompt level). -->

## Feeds

`design/bro-harness/bro-harness-hooks.md` (system-prompt split + Nudger) — the
trigger-gated reminder cadence is the model for the harness Nudger.
