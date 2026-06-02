---
title: "Codex · Memory & Persistence"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: memory-persistence
version: "0.136.0"
last_verified: "0.136.0"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - memory-persistence
brief: "Codex: a two-phase cross-session memory pipeline — Phase 1 extracts a structured memory per rollout; Phase 2 consolidates globally in a dedicated MemoryConsolidation sub-agent session with write access to a git-tracked memory workspace. Model reads injected memories at session start (raw_memories.md / rollout_summaries) and can write ad-hoc notes."
---

# Codex · Memory & Persistence

> From the codex-lens discovery mine (general-purpose readers over `~/repos/codex/codex-rs`, 2026-06-02) — the pass that surfaced these axes. **confidence: high** (file:line). Codex's base-axis cells (transport…skills) remain stubs pending a full mining pass.
See axis: [Memory & Persistence](../memory-persistence.md) · snapshot: [Codex 0.136.0](codex-0.136.0.md).

**Finding.** A genuine cross-session pipeline: **Phase 1** finds recent eligible rollouts and extracts a structured memory per thread; **Phase 2** consolidates globally as a dedicated sub-agent session (`ThreadSource::MemoryConsolidation`) with write access to a **git-tracked memory workspace**. The model reads injected memories at session start (`raw_memories.md`, `rollout_summaries/`) and can write an **append-only ad-hoc note** ("after the user explicitly asks Codex…").

**Evidence.**
- `memories/README.md:40` — "Phase 1 … extracts a structured memory from each [rollout]"
- `protocol/src/protocol.rs:2501` — `ThreadSource::MemoryConsolidation`
- `ext/memories/src/tools/ad_hoc_note.rs:56` — append-only ad-hoc memory note

**Vs the axis.** Confirms durable, model-perceived + model-written memory. One of three distinct architectures (codex: extract→consolidate sub-agent; claude: auto-dream + sync; agy: versioned brain artifacts), vs vibe (none).

## Open
<!-- Phase-1 extraction prompt; eligibility heuristic; injection scoping (project vs global). -->
