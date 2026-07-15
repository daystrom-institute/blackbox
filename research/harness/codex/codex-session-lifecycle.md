---
title: "Codex · Session Lifecycle & History"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: session-lifecycle
version: "main@8aae858958"
last_verified: "main@8aae858958"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - session-lifecycle
brief: "Codex retains persisted rollout resume/fork/rollback and a first-class agent graph, while adding UUIDv7 context-window lineage, bounded reconstruction from safe compaction checkpoints, persisted inter-agent communication, and cold restoration of v2 descendant identities with lazy runtime loading on targeted messages."
---

# Codex - Session Lifecycle & History

See axis: [Session Lifecycle & History](../session-lifecycle.md) and snapshot:
[Codex main@8aae858958](codex-main-8aae858958.md).

## Finding

Durable rollouts, fork, rollback, and graph topology from 0.136.0 remain. The
current implementation extends reconstruction to two additional state families:
model context-window lineage and live multi-agent identity.

**Confidence: high.** Rollout protocol types, reconstruction, graph-store code,
and cold-resume tests are open source at the captured revision.

### History and context windows

`ThreadRolledBack` still trims effective history at user-turn boundaries, which
is rewind rather than forward summarization. Compaction now advances a UUIDv7
context-window chain while retaining the same thread identity. First, previous,
and current window identity are persisted and restored.

Model context can be reconstructed from a bounded rollout suffix beginning at a
safe compaction checkpoint. If the suffix cannot prove a valid baseline, Codex
uses the full rollout instead of silently dropping required history.

### Descendant restoration

Multi-agent v2 communication is persisted as typed rollout items. On cold root
resume, Codex restores descendant canonical names and graph positions. It does
not need to eagerly instantiate every descendant runtime: targeted communication
can load the required runtime lazily.

This separates durable identity from live process/task state. An interrupted or
completed turn does not by itself destroy the addressable agent object.

### Fork scope

New agents can choose a fork history of none, all, or a positive number of most
recent turns. A reusable extension-level runner starts a resolved agent in a
fresh forked thread and propagates trace context. Spawned-agent prompt-cache
identity now derives from the root session rather than the child thread ID, so
related forks can reuse a stable prefix.

## Evidence

- `codex-rs/core/src/session/rollout_reconstruction.rs` - history, World State,
  and context-window reconstruction.
- `codex-rs/core/src/agent/` and `codex-rs/agent-graph-store/` - persisted
  topology and restoration.
- `codex-rs/ext/agent/src/lib.rs` - reusable fork runner.
- Commits `592467fb96`, `088239294a`, `b4f0f3eff1`, and `4aa950d456`.

## Vs the axis

The refresh adds a useful invariant: **identity outlives runtime materialization**.
Persist canonical identity and topology; load execution state only when a live
operation needs it. Context-window lineage is related but distinct: it tracks
which bounded model history is active inside the same durable thread.

## Open

- Cross-process or remote-worker restoration still needs an owner for the live
  runtime and mailbox transport.
- Agent eviction and retention policy are operational choices above the durable
  identity contract.
