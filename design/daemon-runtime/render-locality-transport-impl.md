---
title: "Project render locality: authorized view, checkout-owned projection"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
  - bro-harness
  - knowledge
tags: [locality, render, knowledge, workspace, transport]
brief: "Keep bbox_render model-facing while moving project-file writes into the bound checkout owner. The corpus supplies a bounded authorized published/own/all snapshot; the harness invokes the shared renderer locally and returns an exact path-free receipt."
---

# Project render locality

> **Status: the managed route and strict per-project cutover are implemented
> and workspace test-verified as of 2026-08-09.** Applying a production marker
> remains a separate operator-authorized ceremony; this document claims no
> production marker. Bridge, uncovered, and `LegacyLocal` project renders keep
> their named compatibility adapter.

## 0. Outcome

For a managed workspace-bound harness:

1. `bbox_render` remains the model-facing tool and keeps its public schema;
2. the corpus selects the exact `published`, `own`, or `all` knowledge view and
   returns a bounded path-free render plan;
3. the harness validates that plan against its bound published scope and
   workspace identity, invokes the shared `bbox-knowledge` renderer, and may
   write only fixed provider filenames at its bound project root;
4. the harness returns projection hashes, byte counts, local `PROJECT.md`
   presence, and write/refusal dispositions—never a checkout root or file
   body; and
5. the daemon reconstructs the current plan, validates every receipt field,
   and records durable completion evidence without acquiring a checkout.

Global rendering remains daemon/operator-host local. For `scope=both`, the
daemon performs the global half first, then the harness performs the project
half, preserving the existing operation order and combined response.

## 1. Executable owner map

- `src/tools/render.rs` authenticates the workspace binding, resolves a stable
  project identity without opening a checkout, selects the explicit knowledge
  view, normalizes project rows to a transport-only scope key, and returns the
  plan. Completion re-derives that plan and validates the receipt.
- `crates/bro-harness/src/locality.rs` wraps only the qualified daemon
  `bbox_render` capability in a live managed workspace. It strips any
  caller-supplied transport field, keeps an absolute project path local, and
  delegates global-only calls unchanged.
- `crates/bbox-knowledge/src/knowledge.rs` owns the plan, receipt, validation,
  pure projection seam, fixed provider target mapping, and checkout-local
  execution. The same renderer continues to power ordinary render and the
  candidate-tree `render --check` gate.
- `crates/bbox-indexing/src/render_locality_observations.rs` persists the
  latest exact completion for each project and `published`/`own`/`all` view.
  Only identity, counts, dispositions, and receipt checksums are durable.
- `crates/bbox-indexing/src/render_locality_cutover.rs` requires successful
  all-provider non-dry-run completions for every view, unique producer
  assignment, stable catalog authority, and an unchanged project-specific
  `RenderFileProvider` checkout baseline for at least 300 seconds. It then
  writes a checksummed marker that startup loads fail-closed.

## 2. Fixed contracts

### RL-D1: transport carries selected knowledge, never a checkout path

`ProjectRenderPlanV1` binds version, project id, published scope, workspace
id, provider selection, dry-run flag, normalized request scope, explicit view,
authorized project entries, and bounded diagnostics. Project entries are
rewritten to a constant transport scope; every entry must carry the plan's
project id. Count, diagnostics, and encoded plan bytes have hard bounds.

The wrapper converts a path that resolves to its bound project into the opaque
`$bound-workspace` selector before calling the daemon. Stable project ids and
aliases may still cross as selectors. Another path, a moved root, or a
workspace/scope mismatch refuses locally.

### RL-D2: the shared renderer remains the byte authority

The harness constructs a detached authorized `Knowledge` view and invokes the
same project renderer used by the daemon compatibility path. Projection
generation has a pure seam parameterized by the local nonempty-`PROJECT.md`
fact, so the daemon can independently validate every provider hash without
seeing the project root.

Only these project targets are valid:

- `CLAUDE.md` for `claude`;
- `AGENTS.md` for `agents`, `codex`, or `vibe`; and
- `GEMINI.md` for `gemini`.

Unknown or path-shaped provider names fail before a target is joined. Existing
hand-authored provider files retain the established refusal behavior; the
receipt records that refusal and cannot satisfy the positive-control cutover
gate.

### RL-D3: view semantics are explicit and pinned

A workspace plan defaults to `own`; callers may request `published`, `own`, or
`all`. The selected view is part of the plan and completion evidence. Tests
prove published excludes provisional variants, own replaces the logical row
with the bound workspace variant, and all carries published plus every valid
provisional variant under the existing degradation rules.

The transport does not reload `.bbox/knowledge` independently and pretend it
is a published view. Published and provisional authority continues to come
from the corpus knowledge-source and overlay model.

### RL-D4: completion is exact but path-free

`ProjectRenderReceiptV1` binds the same project/scope/workspace authority and
contains one fixed-filename projection record per selected provider. Each
record carries disposition, expected SHA-256, and expected byte count. The
daemon reconstructs the plan, recomputes projections from the shared
renderer, and rejects stale plans, provider cardinality drift, mismatched
bytes, or impossible dispositions before recording completion.

The user-facing result stays local because it includes the local path. Only
the receipt crosses back to the daemon.

### RL-D5: cutover is measured and per project

Offline preflight accepts an explicit Published-project set. For each row it
requires the latest successful all-provider, non-dry-run, no-refusal receipt
for all three views and captures the exact `RenderFileProvider` target
counters. Apply waits at least five minutes and refuses changed catalog bytes,
producer assignment, completion evidence, or checkout counter. The marker is
checksummed; corrupt bytes fail daemon startup.

After the marker is loaded, an unbound render for a covered project resolves
stable identity and refuses before the checkout broker. A managed bound call
already requires locality transport. Losing the producer, binding, source, or
receipt never reopens the daemon adapter.

## 3. Compatibility and parity

Catalog compatibility render remains available for uncovered projects and is
gated directly on `render_output`; it no longer takes an unrelated
`repo_mutation` capability. Accepted catalog rows render by stable project id
rather than disappearing behind the historical path-only filter. Bridge
render keeps its existing worktree/base-path behavior.

The test gates cover:

- caller transport stripping and absence of checkout roots on the wire;
- actual writes through the shared renderer under a canonical bound root;
- fixed-target confinement and provider path-injection refusal;
- published/own/all plan contents;
- exact candidate-tree check parity;
- zero daemon checkout observations for plan and completion;
- positive uncovered catalog compatibility; and
- checksummed marker, mandatory quiet window, changed-counter refusal, runtime
  projection, and covered pre-broker refusal.

## 4. Non-goals

- No arbitrary output filename or project root in the transport.
- No second renderer in the harness.
- No collapse of published/own/all into a working-tree-only approximation.
- No movement of global render authority into a remote workspace.
- No production marker merely because the code and tests landed.
- No implicit retirement of bridge, uncovered, or `LegacyLocal` adapters.

## 5. Parent-plan effect

Project render is no longer a remaining checkout reach-in for explicitly
marked Published projects. The collected project-source successor is now
implemented in
[code-source-locality-cutover-impl.md](code-source-locality-cutover-impl.md).
