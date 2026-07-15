---
title: "Axis: Session Lifecycle & History"
kind: research-axis
corpus: blackbox-research
track: harness
axis: session-lifecycle
topic:
  - harness
  - session-lifecycle
brief: "Cross-harness invariant model (tier-2 candidate) for the session axis: the persisted-session machinery the agent perceives — resume, fork, and rollback/rewind of turn history (distinct from forward-summarizing compaction), plus the spawn-tree topology (depth, path, parent→child graph) an agent occupies. Surfaced by the codex-lens discovery pass; tier-2 because parts overlap subagents (axis 8) and compaction (axis 3) and need de-confliction as it matures."
---

# Axis: Session Lifecycle & History

> **Scope (tier-2 candidate).** The session/rollout machinery the model
> perceives or that changes the history it reasons from: resume, fork,
> **rollback** (rewind N turns — distinct from compaction's forward-summarize),
> and **spawn-tree topology** (an agent's depth/path/position in the parent→child
> graph). Tier-2 because it partially overlaps [subagents](subagents.md)
> (topology) and [compaction](compaction.md) (history mutation); promote/split it
> as cells land.
>
> **Surfaced by:** the codex-lens bottom-up pass.

## The dimension

Long-lived agentic work is session-based: histories are persisted (rollouts),
can be resumed, forked into children from a baseline, and **rolled back** to trim
the last N turns from the effective context. Rollback is categorically different
from compaction — compaction *summarizes forward* to save space; rollback
*erases backward* at explicit turn boundaries, and the model reasons from the
post-rollback history without seeing the erased turns. Separately, a spawned
agent occupies a **position** in a topology (depth, a path like `/root/child`, a
persisted parent→child edge graph) that the harness uses to enforce depth limits
and route escalation, and that the agent may perceive as positional
self-knowledge.

## Questions a finding must answer

- **Persistence.** Are sessions/rollouts durably stored? In what form?
- **Resume.** Can a session resume with history (and usage) intact?
- **Fork.** Can a child fork from a baseline, optionally truncated? What history
  does the child see?
- **Rollback / rewind.** Can history be trimmed at turn boundaries? Does the
  model perceive post-rollback history only? How does this interact with
  compaction?
- **Topology.** Is there a spawn-tree (depth, path, graph)? Depth-limited? Does
  the agent know its position/role/nickname?
- **Session source/type.** Are there distinct session kinds (root / sub-agent /
  background-maintenance) the model or harness branches on?

## Convergence / divergence

| Subject | Resume | Fork | Rollback | Topology | Cell |
|---|---|---|---|---|---|
| Claude | `--continue`/`--resume` (.jsonl) | (via rewind) | `/rewind` (file snapshots) | flat subagent spawns | [claude](claude/claude-session-lifecycle.md) |
| Codex | rollouts | fork w/ truncated baseline | `ThreadRolledBack` (trim N turns) | depth/path/graph store | [codex](codex/codex-session-lifecycle.md) |
| Antigravity | continue/resume (JSON→SQLite) | — | "rewind to step" + IDE export | server-side | [antigravity](antigravity/antigravity-session-lifecycle.md) |
| Vibe | `--continue`/`--resume` | `fork(message_id)` | RewindManager (file snapshots) | `parent_session_id` chain | [vibe](vibe/vibe-session-lifecycle.md) |

**Synthesis (4 subjects).** **Rewind/rollback is a 4-way convergence** — every subject can undo history at a boundary (codex `ThreadRolledBack`, claude `/rewind`, vibe `RewindManager`, agy "rewind to step"), and claude+vibe share the **file-snapshot restore** mechanism. **Persisted spawn topology** (depth/path/graph) is **codex-distinctive**; others keep subagents flat or ephemeral. Storage uniquely moving to SQLite in agy. This confirms session-lifecycle as a real axis (de-conflict rewind↔compaction noted in Open invariants).

**Codex refresh (main@8aae858958).** Durable identity is now explicitly
separable from live runtime materialization: descendant names/topology restore
on cold root resume and a targeted message lazily loads the runtime. Context
windows also have first/previous/current UUIDv7 lineage within the same thread.

## Open invariants

<!-- TODO(synthesis): -->
- De-conflict with axes 3 (compaction) and 8 (subagents): rollback → here or
  compaction? topology → here or subagents? Decide as evidence accumulates.
- Is rollback/rewind common, or a codex-distinctive capability?
- Is persisted spawn-tree topology general, or do most harnesses keep subagents
  ephemeral and flat?

## Feeds

bro-harness sessions + (future) neuralyze (rewind + carry a message) are the
closest analogues — `design/bro-harness/bro-harness-neuralyze.md`. Topology
relates to the recursion guard + subagent delegation.
