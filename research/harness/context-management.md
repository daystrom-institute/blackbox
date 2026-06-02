---
title: "Axis: Context Management"
kind: research-axis
corpus: blackbox-research
track: harness
axis: context-management
topic:
  - harness
  - context-management
brief: "Cross-harness invariant model for the context-management axis: what enters and persists in the model's window across the turn lifecycle — the system-prompt split, markdown overlays (CLAUDE.md/AGENTS.md/GEMINI.md), first-turn injections, subsequent-turn reminders/nudges, env context, todo reinjection, and the ordering/cadence of all of it. Sibling of compaction (assembly in; compaction out). Synthesis of the per-subject context-management cells."
---

# Axis: Context Management

> **Scope.** The assembly side of the window: every byte the harness *adds* to
> the model's context and *when*. System prompt, markdown overlays, first-turn
> vs subsequent-turn injections, reminders/nudges, environment context, todo
> reinjection — and the cadence rules that govern them. Sibling of
> [compaction](compaction.md), which is the removal side.

## The dimension

This is where the "steer without bloat" line is actually walked. Every injection
is a token cost paid every turn it persists; every omission is a chance for the
agent to lose the thread. The mature harnesses are surgical: a cache-stable
system-prompt prefix, a small volatile tail, and reminders that fire only on a
trigger rather than every turn. Understanding the *cadence* (first-turn-only vs
recurring vs trigger-gated) is as important as the content.

## Questions a finding must answer

- **System-prompt structure.** Split into a cache-stable prefix + volatile tail?
  What lives where?
- **Markdown overlays.** How are `CLAUDE.md`/`AGENTS.md`/`GEMINI.md` discovered
  (cwd walk? global?), merged, and positioned? Precedence rules?
- **First-turn injections.** What is injected only on turn 1 (env context,
  directory listing, git status, available tools/skills)?
- **Subsequent-turn injections.** What recurs, and on what trigger? `<system-reminder>`
  text; todo-list reinjection; deferred-tool disclosure; "N tools available".
- **Cadence & ordering.** Trigger-gated vs every-turn? Where in the message does
  each injection sit (pre/post user content)?
- **Token discipline.** Anything actively trimmed to control bloat?

## Convergence / divergence

| Subject | Sys-prompt split | Overlay file | Reminder cadence | Todo reinjection | Cell |
|---|---|---|---|---|---|
| Claude | _TBD_ | CLAUDE.md | _TBD_ | _TBD_ | [claude](claude/claude-context-management.md) |
| Codex | _TBD_ | AGENTS.md | _TBD_ | _TBD_ | [codex](codex/codex-context-management.md) |
| Antigravity | _TBD_ | GEMINI.md | _TBD_ | _TBD_ | [antigravity](antigravity/antigravity-context-management.md) |
| Vibe | _TBD_ | _TBD_ | _TBD_ | _TBD_ | [vibe](vibe/vibe-context-management.md) |

## Open invariants

<!-- TODO(synthesis): -->
- Is the cache-stable-prefix + volatile-tail split universal?
- Do reminders converge on trigger-gating, or do some harnesses re-inject every
  turn (the bloat anti-pattern we want to avoid)?
- Overlay precedence: is "deeper cwd wins" / "project overrides global" common?

## Codex-lens extensions

- **Modality-switch context** — a realtime/voice mode may swap in a different
  startup-context blob and a different identity; context assembly is
  modality-dependent.
- **Model-switch continuity bridge** — a mid-session model change injects an
  explicit bridging instruction to preserve behavioral continuity.
- **Differential state-update injection** — permission/sandbox state changes are
  injected as *deltas* per turn (not full re-sends, which would bloat); cross-ref
  [privilege-approvals](privilege-approvals.md).
- **Behavioral layers** — operating modes / persona / roles are *injected* here
  as fragments but are modeled as a distinct axis
  ([modes-personas](modes-personas.md)); this axis carries the assembly
  mechanics, that axis carries the contracts.

## Feeds

`design/bro-harness/bro-harness-hooks.md` (system-prompt split + Nudger), and the
context-assembly idioms that should land in bro-harness's injection layer.
