---
title: "Codex · Session Lifecycle & History"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: session-lifecycle
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - session-lifecycle
brief: "Codex: persisted rollouts with resume + fork (from a truncated baseline) + ROLLBACK via ThreadRolledBack markers (trim last N user turns — rewind, not summarize). Spawn-tree topology is first-class: SubAgentSource carries parent_thread_id/depth/agent_path (/root/child), backed by an AgentGraphStore (BFS parent→child edges); depth-limited."
---

# Codex · Session Lifecycle & History

> From the codex-lens discovery mine (general-purpose readers over `~/repos/codex/codex-rs`, 2026-06-02) — the pass that surfaced these axes. **confidence: high** (file:line). Codex's base-axis cells (transport…skills) remain stubs pending a full mining pass.
See axis: [Session Lifecycle & History](../session-lifecycle.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** Durable rollouts with resume and **fork** (child from a baseline, optionally truncated). **Rollback:** `ThreadRolledBack` markers trim the last N user turns from effective history — the model reasons post-rollback (rewind backward, distinct from forward-summarizing compaction). **Topology is first-class:** `SubAgentSource::ThreadSpawn{parent_thread_id, depth, agent_path, agent_role}` with URL-like `agent_path` (`/root`, `/root/child`), persisted in an `AgentGraphStore` (BFS-traversable parent→child edges), depth-limited by config.

**Evidence.**
- `core/src/thread_rollout_truncation.rs:31` — ThreadRolledBack trims last N user turns
- `protocol/src/protocol.rs:2543` — `SubAgentSource::ThreadSpawn{depth,agent_path,agent_role}`
- `agent-graph-store/src/store.rs:12` — `AgentGraphStore` spawn-edge graph

**Vs the axis.** Anchors the tier-2 axis: resume + fork + rollback + **persisted spawn topology** (depth/path/graph). 4-way rewind convergence (codex rollback / claude /rewind / vibe RewindManager / agy step-rewind); topology is codex-distinctive (others keep subagents flat/ephemeral).

## Open
<!-- Rollback vs compaction interaction; whether agent_path is model-visible self-knowledge. -->
