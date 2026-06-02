---
title: "Antigravity · Memory & Persistence"
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
brief: "agy memory = BRAIN ARTIFACTS: ~/.gemini/antigravity/brain/<uuid>/artifacts/<name>.md + .metadata.json (versioned), surviving across sessions; knowledge_retrieval / knowledge_past_work subagents search them; MemoryConfig injects user memories into the prompt. Artifact-based, not a vector store. Import path Antigravity 2.0 → SQLite → CLI."
---

# Antigravity · Memory & Persistence

> Mined from the `agy` v1.0.4 Go binary (`strings` ~500K lines) + `~/.gemini/` config + docs/CHANGELOG by DeepSeek-v4-pro bros, 2026-06-02. **Caveat:** agy is a THIN gRPC client to Google's server-side "cortex" engine — tools/loop/compaction run server-side, so confidence is capped at *medium* for anything not a verbatim binary string or a live config file.
See axis: [Memory & Persistence](../memory-persistence.md) · snapshot: [Antigravity 1.0.4](antigravity-1.0.4.md).

**Finding.** Cross-session memory is **artifact-based** ("brain"): `~/.gemini/antigravity/brain/<uuid>/artifacts/<name>.md` + `.metadata.json`, versioned, surviving sessions (3 artifacts in 1 brain dir on this host). Dedicated subagents `knowledge_retrieval` and `knowledge_past_work` search prior work. `MemoryConfig{GetAddUserMemoriesToSystemPrompt, GetNumMemoriesToConsider}` injects user memories. No "remember"/"memory" command and no vector-DB strings in the binary (unlike the 2.0 GUI). Import path: Antigravity 2.0 → SQLite → CLI.

**Evidence.**
- `~/.gemini/antigravity/brain/3e44435a-…/artifacts/*.md + .metadata.json` (versioned)
- `MemoryConfig{GetAddUserMemoriesToSystemPrompt,GetNumMemoriesToConsider}`
- subagents `knowledge_retrieval`, `knowledge_past_work`

**Vs the axis.** Confirms cross-session durable memory — but as **versioned artifacts + retrieval subagents** rather than codex's extract→consolidate text pipeline or Claude's auto-dream. Three distinct memory architectures now across subjects.

## Open
<!-- Whether the model can write brain artifacts directly (MemoryToolConfig) vs only planning writes them. -->
