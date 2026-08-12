---
title: "Reflective graph state transport and visibility"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
tags:
  - reflective-graph
  - locality
  - knowledge-source
  - provisional-lane
  - checkout-owner
  - visibility
brief: "How project-owned reflective graph documents reach a zero-checkout-authority corpus daemon and what visibility graph reads get: graphs ride the existing knowledge-source transport as a third admitted lane, reads take published|own|all with knowledge's defaults, the overlay unit is the whole graph rather than the vertex, structural validation splits between accept-time corpus-side and on-demand checkout-side, and v1 mutation stays file-first with later tool writes riding the checkout-owner lane."
date: 2026-08-12
---

# Reflective graph state transport and visibility

> **Status: proposed.** Everything graph-specific here is proposed and unbuilt.
> What it builds on is landed on `beta/blackbox-v2`: the knowledge source
> transport (publication candidates plus leased provisional workspaces, KT-A
> through KT-F), checkout identity, the provisional overlay model with
> `published|own|all`, content-equality promotion, the candidate-tree merge
> gate, and the checkout-owner mutation lane that routes project-scoped
> knowledge and gap writes as exact `.bbox/` byte mutations applied by a
> collector and published by a human commit. The reflective graph kernel is a
> design plus donor code on a diverged salvage branch; its port is in flight.
> Names below come from those designs, not a fresh code read: reverify against
> the tree before building.

## 0. What this decides

The kernel leaves daemon materialization unspecified, and the salvage-era
implementation read `<project>/.bbox/graphs/` off the daemon host. The
production daemon has no checkout filesystem authority, so that path is dead.

1. Graph documents travel over the existing knowledge-source transport as an
   additive third lane, not a new wire.
2. Graph reads take the same `published|own|all` parameter, defaults, and
   session-authority rules knowledge reads carry.
3. The provisional overlay unit is the whole graph, not the vertex.
4. Structural validation runs corpus-side at accept and view-build time and
   checkout-side on demand, with a stated split of what surfaces where.
5. V1 mutation is file-first; a later tool write rides the checkout-owner
   mutation lane, never a daemon file write.

## 1. Transport

### 1.1 Graphs are a third lane on the existing descriptors

`.bbox/graphs/` is exactly the class of state the knowledge source transport
carries: committed, repo-owned, reviewable, project-scoped, edited in a
checkout the daemon cannot open. It rides the same contract.

- `PublicationCandidateDescriptorV2` adds a `graphs` lane beside `knowledge`
  and `gaps`, carrying committed graph documents at one full branch ref and
  exact commit. A V1 descriptor decodes with an empty `graphs` manifest.
- `ProvisionalWorkspaceDescriptorV2` adds the same lane to the `baseline` and
  `working` classes, so overlays compute from a merge-base baseline and a
  working tree exactly as knowledge and gap overlays do.
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
It appears in no manifest, candidate, or workspace descriptor and has no
published form, reachable only by a checkout-confined reader in the owning
workspace. Sharing a scratch graph means moving its files under `.bbox/graphs/`
and committing them; there is no daemon-side promotion of scratch state.

## 2. Visibility

### 2.1 Parameter and defaults

Every graph read surface takes `provisional = published | own | all` with the
knowledge lane's semantics:

- `published`: the accepted publication generation's graph set only;
- `own`: published graphs except those shadowed or tombstoned by the session
  workspace's graph overlay, plus that overlay's upserted graphs;
- `all`: published graphs plus every live valid workspace's upserts, labeled
  with their checkout.

Default is `own` only when the server holds an authoritative session workspace
binding, otherwise `published`. `all` is always explicit. A model-supplied
project, graph id, checkout id, or workspace id scopes results but never
creates, replaces, or widens own authority. Covered surfaces:
`bbox_project_graph_list`, `bbox_project_graph_describe`,
`bbox_project_graph_validate`, exact vertex inspection, traversal, evidence
bundling. The filter applies before ranking, inspection, traversal expansion,
and bundle assembly; post-hoc decoration can leak a peer vertex into a
traversal that already crossed an edge into it.

### 2.2 The overlay unit is the whole graph

Knowledge overlays key per entry because an entry is self-contained. A vertex is
not: its validity depends on `schema.json` and on other rows in the same files.
A per-vertex overlay would let a session read an own vertex whose declared type
exists only in the published schema, or an own edge whose endpoint was deleted
in the same working tree. Therefore:

```text
GraphOverlayKey   = (PublishedScope, checkout_id, graph_id)
GraphOverlayValue = Upsert { schema, vertices, edges, content_hash }
                  | Tombstone
```

A graph resolves whole: for one graph id a view yields either the published
generation or exactly one workspace's generation, never a merge. Tombstones are
first-class, so deleting `.bbox/graphs/<id>/` hides the published graph from
that checkout's `own` view and can promote by absence.

Overlay publication is all-or-nothing per `(scope, checkout, graph)`: an
invalid graph is marked `Invalid` with diagnostics and the workspace's other
graphs, knowledge, and gaps keep serving. That is narrower than the knowledge
overlay's per-scope rule on purpose, because a half-edited `vertices.jsonl` is
a normal mid-edit state and must not blind knowledge reads.

### 2.3 Provisional graphs never masquerade as published

A provisional graph carries its own ref family and stamp:

```text
provisional_project_graph_vertex:<scope_hash>:<checkout_id>:<graph-id>:<vertex-id>
```

`scope_hash` is the full SHA-256 of `(repo_id, bbox_root_relpath)`;
`checkout_id` is the 32 lowercase hex workspace id; `graph-id` contains no `:`;
the remainder is the raw vertex id. Properties carry the unhashed published
scope, the logical published-form ref, the checkout label, the graph content
hash, and the overlay snapshot stamp. Responses carry the landed `built_from`
stamp table, published rows pointing at the accepted generation stamp and
provisional rows at the workspace overlay stamp.

Promotion is content equality, not id existence: an overlay retires when the
accepted generation's content hash for that graph id equals the overlay's, and
a tombstone retires when the id is absent from that generation. One workspace's
promotion never retires a peer's variant.

Scratch graphs appear only in `own`, only for the owning workspace's session,
only on explicit opt-in (the kernel's rule that scratch graphs do not traverse
by default), always labeled scratch, with no published-form logical ref. They
never appear in `all` or in a citable evidence bundle.

### 2.4 Degradation

An `own` request whose graph overlay is invalid or whose lease expired
hard-errors that graph rather than silently serving the published generation.
An `all` request omits that workspace's graph and reports it in structured
`degraded.overlays`, preserving published and valid peers. A project with no
accepted generation has no published graph set: a scope-local hard error for an
explicit query, an omission with diagnostics for an aggregate.

## 3. Validation placement

### 3.1 Three error classes

| Class | Examples | Where detectable |
|---|---|---|
| Lane admission | path depth, unknown filename in a graph dir, symlink, byte or row ceiling exceeded, blob digest mismatch | producer capture, transport finalize |
| Document parse | malformed `schema.json`, malformed JSONL row, non-UTF-8, reserved top-level key misuse | checkout tool, merge gate, accept, view build |
| Graph structure | undeclared vertex type, non-`meta:VertexType` type, edge type with no endpoint declaration, endpoint type mismatch, duplicate vertex id, duplicate edge key, reserved namespace misuse, missing referenced vertex, property shape violation | checkout tool, merge gate, accept, view build |

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

### 3.3 Corpus-side, at provisional view build

A workspace descriptor is validated for lane admission at finalize, fatal to
the whole descriptor by the atomicity rule. Parse and structure are evaluated
afterward, per graph, at overlay view build, and a failure marks only that
graph `Invalid`. The asymmetry is intentional: a candidate is a reviewed
commit, a workspace snapshot is a running edit, and a workspace must not lose
knowledge visibility because a graph file is mid-rewrite.

### 3.4 Checkout-side, on demand

`bbox_project_graph_validate` runs in the workspace against working files, with
no dependence on transport or on a current accepted generation. It is the
authoring gate and pre-commit check, reports the full parse and structure set
with file and line, and validates scratch graphs the corpus never sees. Under
the confined-tool model it executes harness-native for a workspace-bound
session, mirroring project knowledge and gap mutations. An empty local error
list does not promise acceptance: admission ceilings are server config and can
reject a locally valid graph, so the tool reports the ceilings it knows and
marks that check advisory.

## 4. Ref resolution

Published vertices resolve by the kernel ref family against the accepted
generation for the project's published scope; provisional vertices resolve by
the compound ref of section 2.3.

- `published`: a published-form ref resolves against the accepted generation. A
  vertex existing only in a workspace is not found, and the error names the
  active visibility mode rather than widening silently.
- `own`: a published-form ref resolves against the session view, which is the
  published generation of that graph unless the session workspace holds an
  upsert or tombstone for the graph id. A vertex found only through the
  workspace generation is labeled provisional and its canonical identity in the
  response is the compound ref, so a caller that stores the result stores an
  unambiguous handle.
- `all`: a published-form ref resolving in more than one generation is
  ambiguous. Return an ambiguity error listing candidate compound refs rather
  than choosing; `all` is a survey mode, and picking a winner there is the
  failure the compound ref family exists to prevent.
- A compound provisional ref resolves only while that workspace overlay is live
  and valid, returning a lease or validity error otherwise, and never falls
  back to the published vertex of the same logical id.

Traversal stays inside one graph generation; the whole-graph overlay unit
guarantees both endpoints of a walked edge come from the same generation, and
cross-graph edges remain deferred by the kernel. Evidence bundles record the
visibility mode and per-graph `built_from` stamp for every included vertex and
edge; a bundle holding any provisional or scratch vertex is labeled provisional
and is not citable as published evidence.

## 5. Mutation

**V1 is file-first.** Edit `.bbox/graphs/<id>/*` in a checkout, validate with
the checkout-side tool, commit. The commit is the publish gate; the checkout
owner captures a candidate and the operator advances the pointer. Between edit
and commit, provisional capture makes the change visible in `own` and, on
explicit request, `all`. The daemon never opens or writes a checkout path.

**If tool writes arrive later**, `bbox_project_graph_put_vertex` and
`bbox_project_graph_put_edge` ride the checkout-owner mutation lane in the
shape knowledge and gap writes took: the daemon validates the proposed result,
seeds from the session's own view rather than published, produces the exact
replacement bytes, and enqueues a durable pending checkout mutation the
collector applies and acks. Human commit stays the publish gate. Graph-specific
constraints: one graph id per mutation and whole-file byte replacement of the
affected `vertices.jsonl` or `edges.jsonl` (never a row append, because the
kernel requires normalized state files rather than append-only logs); refusal
when the target graph's own view is invalid, the workspace has a pending
transaction, or the result would fail structural validation; ids stay
project-owned, so the daemon validates and never mints them; and the write is
path-constrained to `.bbox/graphs/` and grant-scoped per producer.

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
- No per-vertex overlay granularity and no cross-generation graph merge.
- No full-text or vector indexing of graph vertices; the kernel defers it.
- No promotion of graph facts into knowledge, decisions, pins, or rendered
  memory; that stays explicit and operator-gated.
- No cross-machine scratch-graph visibility, and no remote-branch fetch to
  reconstruct a torn-down workspace's overlay.
- No graph-specific staleness clock or lease lifecycle.

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
accept-time structural validation plus visibility semantics that lane lacks.

**Ignoring unknown files inside a graph directory:** rejected in favor of
failing admission, so the corpus view cannot silently differ from the checkout.

## 8. Acceptance criteria

1. A committed graph reaches the corpus with no daemon filesystem access to any
   checkout, over the existing routes with `lane=graphs`.
2. A candidate failing graph admission, parse, or structure is rejected, the
   pointer does not move, and the prior accepted graph set keeps serving.
3. Every graph read surface accepts `published|own|all`, defaults to `own` only
   under an authoritative session workspace binding, and refuses to establish
   own authority from a model-supplied argument.
4. A workspace edit is visible in that session's `own` and in a peer's explicit
   `all`, labeled with checkout and stamp, and invisible in `published` until
   its commit is accepted; deleting a graph directory tombstones instead, and
   promotes when the id is absent from a newly accepted generation.
5. An invalid graph marks only that graph's overlay invalid; the workspace's
   other graphs, knowledge, and gaps keep serving.
6. A published-form ref for a provisional-only vertex resolves under `own`, is
   not found under `published` with a mode-naming error, and returns an
   ambiguity error listing compound refs under `all`.
7. Scratch graphs never appear in a candidate, in `all`, or in a citable bundle.
8. The candidate-tree merge gate fails on a structurally invalid graph before
   any ref moves, and checkout-side validation reports identical error codes and
   locations to corpus-side accept validation on one tree.

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
- **Transient preservation.** Whether a graph overlay should survive a brief
  lease gap as knowledge does, or fail closed immediately.
- **Scratch inclusion ergonomics.** Whether explicit scratch inclusion is a
  separate boolean or a fourth token, given it is orthogonal to
  published/own/all.

## 10. Relationship

- **Extends** [reflective-project-graph.md](reflective-project-graph.md) by
  filling the Implementation Boundary it deferred: how graph documents reach
  the daemon and what a read sees. It changes nothing in the fixed floor,
  storage shape, validation vocabulary, or tool surface.
- **Extends** the knowledge lane defined by
  [checkout-identity-and-provisional-knowledge.md](../knowledge/checkout-identity-and-provisional-knowledge.md)
  and implemented by
  [knowledge-source-transport-impl.md](../../daemon-runtime/knowledge-source-transport-impl.md),
  reusing checkout identity, published scope, merge-base overlays, built_from
  stamps, content-equality promotion, and `published|own|all` rather than
  inventing a parallel scheme. The one deliberate divergence is overlay
  granularity, argued in section 2.2.
- **Consumes** the committed-versus-host-local split from
  [repo-owned-project-state.md](../knowledge/repo-owned-project-state.md), and
  the checkout-owner mutation lane for the deferred write path.
- **Companion of**
  [reflective-graph-connector-program.md](../../connectors/reflective-graph-connector-program.md):
  it supplies the checkout-plane transport and visibility that program's section
  3.2 assumes for tenant record graphs, and unblocks the runtime wiring of
  milestone M1, whose salvage implementation predates both the locality split
  and the provisional-visibility model. Connector-owned source graphs stay a
  separate authority plane, not specified here.
