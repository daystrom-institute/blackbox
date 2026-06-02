---
title: "Antigravity - Memory & Persistence"
kind: research-finding
corpus: blackbox-research
track: harness
harness: antigravity
axis: memory-persistence
version: "1.0.4"
last_verified: "1.0.4"
status: enriched
confidence: medium
topic:
  - harness
  - antigravity
  - memory-persistence
brief: "SDK persistence is conversation/artifact persistence, not proven model-writable long-term memory: conversation_id plus save_dir resume sessions, app_data_dir stores artifacts/scratch/media, and Conversation keeps usage/history. CLI binary/local state add knowledge/brain/persistent-context signals, but the durable memory contract still needs live examples."
---

# Antigravity - Memory & Persistence

> Evidence: public google-antigravity SDK source at f74a23fc5f4026129a5b4498ce652d7d6018e23f, installed agy 1.0.4 binary strings/changelog, and current ~/.gemini host state. SDK claims are high confidence for the SDK/localharness surface; CLI/cortex memory claims remain medium unless backed by populated live state.
See axis: [Memory & Persistence](../memory-persistence.md) - snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

## Finding

The SDK clearly supports persistence, but it does not prove a general model-writable long-term memory system. save_dir persists conversation state; conversation_id resumes that state; app_data_dir controls artifact/scratch/media location. Conversation exposes history, turn counts, usage totals, compaction indices, clear_history, delete, cancel, and disconnect.

The standalone CLI has additional memory-looking surfaces. Current host state includes ~/.gemini/antigravity-cli/knowledge/knowledge.lock and brain transcript directories under ~/.gemini/antigravity-cli/brain/<uuid>/.system_generated/logs/. Binary strings reference knowledge items, persistent context, MemoryConfig, and knowledge-oriented subagents. This pass did not find populated user memory artifacts or a confirmed memory write/read schema.

## Design Takeaways

- Treat Antigravity persistence as three separate layers: resumable conversation state, artifact/media storage, and possible cross-session knowledge retrieval.
- The SDK proves the first two layers. The third is suggested by CLI strings and local directory names but not source-confirmed.
- Do not model Antigravity memory as a vector store or as a Codex-style remember/learn pipeline without stronger evidence.

## Open

- Populated knowledge/artifact schema and lifecycle.
- Whether the model can write memory directly or only through artifacts/tools.
- Whether knowledge_retrieval and knowledge_past_work are CLI-only server subagents or SDK-accessible roles.
