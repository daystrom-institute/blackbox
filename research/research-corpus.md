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

## Operating procedures

The repeatable procedures that run this program live in the top-level
[`prompts/`](../prompts/README.md) corpus: **REFRESH_ALL_CLIS** (operator-pointed
orchestrator that fans the mining lens over all CLIs) plus the dispatched-agent
lenses under [`prompts/agents/`](../prompts/agents/README.md) — **MINE_CLI**
(forward-mine one CLI against the axes) and **CLI_INVESTIGATOR**
(backward-discover missing axes). Agents and bros are pointed at these docs
rather than carrying baked-in lenses, so the procedures stay tweakable in one
place.

## Why a separate corpus

`corpus: blackbox-research` keeps these docs out of `design/`'s lifecycle
tooling (`list-design-docs.sh` only sweeps `kind: design`) and gives Obsidian a
distinctly-colored subgraph. Research docs use research-specific `kind`s and a
research-specific `status` lifecycle (see the harness charter), because a
research leaf is never "proposed/archived" — it is `stub → researching →
enriched → verified`, and its claims carry inline confidence tiers.

## Frontmatter schema

The full frontmatter contract — the chassis fields shared by `design/`,
`research/`, and `specs/`, plus the per-corpus vocabulary matrix and the
`topic[]` convention — lives in
[`docs/corpus-frontmatter-schema.md`](../docs/corpus-frontmatter-schema.md).
That doc is the single source of truth for the chassis. This section is a
quick reference for the per-`research/` vocabulary only.

```yaml
# Hub (research-corpus.md, harness/harness-tracks.md)
kind: research-hub
corpus: blackbox-research
track: harness                              # the track key
topic: [<corpus-root-segment>, …]           # mirrors the directory path

# Subject snapshot (one harness at one version)
kind: research-subject
corpus: blackbox-research
track: harness
harness: claude                             # one of: claude, codex, vibe, antigravity
version: "2.1.160"
platform: macos-aarch64
captured: "2026-06-02"
supersedes: null
status: enriched

# Axis (one axis across harnesses)
kind: research-axis
corpus: blackbox-research
track: harness
axis: builtin-tools                         # see the axis vocabulary below
status: stub|enriched                       # default = no field, only the controlled value

# Finding (one cell of the matrix: subject × axis)
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude                             # the matrix row
axis: builtin-tools                         # the matrix column
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: high                            # high | medium | mixed
```

**Per-`research/` vocabulary (observed in the current corpus):**

| Field | Hub | Subject | Axis | Finding | Notes |
|---|---|---|---|---|---|
| `kind` | `research-hub` | `research-subject` | `research-axis` | `research-finding` | required |
| `corpus` | `blackbox-research` | `blackbox-research` | `blackbox-research` | `blackbox-research` | required |
| `track` | required | required | required | required | the track key (only `harness` is seeded) |
| `harness` | — | required | — | required | the matrix row: `claude`, `codex`, `vibe`, `antigravity` |
| `axis` | — | — | required | required | the matrix column: `agent-loop`, `builtin-tools`, `compaction`, `context-management`, `hooks`, `mcp`, `memory-persistence`, `metatools`, `modes-personas`, `planning-goals`, `privilege-approvals`, `robustness`, `session-lifecycle`, `skills`, `subagents`, `transport` |
| `version` | — | required | — | required | the harness version the leaf was mined against (e.g. `"2.1.160"`, `"0.136.0"`, `"2.9.6"`, `"1.0.4"`) |
| `last_verified` | — | — | — | required | when the leaf was last re-verified against the live harness (same value as `version` when current) |
| `status` | optional | required | required (often) | required | controlled lifecycle: `stub` → `researching` → `enriched` → `verified`; only `stub`, `researching`, and `enriched` are observed in the current seeded corpus |
| `confidence` | — | — | — | required | one of `high`, `medium`, `mixed`; `high` on 53 of 60 findings |
| `platform` | — | required (subject) | — | — | the host platform the snapshot was captured on (`linux-x86_64`, `macos-aarch64`) |
| `captured` | — | required (subject) | — | — | the capture date (ISO-8601) |
| `supersedes` / `replaces` | — | optional | — | — | predecessor snapshot; observed as `null` in the current four subjects (and `replaces: gemini` once) |
| `generated_by` / `last_reviewed` | optional | — | — | — | provenance and review metadata; observed on the two research-hub outliers |
| `topic` | required | required | required | required | list, mirrors the directory path minus the corpus root; see the shared chassis doc for the convention |

The full lifecycle arrows for `status` (`stub` → `researching` → `enriched`
→ `verified`) and the inline confidence tier rules are documented in the
harness charter at
[`harness/harness-tracks.md`](../harness/harness-tracks.md).

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
