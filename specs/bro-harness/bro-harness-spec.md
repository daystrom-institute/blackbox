---
title: "Bro-Harness Spec — Domain Charter"
kind: spec-hub
corpus: blackbox-spec
domain: bro-harness
topic:
  - specs
  - bro-harness
  - charter
brief: "Hub and charter for the bro-harness spec domain: the canonical, source-grounded contracts the custom headless agent must satisfy — the agent loop and the API transports (Anthropic Messages, OpenAI Responses, openai-chat) plus the common Claude stream-json output envelope. Defines the contracts in scope, the source-tier grading, the conformance-wiring contract, the code+design+research inversion (backfill) shape, and the spec-author pickup contract."
---

# Bro-Harness Spec — Domain Charter

This is the **nav waypoint and charter** for the bro-harness canon. Start here.
It defines *what* contracts this domain specifies, *how* clauses are graded and
wired to the three faces, and the *contract* a spec-author follows when picking
up a leaf.

> **Why this domain first.** bro-harness (`crates/bro-harness`) emits the Claude
> stream-json envelope and slots into the daemon's dispatch seam, carrying
> GLM/DeepSeek (Anthropic transport), Brodex (OpenAI Responses), and VibeBh
> (openai-chat / Mistral). Its quality bar is "rock-solid, super-stable, highly
> idiomatic." The Anthropic/OAI **canonicalization** already underway in
> `design/bro-harness/` (jittered backoff, SSE idle timeout, `pause_turn` resume,
> server-tool-block preservation, structured compaction) is exactly the activity
> this corpus formalizes: turning hard-won as-built behavior into a normative
> as-specified contract, grounded in the vendor APIs and the harness research.

## Invariant (binds every clause)

bro-harness shares code with the daemon (workspace crates) but **never has a
runtime dependency on it** — no MCP/RPC backchannel from harness to daemon. The
only daemon↔harness contract is the Claude stream-json envelope on stdout. Any
clause that would require the harness to call the daemon is out of scope and
wrong by construction. `[derived]`

## Contracts in scope

The spec leaves under this domain (`draft` = stub awaiting mining):

| Spec | Leaf | Covers |
|------|------|--------|
| Agent loop | [agent-loop.md](agent-loop.md) | turn boundary, stop detection, parallel tool calls, tool-result threading, steering/interrupt mid-flight, `pause_turn`/resume, recursion guard. |
| Transports | [transports.md](transports.md) | the wire contract per backend (Anthropic Messages, OpenAI Responses, openai-chat) + the common stream-json output envelope: retry/backoff, SSE idle timeout, cache TTL, feature flags, role-alternation repair. |

Candidate future leaves (not yet seeded): compaction (the canonical summarize
model), tool-surface (built-in tool contracts), hooks.

## Source-tier grading

Each normative clause is tagged with the authority it rests on (no RFC-2119
keywords; the tier carries the weight):

- `[standard]` — external normative standard (RFC, ISO, protocol spec).
- `[vendor]` — vendor API documentation or an observed wire contract
  (Anthropic Messages, OpenAI Responses, Mistral chat).
- `[research]` — a `research/harness/` finding (cite the leaf + version).
- `[derived]` — a Blackbox house invariant, not externally mandated.

A clause with no defensible tier is not canon yet — leave it in the Open section.

## Conformance wiring (the three faces)

Every spec leaf carries a **Conformance** section that links each clause to:

- **code** — the anchor in `crates/bro-harness/src/…` that satisfies (or
  violates) it;
- **intent** — the `design/bro-harness/…` doc that proposed it;
- **evidence** — the `research/harness/…` finding that grounds it.

A clause whose code anchor diverges from the spec is a **conformance gap** — note
it; that is the spec earning its keep.

## Backfill shape (inverting code + design + research into canon)

To enrich a `draft` stub into a `specified` leaf:

1. **Read the research finding** (`research/harness/<axis>.md`) for the
   descriptive cross-harness model — what the mature harnesses actually do.
2. **Read the design docs** (`design/bro-harness/…`) for our intent and the
   as-built reconciliation (e.g. `compaction-canonical-anthropic.md`,
   `bro-harness-api-robustness.md`, `brodex-responses-deep-dive.md`).
3. **Read the code** (`crates/bro-harness/src/…`) for the as-built truth, and
   the canonicalization commits for why it is the way it is.
4. **Invert** those three into normative clauses: state what the harness *must*
   do, tag each clause with its source tier, and wire it in Conformance to the
   code/intent/evidence anchors.
5. **Set frontmatter**: bump `status`, list `sources`, set `last_reviewed`.
   Leave an **Open** section for unresolved clauses.

A leaf is **`ratified`** when its clauses are sourced, the Conformance section is
complete, and the canon has been accepted as the authority design + code defer
to — not merely when prose exists.

## Spec-author contract (how to pick up a leaf)

When you take a `draft` (or refresh a `specified`) leaf: follow the backfill
shape above, keep clauses atomic and tier-tagged, cite verbatim vendor/standard
text only as evidence (adopt the contract, not proprietary prose), and update
this charter's contracts table if you add a leaf.
