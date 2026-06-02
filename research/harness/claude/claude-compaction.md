---
title: "Claude · Compaction"
kind: research-finding
corpus: blackbox-research
track: harness
harness: claude
axis: compaction
version: "2.1.160"
last_verified: "2.1.160"
status: enriched
confidence: mixed
topic:
  - harness
  - claude
  - compaction
brief: "Claude Code 2.1.160's compaction model, reverse-derived in the bro-harness canonical-compaction work: a purely client-side buffer rewrite (stateless transport) that picks a split point, summarizes the prefix into one synthetic message, and rebuilds as [summary, ...tail]. Ships two summarizer prompts (continuing-session vs ran-out-of-context) sharing an 8-section skeleton. Prompt structure is high-confidence (verbatim literals); the trigger/threshold math is medium (decoded minified JS)."
---

# Claude · Compaction

> **Provenance.** `design/bro-harness/compaction-canonical-anthropic.md` —
> reverse-derived from the 2.1.160 binary. **Prompts: high** (verbatim string
> literals). **Threshold math: medium** (decoded minified JS, mangled identifiers
> `rN_`/`tqH`/`mg8`; constants & structure reliable, call graph best-effort). See
> [snapshot](claude-2.1.160.md).

See the axis: [Compaction](../compaction.md).

## Shape (high)

Anthropic Messages is **stateless**, so compaction is client-side:

1. Pick a **split point** in the message history.
2. **Summarize** the prefix `[..split]` into one synthetic message.
3. **Rebuild** as `[summary, ...tail]` and continue.

Session recap is a separate feature from this compaction path. `/recap` and the automatic return-from-away recap run a one-turn no-tools `away_summary` generation; the automatic path appends a short `system` status message, while manual `/recap` displays local slash-command output. Neither path picks a split point, replaces history, or rebuilds the message buffer. Treat recap as context/status injection, not context-window compression.

## Two summarizer prompts (high)

Selected by situation, sharing one section skeleton; framing and final two
sections differ:

- **Variant A — partial / "continuing session"** (real messages will follow the
  summary). Framing: the summary is placed at the start of a continuing session;
  newer messages follow. Final sections retrospective: **8. Work Completed**,
  **9. Context for Continuing Work**.
- **Variant B — full / "ran out of context"** (hard compaction). Framing: *"create
  a detailed summary of the conversation so far …"*. Final sections
  forward-looking: **8. Current Work**, ….

*(Adopt the structure/idiom; do not paste the proprietary prompt prose into
shipped code — charter §8.)*

## Trigger / threshold (medium)

Threshold math decoded from minified JS — constants and structure reliable, exact
call graph best-effort. Treat as "how CC appears to do it," not a bug-for-bug
spec. The proactive-vs-reactive characterization and constants live in the
canonical doc.

## Open

<!-- TODO(mine): exact threshold constants and the trigger call graph (re-mine to
raise medium→high where possible); the full 8-section skeleton verbatim; split-
point selection heuristic; verbatim-retention tail sizing. Compare against the
Codex/Responses server-side model (brodex-compaction.md) at the axis level. -->

## Feeds

`design/bro-harness/compaction-canonical-anthropic.md`. Sibling transport:
`design/bro-harness/brodex-compaction.md` (OAI Responses, server-side).
