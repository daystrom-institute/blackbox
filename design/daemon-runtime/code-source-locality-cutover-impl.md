---
title: "Code-source locality cutover: collected authority, no checkout fallback"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
  - indexing
tags: [locality, code-source, collector, indexing, cutover]
brief: "Make a verified collected generation authoritative for selected Published projects, close LocalProjectWalk and local cutback, and preserve transcript tool edges from immutable collected blobs."
---

# Code-source locality cutover

> **Status: implemented and workspace test-verified as of 2026-08-09.** The
> checked-in code contains the evidence store, offline cutover, startup and
> reload fences, pre-broker refusal, collected tool-edge attribution, and
> recovery tests. Applying a production marker remains an operator-authorized
> ceremony. Bridge, uncovered, and `LegacyLocal` projects remain outside this
> cutover.

## 0. Outcome

For an explicitly selected Published project:

1. one configured producer remains bound to the project's exact published
   scope;
2. one current v2 collected generation is active in both the code-source store
   and workspace manifest;
3. successful daemon startup recovery and a successful full rebuild record
   exact evidence for that same generation;
4. offline preflight captures those controls and the project's
   `LocalProjectWalk` counters, then apply requires at least 300 seconds with no
   counter, catalog, assignment, generation, or recovery-evidence drift;
5. the checksummed marker makes collected transport authoritative, so runtime
   startup and config reload refuse assignment or generation loss and the
   checkout broker refuses `LocalProjectWalk` before path resolution or
   observation; and
6. transcript project stamps and file-tool edges continue to resolve from the
   verified immutable generation blobs. The attachment path is used only as a
   lexical transcript namespace and is never read.

The cutover deliberately does not return a governed project to local source.
Removing its producer assignment is an invalid configuration, not cutback.

## 1. Executable owner map

- `crates/bbox-indexing/src/code_source_locality_observations.rs` stores the
  latest exact `StartupRecovery` and `FullRebuild` evidence for every healthy
  current v2 activation.
- `crates/bbox-indexing/src/code_source_locality_cutover.rs` owns preflight,
  apply, verify, marker validation, assignment fencing, and live generation
  validation.
- `src/server/code_source.rs` records startup evidence after recovery, rejects
  governed assignment drift before swapping a reloaded snapshot, installs the
  broker policy, and removes local cutback from governed reconciliation.
- `crates/bbox-indexing/src/index/writer_actor.rs` prevents governed source
  plans from requesting `LocalProjectWalk`, including full rebuilds.
- `crates/bbox-indexing/src/index/reindex.rs` records full-rebuild evidence only
  after the index and sidecar publication commits. It also constructs
  collected transcript attribution from the exact active activation and
  generation.
- `crates/bbox-corpus-index/src/index/tool_edges.rs` resolves collected file
  calls lexically, verifies the generation blob, chunks those bytes under the
  active snapshot id, and emits the same project-relative edge metadata. It
  never canonicalizes or reads the governed checkout.
- `src/bin/blackbox.rs` exposes the offline
  `project-catalog code-source-locality-cutover` command.

## 2. Fixed contracts

### CS-D1: evidence is generation-exact

An observation binds project id, published scope, producer, generation,
selector, snapshot, document count, and entity inventory checksum. Startup and
full-rebuild controls must name the same values as the live activation. A
legacy row, pending cutback, inactive generation, or workspace-manifest
disagreement cannot mint evidence.

The full-rebuild control is written only after the Tantivy commit and sidecar
publication succeed. A failed or partial rebuild therefore cannot satisfy
preflight.

### CS-D2: apply is measured and offline

Preflight accepts explicit Published project ids and writes a reviewable
report. Apply reopens every authority and refuses unless:

- the minimum 300-second window elapsed;
- catalog epoch and checksum are unchanged;
- configured producer ownership is unchanged;
- the exact active collected generation is unchanged;
- startup and full-rebuild evidence still describe that generation; and
- every project-specific `LocalProjectWalk` target counter is unchanged.

Verify revalidates the checksummed marker, live catalog/config/store state, and
the post-cutover counter baseline. A later local walk is therefore a visible
verification failure even if another bug bypasses the runtime policy.

### CS-D3: governed means collected or fail closed

The runtime marker is loaded before the listener binds. It requires the exact
scope-to-project-to-producer assignment and exact active generation. Dynamic
reload performs the assignment check before replacing the live snapshot or
advancing its revision.

The broker policy refuses a governed `LocalProjectWalk` before authority
selection, path discovery, or observation. Source planning separately refuses
to request the capability. The reconciler keeps the collected assignment and
does not schedule or probe local cutback.

### CS-D4: corpus features do not depend on checkout bytes

Before this cutover, incremental transcript ingestion needed a local root to
stamp `base_project_id` and emit `READ_FILE`, `EDITED_FILE`, and `RAN_BASH`
edges. Simply removing the lease would have silently degraded search and graph
coverage.

For a governed attached project, the upper layer now supplies:

- stable project id;
- the attachment root only as a lexical namespace for historical transcript
  cwd and file arguments;
- exact activation snapshot and generation head;
- exact generation manifest; and
- a handle that verifies immutable content-addressed blobs.

File edges are chunked from those verified blobs and receive V2 entity refs for
the active snapshot. Bash edges and base-project stamps use the same lexical
project match. A missing manifest path is diagnosed and skipped. It is never
read from the checkout and never reassigned to another project.

Remote-only projects have no local transcript namespace to map. Their collected
project documents remain complete, while a path event with no stable project
namespace remains bounded and unresolvable rather than being guessed.

## 3. Test gates

The executable gates include:

- a real catalog-mode v2 store, activation, workspace manifest, both recovery
  observations, preflight, elapsed quiet window, apply, and verify;
- post-cutover `LocalProjectWalk` counter drift refusal;
- corrupt marker checksum refusal;
- producer assignment removal refusal with the prior live snapshot and
  revision left intact;
- broker refusal before an observation is written;
- source planning with no local-walk request during governed full rebuild; and
- a collected tool edge resolved while the named checkout does not exist,
  including V2 target, content hash, commit anchor, and base-project stamp.

## 4. Compatibility and non-goals

- No production marker is implied by landing code or passing CI.
- No implicit conversion of `LegacyLocal`, bridge, or uncovered projects.
- No weakening of the current v2 activation and workspace-manifest agreement.
- No local fallback after producer, scope, generation, or marker authority
  loss.
- No transport of absolute checkout paths, file bodies, or blob content beyond
  the existing daemon-local indexing boundary.
- Git history, provenance, knowledge, blame, and render keep their independent
  transport markers and compatibility decisions.

## 5. Parent-plan effect

For marked Published projects, project-file indexing is no longer a daemon
checkout reach-in and local source is no longer a cutback destination. The
remaining locality work is operational coverage, explicit migration or
retirement of the named compatibility projects, and then the corpus off-host
move with all shared stores and MCP surfaces validated in their deployed
topology.
