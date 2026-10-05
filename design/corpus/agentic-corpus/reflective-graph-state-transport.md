---
title: "Reflective graph state transport"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
tags:
  - reflective-graph
  - locality
  - knowledge-source
  - checkout-owner
brief: "How project-owned reflective graph documents reach a zero-checkout-authority corpus daemon and what graph reads see: graphs ride the knowledge-source publication transport as a third admitted lane, reads are published-only, structural validation runs on the committed tree (closeout merge gate and accepted view build), and v1 mutation stays file-first with later tool writes riding the checkout-owner lane."
date: 2026-08-12
---

# Reflective graph state transport

> **Status: partial.** The graphs lane on publication candidates, the graph
> pass in the candidate-tree merge gate, and published graph views built from
> accepted generations are landed on `beta/blackbox-v2`. Graph tool writes
> (section 5) are unbuilt. Names below come from those designs; reverify
> against the tree before building.

## 0. What this decides

The kernel leaves daemon materialization unspecified, and the salvage-era
implementation read `<project>/.bbox/graphs/` off the daemon host. The
production daemon has no checkout filesystem authority, so that path is dead.

1. Graph documents travel over the existing knowledge-source transport as an
   additive third lane, not a new wire.
2. Graph reads are published-only, as knowledge and gap reads are.
3. Structural validation runs on committed trees: the candidate-tree merge
   gate before refs move and the accepted view build after acceptance.
4. V1 mutation is file-first; a later tool write rides the checkout-owner
   mutation lane, never a daemon file write.

## 1. Transport

### 1.1 Graphs are a third lane on the existing descriptors

`.bbox/graphs/` is exactly the class of state the knowledge source transport
carries: committed, repo-owned, reviewable, project-scoped, edited in a
checkout the daemon cannot open. It rides the same contract.

- `PublicationCandidateDescriptorV2` adds a `graphs` lane beside `knowledge`
  and `gaps`, carrying committed graph documents at one full branch ref and
  exact commit. A V1 descriptor decodes with an empty `graphs` manifest.
- Route shapes are unchanged; `lane` gains the value `graphs`.

Lane atomicity follows the landed rule that knowledge and gaps are atomic
everywhere: a descriptor carries all three lanes, including explicit empty
manifests, and finalize publishes none on any lane's admission failure. The
rationale is stronger for graphs than for gaps: a record graph's evidence edges
reference knowledge entries and gap records from the same commit, so a
generation accepting knowledge without the graph facts asserted alongside it is
a state the checkout never had.

### 1.2 What the transport must learn

**Admitted subtree shape.** Knowledge and gap lanes are flat directories of one
JSON file per entry; the graph lane is two levels. A manifest path is relative
to `.bbox/graphs/` and must be exactly `<graph-id>/<file>`, where `<graph-id>`
matches the kernel charset (no `:`, no separator, no dot segment, bounded
length) and `<file>` is one of the three required fact files `schema.json`,
`vertices.jsonl`, `edges.jsonl`, or the optional descriptor `graph.json`.
The kernel's loader treats the descriptor as optional on both sides of the
wire: absent, the descriptor is synthesized deterministically; present, it is
parsed and consistency-checked, and it ships so accept-side validation sees
the same bytes the checkout validated. Other depths, other filenames, and
symlinks fail admission. Unknown files in a graph directory are rejected, not
ignored: dropping them silently makes the corpus view differ from the
checkout's for a reason no reviewer sees.

**Size and count bounds.** Knowledge entries are small by construction; a
`vertices.jsonl` is not. The lane needs ceilings on per-file bytes, per-graph
bytes, per-generation lane bytes, graphs per project, and decoded rows per
file, enforced at manifest admission and again at parse so an adversarial file
cannot pass a byte check and expand in memory. Ceilings are server config with
static maxima, failing closed when unset; defaults are tuning, not contract.

**Validation at accept.** Section 3.

**Graph pass in the merge gate.** The landed closeout gate builds a candidate
tree and runs a shared-implementation render check. It gains a graph pass:
every `.bbox/graphs/<graph-id>/` in that tree must parse and validate
structurally, so a broken graph never reaches a publication candidate. That
matters because accept-time rejection is candidate-fatal (section 3.2).

### 1.3 Scratch graphs never transport

`.bbox/local/graphs/` is host-local by the committed-versus-host-local split.
It appears in no manifest or candidate and has no published form, reachable
only by a checkout-confined reader in the owning workspace. Sharing a scratch
graph means moving its files under `.bbox/graphs/` and committing them; there
is no daemon-side promotion of scratch state.

## 2. Visibility

Every graph read surface serves the accepted publication generation's graph
set for the project, plus connector-managed source graphs, and nothing else:
`bbox_project_graph_list`, `bbox_project_graph_describe`,
`bbox_project_graph_validate`, exact vertex inspection, traversal, and
evidence bundling. `source` selects `published` or `connector`. A checkout's
uncommitted `.bbox/graphs/` edits are visible to no corpus read; they reach
readers after commit and acceptance. A workspace binding never selects what
a graph read returns.

A project with no accepted generation has no published graph set: a
scope-local hard error for an explicit query, an omission with diagnostics
for an aggregate.

## 3. Validation placement

### 3.1 Three error classes

| Class | Examples | Where detectable |
|---|---|---|
| Lane admission | path depth, unknown filename in a graph dir, symlink, byte or row ceiling exceeded, blob digest mismatch | producer capture, transport finalize |
| Document parse | malformed `schema.json`, malformed JSONL row, non-UTF-8, reserved top-level key misuse | merge gate, accept, view build |
| Graph structure | undeclared vertex type, non-`meta:VertexType` type, edge type with no endpoint declaration, endpoint type mismatch, duplicate vertex id, duplicate edge key, reserved namespace misuse, missing referenced vertex, property shape violation | merge gate, accept, view build |

### 3.2 Corpus-side, at accept

A publication candidate is validated in full before an accepted generation is
installed: admission, parse, and structure. Any failure rejects the candidate,
the accepted pointer does not move, and the prior accepted graph set keeps
serving, exactly as an invalid evidence binding leaves the prior generation
standing. Diagnostics name graph id, file, line where applicable, stable error
code, and message, and are visible on candidate status before the operator
advances the pointer.

Candidate-fatal rather than per-graph quarantine is the choice: an accepted
generation is one operator-reviewed commit, and admitting a subset of its
graphs would make "which graphs are accepted" a function of validator version
rather than of the reviewed tree.

### 3.3 Uncommitted edits are validated after commit

There is no checkout-side validator. An edit to `.bbox/graphs/` is validated
once it is committed: the closeout merge gate runs the graph pass over the
candidate merge tree before any ref moves, and the accepted view build
validates every graph of an accepted generation, reporting an invalid graph
through `bbox_project_graph_validate`. Scratch graphs are never validated
corpus-side, because the corpus never sees them.

## 4. Ref resolution

Published vertices resolve by the kernel ref family against the accepted
generation for the project's published scope. A vertex that exists only in a
checkout's working files is not found. Traversal stays inside one graph
generation, and cross-graph edges remain deferred by the kernel. Evidence
bundles record the per-graph `built_from` stamp for every included vertex and
edge.

## 5. Mutation

**V1 is file-first.** Edit `.bbox/graphs/<id>/*` in a checkout and commit; the
closeout merge gate validates the committed tree (section 3.3). The commit on
the configured ref is the publish gate; the checkout owner captures a candidate
and the daemon accepts it. The daemon never opens or writes a checkout path.

**If tool writes arrive later**, `bbox_project_graph_put_vertex` and
`bbox_project_graph_put_edge` ride the checkout-owner mutation lane in the
shape knowledge and gap writes took: the daemon validates the proposed result,
produces the exact replacement bytes, and enqueues a durable pending checkout
mutation the collector applies and acks. Human commit stays the publish gate.
Graph-specific constraints: one graph id per mutation and whole-file byte
replacement of the affected `vertices.jsonl` or `edges.jsonl` (never a row
append, because the kernel requires normalized state files rather than
append-only logs); refusal when the workspace has a pending transaction or the
result would fail structural validation; ids stay project-owned, so the daemon
validates and never mints them; and the write is path-constrained to
`.bbox/graphs/` and grant-scoped per producer.

**No agent self-service graph creation on the daemon.** Creating a graph id,
declaring a namespace, or authoring `schema.json` is a checkout action or an
explicit operator-authorized mutation, never an ambient outcome of a search,
inspect, or reasoning path. That is the kernel's read-paths-must-not-invent
rule carried into the transport. Connector-owned source graphs are a separate
authority plane, published by the producer that owns the remote observation,
and do not ride this lane.

## 6. Non-goals

- No new wire, route family, producer credential family, or store for graphs.
- No daemon-local mutable graph store, and no daemon read of a checkout path.
- No cross-generation graph merge.
- No full-text or vector indexing of graph vertices; the kernel defers it.
- No promotion of graph facts into knowledge or rendered
  memory; that stays explicit and operator-gated.
- No cross-machine scratch-graph visibility.
- No transport of uncommitted graph state.
- No graph-specific staleness clock.

## 7. Rejected alternatives

**Daemon-local graph store as authority** (agents call put tools, the daemon
owns canonical files, the checkout gets an export): reintroduces the checkout
filesystem authority the locality program removed, makes graphs unreviewable
and undiffable in git, and breaks the kernel's rule that committed graphs live
under the project rather than in hidden daemon state.

**A parallel graph-specific wire** (`/internal/graph-source/v1` with its own
descriptors, auth, CAS, journals, leases, recovery): duplicates five landed
subsystems for no semantic gain and gives graphs a second staleness clock that
can disagree with knowledge and gaps about the same commit, which is what the
atomic-lane rule exists to prevent.

**A separate non-atomic graph generation** (same descriptors, independent
finalize and accept): rejected for the evidence-edge reason in section 1.1.

**Graphs as ordinary project files over the code-source transport:** `.bbox` is
excluded from the project file walk by design, and graph documents need
accept-time structural validation that lane lacks.

**Ignoring unknown files inside a graph directory:** rejected in favor of
failing admission, so the corpus view cannot silently differ from the checkout.

## 8. Acceptance criteria

1. A committed graph reaches the corpus with no daemon filesystem access to any
   checkout, over the existing routes with `lane=graphs`.
2. A candidate failing graph admission, parse, or structure is rejected, the
   pointer does not move, and the prior accepted graph set keeps serving.
3. Every graph read surface serves the accepted generation and connector
   graphs only, for bound and unbound sessions alike.
4. A workspace edit is invisible to every corpus read until its commit is
   accepted.
5. Scratch graphs never appear in a candidate or a citable bundle.
6. The candidate-tree merge gate fails on a structurally invalid graph before
   any ref moves, and reports identical error codes and locations to
   accept-time validation on one tree.

## 9. Open questions

- **The published ref keys on a host-local id.** The kernel's
  `project_graph_vertex:<project-id>:<graph-id>:<vertex-id>` uses a project id
  that is a host realpath hash and does not travel, while the published scope is
  `(repo_id, bbox_root_relpath)`. Recommendation: key on the durable catalog
  project id and treat the path-derived value as a selector. The kernel owns
  that grammar, so the change belongs there, not shadowed here.
- **Ceilings and paging.** Where per-file and per-graph ceilings land, and
  whether a large graph needs manifest paging beyond the existing page contract.
- **Candidate-fatal versus per-graph quarantine at accept.** If projects
  accumulate graphs at differing maturity, one experimental graph blocking a
  knowledge publication may be the wrong tradeoff. Revisit with evidence.
- **Cross-lane reference checks.** Once cross-entity evidence endpoints exist,
  whether the merge gate should verify that graph edges referencing knowledge or
  gap ids resolve within the same candidate.

## 10. Relationship

- **Extends** [reflective-project-graph.md](reflective-project-graph.md) by
  filling the Implementation Boundary it deferred: how graph documents reach
  the daemon and what a read sees. It changes nothing in the fixed floor,
  storage shape, validation vocabulary, or tool surface.
- **Extends** the knowledge-source transport implemented by
  [knowledge-source-transport-impl.md](../../daemon-runtime/knowledge-source-transport-impl.md),
  reusing checkout identity, published scope, and `built_from` stamps from
  [checkout-identity-and-provisional-knowledge.md](../knowledge/checkout-identity-and-provisional-knowledge.md)
  rather than inventing a parallel scheme.
- **Consumes** the committed-versus-host-local split from
  [repo-owned-project-state.md](../knowledge/repo-owned-project-state.md), and
  the checkout-owner mutation lane for the deferred write path.
- **Companion of**
  [reflective-graph-connector-program.md](../../connectors/reflective-graph-connector-program.md):
  it supplies the checkout-plane transport and read semantics that program's
  section 3.2 assumes for tenant record graphs, and unblocks the runtime wiring
  of milestone M1, whose salvage implementation predates the locality split.
  Connector-owned source graphs stay a separate authority plane, not specified
  here.
