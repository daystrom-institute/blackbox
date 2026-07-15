---
title: "Codex · Context Management"
kind: research-finding
corpus: blackbox-research
track: harness
harness: codex
axis: context-management
version: "main@8aae858958"
last_verified: "main@8aae858958"
status: enriched
confidence: high
topic:
  - harness
  - codex
  - context-management
brief: "Codex has generalized its reference-context diff engine into persisted model-visible World State: stable section IDs, typed serializable snapshots, RFC 7386 merge patches, absent/unknown/known restore semantics, retained-fragment reconciliation, and one-time reinjection when a known section vanished from retained history. Environment, AGENTS.md, apps, plugins, extensions, and executor skills use the mechanism."
---

# Codex - Context Management

See axis: [Context Management](../context-management.md) and snapshot:
[Codex main@8aae858958](codex-main-8aae858958.md).

## Finding

The 0.136.0 implementation already diffed current turn context against a
reference item. Current Codex promotes that idea into **World State**, a typed
ledger of the state the model is expected to know.

**Confidence: high.** The section trait, rollout representation, reconstruction,
and tests are open source at the captured revision.

### Section contract

Each section owns:

- a stable persisted identifier;
- a serializable typed snapshot containing only comparison data;
- recognition of legacy fragments where migration is needed;
- optional recognition of the section's rendered fragment in retained history;
- `render_diff(previous)` over an explicit previous state of absent, unknown,
  or known.

The World State object preserves section order for rendering while its compact
snapshot is keyed by stable IDs. Extension contributors can add sections through
the same contract without editing core assembly code.

### Persistence and reconstruction

Rollouts store full snapshots or RFC 7386 merge patches. Reconstruction applies
the patches to recover the model-visible baseline on resume, fork, rollback, and
after compaction. Legacy history without a typed snapshot is represented as
unknown rather than guessed.

Retained-history reconciliation closes a second failure mode: a persisted
snapshot may say the model knows a section while compaction or migration removed
the corresponding rendered fragment. Sections with a retained-fragment matcher
detect that mismatch and re-inject once.

### Current sections

Environment, AGENTS.md, app instructions, plugin instructions, extension state,
and executor skill instructions use World State. AGENTS.md is recomputed when
the execution environment changes rather than remaining frozen at thread start.
App and plugin guidance can react to MCP and environment readiness.

The older context behaviors remain: role-split developer/user fragments,
initial assembly, model-switch continuity, `AGENTS.override.md` precedence, and
small subsequent updates rather than full resend.

## Evidence

- `codex-rs/core/src/context/world_state/mod.rs` - typed section and snapshot
  contracts.
- `codex-rs/core/src/context/world_state/` - built-in sections and rendering
  tests.
- `codex-rs/ext/extension-api/src/contributors/world_state.rs` - extension
  contributions.
- Commits `3e51b46eba`, `fa036d39aa`, `a74771340d`, `ab80d4d484`,
  `723b23efd0`, and `f2f80ef442`.

## Vs the axis

The cross-harness question is no longer only "what fires when?" Mature context
management also needs a durable answer to "what does the model already know?"
Codex makes that answer typed, replayable, and independently diffable by
section. This is stronger than a single monolithic reference-context item.

## Open

- Not every model-visible fragment has necessarily migrated into World State;
  the extension seam allows staged adoption.
- The ideal retention matcher for non-text or provider-native items remains
  subject to each item type.
