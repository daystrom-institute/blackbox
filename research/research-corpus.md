---
title: "Research Corpus"
kind: research-hub
corpus: blackbox-research
topic:
  - research-corpus
brief: "Top-level map for the Blackbox research corpus: a point-in-time, evidence-graded study of the external problem space (reference harnesses, provider APIs, protocols) that feeds the design corpus. Distinct from design/ — research records what the world does and why; design records what we will build. Lists the research tracks and the conventions that bind researcher agents."
---

# Research Corpus

This directory holds **research** records for Blackbox — a graded, point-in-time
study of the external problem space that informs what we build. It is a sibling
of [`design/`](../design/design-corpus.md), not a replacement:

- **`design/`** answers *"what will Blackbox do, and how?"* — design records,
  work-tracking, as-built notes.
- **`research/`** answers *"what does the world already do, and why?"* —
  reverse-engineering, interop study, competitive analysis, protocol archaeology.

Research feeds design. The bro-harness canonicalization work
(`design/bro-harness/compaction-canonical-anthropic.md`,
`bro-harness-api-robustness.md`) is the proof-of-concept: those docs mined the
Claude Code binary for idioms, then mapped them onto our harness. The research
corpus formalizes that activity so it can run continuously, in parallel, by
many agents, without re-deriving provenance or conventions each time.

## Tracks

A **track** is a coherent research area. Each track owns a hub note that doubles
as its charter (the dimensions it studies + the contract for agents working it).

- [Harness](harness/harness-tracks.md) — analysis of agentic coding CLIs
  (Claude Code, Codex, Gemini/Antigravity, Vibe, …): their transports, agent
  loops, context assembly, tool surfaces, and the full suite of agent-facing
  functionality. **First and currently only seeded track.**

Candidate future tracks (not yet seeded): provider-apis (raw wire contracts
across vendors), protocols (MCP, ACP, stream-json envelope evolution),
model-behavior (effort/thinking/steering response across model families).

## Why a separate corpus

`corpus: blackbox-research` keeps these docs out of `design/`'s lifecycle
tooling (`list-design-docs.sh` only sweeps `kind: design`) and gives Obsidian a
distinctly-colored subgraph. Research docs use research-specific `kind`s and a
research-specific `status` lifecycle (see the harness charter), because a
research leaf is never "proposed/archived" — it is `stub → researching →
enriched → verified`, and its claims carry inline confidence tiers.

## Conventions (corpus-wide)

- Hub-note filenames are descriptive, never generic `INDEX.md` — generic names
  create low-signal Obsidian graph nodes. (Same rule as the design corpus.)
- Provenance lives **once** per subject (on its version snapshot), never
  repeated in every leaf.
- Reverse-engineering is for **interop understanding**. Adopt structure and
  idioms; do not paste proprietary prompt prose verbatim into shipped harness
  code. The vault is the evidence store; the design corpus is where idioms get
  synthesized into our implementation.
- Prefer enriching the source leaf over summarizing its details in a hub.
