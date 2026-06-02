---
title: "Axis: Memory & Persistence"
kind: research-axis
corpus: blackbox-research
track: harness
axis: memory-persistence
topic:
  - harness
  - memory-persistence
brief: "Cross-harness invariant model for the memory axis: durable, cross-SESSION memory the model can read and write — distinct from in-context compaction (which is within-session, forward-summarizing). Covers the extract→consolidate pipeline, model-writable memory artifacts, injection scope at session start, and the schedule/ownership of memory writes. Surfaced by the codex-lens discovery pass."
---

# Axis: Memory & Persistence

> **Scope.** Durable memory that crosses session boundaries — what the model can
> recall from past sessions and what it can durably write for future ones.
> Categorically distinct from [compaction](compaction.md): compaction is
> *within-session, forward-summarizing* to fit a window; this axis is
> *cross-session durable recall*. Also distinct from
> [context-management](context-management.md), which assembles the *current*
> window from static overlays — memory is dynamic, accreted, and model-authored.
>
> **Surfaced by:** the codex-lens bottom-up pass.

## The dimension

When an agent's useful knowledge outlives a single session, the harness needs a
memory subsystem: a way to extract durable facts from a session, consolidate them
(often via a dedicated background/sub-agent pass), store them in a scoped
artifact, and inject the relevant subset at the start of a future session. The
agent-facing parts are (a) the injection of recalled memory into context and
(b) any tool by which the model itself writes a memory. The harness-design
decisions — when consolidation runs, what the model may self-write, how injected
memory is scoped — are exactly the levers a "steer without bloat" harness must
get right (recall too much and every session is bloated; too little and the agent
re-learns each time).

## Questions a finding must answer

- **Read path.** Is past-session memory injected at session start? Scoped how
  (project / global / task)?
- **Write path.** Can the model write memory directly (a tool), or only via an
  automated extraction pipeline?
- **Pipeline.** Is there an extract (per-session) → consolidate (global) flow?
  Does consolidation run as a separate (sub-agent) session?
- **Storage.** Where do memories live (git-tracked workspace, store)? Are they
  reviewable/durable, or opaque?
- **Schedule & ownership.** When does memory run (background, on-demand)? What is
  model-owned vs operator-owned?
- **Scoping & bloat control.** How is the injected subset bounded?

## Convergence / divergence

| Subject | Read (inject) | Write (model) | Consolidation | Storage | Cell |
|---|---|---|---|---|---|
| Claude | CLAUDE.md + auto-memory injection | `/memory` writes CLAUDE.md | **auto-dream** + personal/team sync | `~/.claude/projects/<cwd>/memory/` + server API | [claude](claude/claude-memory-persistence.md) |
| Codex | session-start (`raw_memories.md`) | ad-hoc note tool | **two-phase** extract→consolidate sub-agent | git-tracked memory workspace | [codex](codex/codex-memory-persistence.md) |
| Antigravity | `MemoryConfig` user-memory inject | (planning writes artifacts) | retrieval subagents (`knowledge_*`) | versioned brain artifacts | [antigravity](antigravity/antigravity-memory-persistence.md) |
| Vibe | AGENTS.md (static) | — | — | none (read-only session logs) | [vibe](vibe/vibe-memory-persistence.md) |

**Synthesis (4 subjects).** Durable cross-session memory is the dividing line: **three present, one absent**. And the three differ architecturally — claude (**auto-dream** consolidation + personal/team server sync), codex (**two-phase** extract→consolidate sub-agent over a git workspace), agy (**versioned brain artifacts** + retrieval subagents) — while **vibe has none** (static AGENTS.md only). Note: claude's pipeline corrected a session-1 assumption that it had only static overlays.

> **Note vs Blackbox.** Blackbox's own knowledge/memory system (`bbox_learn`/
> `bbox_remember`/`bbox_pin`, rendered overlays) is a mature instance of this
> axis — useful as a comparison point, but this axis studies what *harnesses*
> bake in natively.

## Open invariants

<!-- TODO(synthesis): -->
- Do harnesses converge on extract→consolidate, or is static-overlay (Claude
  CLAUDE.md) the more common (shallower) form?
- Is model-self-write of durable memory common, or mostly automated extraction?

## Feeds

`design/corpus/knowledge/` (Blackbox's knowledge/memory designs, for comparison).
bro-harness is session-scoped by invariant — a native cross-session memory would
be a deliberate new capability this axis scopes.
