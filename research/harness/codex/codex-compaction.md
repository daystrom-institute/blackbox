---
title: "Codex · Compaction"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: compaction
version: "main@8aae858958"
last_verified: "main@8aae858958"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - compaction
brief: "Codex retains remote-v2, remote-v1, and inline compaction, and now treats compaction as a context-window boundary with UUIDv7 first/previous/current lineage, model-facing remaining-token and new-context tools, persisted World State reconstruction, bounded safe-suffix rollout loading, and configurable fallback prompt/buffer settings for auto-compaction."
---

# Codex - Compaction

See axis: [Compaction](../compaction.md) and snapshot:
[Codex main@8aae858958](codex-main-8aae858958.md).

## Finding

The three 0.136.0 implementation variants remain: streamed remote v2, the
`/responses/compact` remote path, and inline summarization. The current design
adds an explicit **context-window lifecycle** around every replacement.

**Confidence: high.** The window state, compact paths, rollout reconstruction,
and model-facing tool schemas are open source at the captured revision.

### Window identity and lineage

Every context window receives a UUIDv7 identity. The session tracks the first,
previous, and current window IDs. Compaction and explicit context rollover
advance the chain; compacted rollout items carry lineage so resume can restore
it. The model may receive a compact `<context_window>` guidance fragment with
thread and lineage identity.

This distinguishes "the same thread" from "the same active model window," which
is important when long work spans several compactions.

### Model-facing agency

`get_context_remaining` returns the current remaining token count or null when
unavailable. `new_context` requests a new context window and explicitly does not
clear or reset environment state. The request is consumed by the normal turn
loop and reuses the history-replacement machinery rather than inventing a second
reset path.

### Reconstruction and bounded loading

World State snapshots and patches are reconstructed alongside compacted
history, so mutable model-visible state survives a new window without blind
full reinjection. Rollout loading can scan backward to the latest safe
compaction checkpoint and load a bounded suffix. It falls back to full history
when the checkpoint cannot safely reconstruct the context.

### Auto-compaction fallback

At the captured head, model settings include an optional fallback prompt and
fallback buffer-token allowance for auto-compaction. The fallback prompt is
bounded to 2 KiB. This keeps fallback behavior model-configurable without
allowing an unbounded prompt addition.

The earlier remote-v2 retention budget, remote-v1 fit trimming, inline summary,
pre-sampling trigger, and compact hooks remain part of the implementation.

## Evidence

- `codex-rs/core/src/state/auto_compact_window.rs` - window state and lineage.
- `codex-rs/protocol/src/protocol.rs` and `compacted_item.rs` - persisted window
  identity.
- `codex-rs/core/src/tools/handlers/get_context_remaining_spec.rs` and
  `new_context_window_spec.rs` - model-facing surface.
- `codex-rs/core/src/session/rollout_reconstruction.rs` - safe reconstruction.
- Commits `5c12034e42`, `d1209bddfc`, `592467fb96`, and `8aae858958`.

## Vs the axis

Compaction should be modeled as a transition between identified context windows,
not merely a destructive history rewrite. The thread persists, environment and
World State persist, and the model can reason about remaining capacity and ask
for the boundary explicitly.

## Open

- The acceptance policy for model-requested rollover is configuration-dependent
  and should not be inferred from the schema alone.
- Remote provider behavior remains distinct from the inline textual-summary
  path even though both advance the same window lineage.
