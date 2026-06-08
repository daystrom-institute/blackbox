---
title: "Specs Corpus"
kind: spec-hub
corpus: blackbox-spec
topic:
  - specs-corpus
brief: "Top-level map for the Blackbox specs corpus: the CANON — what each subsystem should actually be and do, stated normatively and grounded in external standards, vendor API contracts, and graded research. The third sibling of design/ (intent) and research/ (description). Specs are backfilled by inverting code + design + research into the canonical as-specified; design defers to specs as the authority."
---

# Specs Corpus

This directory holds the **canon** for Blackbox: the normative answer to *"what
should this subsystem actually be and do?"*, grounded in the standards, vendor
API contracts, protocols, and graded research that govern it. It is the third
sibling alongside [`design/`](../design/design-corpus.md) and
[`research/`](../research/research-corpus.md).

## The three faces

Most code artifacts have three faces. Each corpus owns one:

| Face | Corpus | Question | Mood |
|---|---|---|---|
| **intent** | [`design/`](../design/design-corpus.md) | *what did we ask for / will we build?* | working docs, lifecycle, exhaust |
| **code** | the tree | *what does it do?* | self-evident |
| **canon** | **`specs/`** (here) | *what should it be/do, per the standards?* | **normative**, source-grounded |

`design/` was meant to be canon but accreted into intent — working documents
thick with implementation detail and operational exhaust. Rather than rework it,
`specs/` captures the canon cleanly and separately.

## How the corpora relate

```
research/  (descriptive: what the world does, evidence-graded)
   │ feeds
   ▼
specs/     (normative: what WE must do, standard-grounded)   ◀── backfilled by
   │ constrains                                                  inverting code
   ▼                                                             + design + research
design/    (intent: what we'll build)
   │ realized in
   ▼
code
```

A spec is **prescriptive** where research is **descriptive**: research records
"Claude Code retries with jittered backoff" (a fact about the world); a spec
states "the Anthropic transport retries with jittered backoff and honors
`Retry-After`" (a contract our code must satisfy), citing the research finding
and the vendor API as its sources.

## Domains

A **domain** is a coherent specification area. Its hub note doubles as the domain
charter (the contracts in scope + the spec-author contract).

- [Bro-Harness](bro-harness/bro-harness-spec.md) — the core headless agent: the
  agent loop and the API transports (Anthropic Messages, OpenAI Responses,
  openai-chat) plus the common stream-json output envelope. **First seeded
  domain** — the canonicalization work already underway in
  `design/bro-harness/` is the proof-of-concept this corpus formalizes.

Candidate future domains (not yet seeded): mcp-surface (tool admission, deferred
tiering), knowledge-render (memory lanes + render invariants), orchestration
(dispatch, recursion guard, allocation).

## Conventions (corpus-wide)

- **`corpus: blackbox-spec`** keeps these docs out of `design/`'s lifecycle
  tooling (`list-design-docs.sh` sweeps only `kind: design`) and gives Obsidian a
  distinct subgraph. Same separation rationale as `research/`.
- **Hub-note filenames are descriptive**, never generic `INDEX.md` (low-signal
  graph nodes). Same rule as design + research.
- **Source-authority grading per clause.** Every normative clause is tagged with
  the authority it rests on (no RFC-2119 MUST/SHOULD keywords; the tier carries
  the weight):
  - `standard` — an external normative standard (RFC, ISO, protocol spec).
  - `vendor` — vendor API documentation or an observed wire contract
    (e.g. Anthropic Messages, OpenAI Responses).
  - `research` — grounded in a `research/` finding (cite the leaf).
  - `derived` — our own house invariant, not externally mandated.
- **Conformance wires the three faces.** Each spec carries a Conformance section
  linking clauses to the code anchor(s) that satisfy/violate them, the `design/`
  doc that is the intent, and the `research/` finding that is the evidence.
- **Status lifecycle:** `draft` (skeleton + source pointers) → `specified`
  (clauses written + sourced + coherent) → `ratified` (accepted as authority;
  design + code must conform). Re-spec across versions via `supersedes`.
- **Prefer enriching the source leaf** over summarizing its clauses in a hub.

## Frontmatter schema

The full frontmatter contract — the chassis fields shared by `design/`,
`research/`, and `specs/`, plus the per-corpus vocabulary matrix and the
`topic[]` convention — lives in
[`docs/corpus-frontmatter-schema.md`](../docs/corpus-frontmatter-schema.md).
That doc is the single source of truth for the chassis. This section is a
quick reference for the per-`specs/` vocabulary only.

```yaml
# Hub (specs-corpus.md, <domain>-spec.md)
kind: spec-hub
corpus: blackbox-spec
domain: bro-harness        # on domain hubs

# Spec (leaf)
kind: spec
corpus: blackbox-spec
domain: bro-harness        # the domain key
spec: agent-loop           # the contract key
topic: [specs, bro-harness, agent-loop]
status: draft|specified|ratified
sources:                   # the authorities this spec rests on
  - "vendor:Anthropic Messages API"
  - "research:research/harness/agent-loop.md"
supersedes: null
last_reviewed: "YYYY-MM-DD"
```

**Per-`specs/` vocabulary (observed in the seeded corpus):**

| Field | Hub | Leaf | Notes |
|---|---|---|---|
| `kind` | `spec-hub` | `spec` | required |
| `corpus` | `blackbox-spec` | `blackbox-spec` | required; matches the directory |
| `domain` | required (e.g. `bro-harness`) | required (e.g. `bro-harness`) | the domain key |
| `spec` | — | required (e.g. `agent-loop`, `transports`) | the contract key within the domain |
| `status` | — | required | one of `draft`, `specified`, `ratified`; only `draft` is observed in the current seeded corpus |
| `topic` | required | required | list, mirrors the directory path minus the corpus root; see the shared chassis doc for the convention |
| `sources` | — | required | list of authority strings, one of `standard:…`, `vendor:…`, `research:<path>`, `derived:…` |
| `supersedes` | — | optional | observed as `null` in the current seeded corpus |
| `last_reviewed` | — | required (recommended) | ISO-8601 date |

The full lifecycle arrows for `status` (`draft` → `specified` → `ratified`)
and the source-tier grading for `sources` are documented in **Conventions
(corpus-wide)** above.
