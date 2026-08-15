---
title: "Unified Retrieval For Reflective Graph Vertices"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - connectors
tags:
  - reflective-graph
  - connectors
  - retrieval
  - hybrid-search
  - evidence
  - agentic-corpus
brief: "Milestone 9 of the graph-native connector campaign: how reflective project-graph vertices join the word index and the optional vector lane under per-graph policy, how authority filters run before ranking and before traversal expansion, and what a graph-bearing search result carries."
date: 2026-08-14
---

# Unified Retrieval For Reflective Graph Vertices

Status: proposed. This document owns milestone M9 of
[Graph-native connector campaign](reflective-graph-connector-program.md) and
resolves the design frame for `gap-5d57d2bb` ("Reflective graph vertices do
not participate in unified retrieval", domain `agentic-corpus/retrieval`).
It does not restate the program's plane split, authority model, or connector
runtime; it assumes them.

The reflective graph v1 deliberately shipped exact-ref inspection and
traversal only
([Reflective Project Graph](../corpus/agentic-corpus/reflective-project-graph.md),
"Query Semantics": *"Search comes later. V1 requires exact refs, graph
listing, or traversal from known graph vertices."*). That was the right
first cut, and it is now the binding constraint. Today
`BlackboxServer::resolve_project_graph_vertex` is a strict
`graph.vertices.get(vertex_id)`: no prefix match, no label match, no fuzzy
resolution. `bbox_inspect_entity` and `bbox_find_paths` are the only doors,
and both need a canonical ref the caller already holds. A tenant who
authored a record graph can only reach it if they already know the vertex
id, and a connector-projected source graph is invisible to every agent that
starts from a question rather than from a ref.

## 0. Outcome

One query path over one corpus, in which graph vertices are ordinary
retrievable entities that never lose the thing that makes them different:
which graph they came from, which authority plane asserted them, and what
they are evidence for.

Concretely, after M9:

- a natural-language query reaches a record vertex, a connector source
  vertex, a knowledge entry, and a file chunk in one ranked list;
- every graph-bearing result carries graph id, authority plane, generation,
  and provisional labeling, so a caller can tell a tenant assertion from a
  connector projection without a second call;
- a result can be expanded along its evidence bindings into a bounded path,
  with `path_id`s the caller hands straight to `bbox_bundle_evidence`;
- a placed file chunk points back at the remote source vertex that placed
  it;
- authorization is applied before ranking, in both lanes, never as a
  post-fusion retain.

The outcome is explicitly NOT a graph query language, not automatic
promotion of graph facts into knowledge, and not "index everything in the
graph". The graph kernel must not assume every property is useful or safe
to embed.

## 1. What this milestone consumes

M9 is the first milestone that touches retrieval, and it is late in the
dependency graph for a reason: it consumes contracts that M1 through M3
already landed on `beta/blackbox-v2`. Naming them precisely is most of the
design, because the seams already exist and this milestone is mostly a
matter of not inventing parallel ones.

### 1.1 The graph kernel (M1)

`crates/bbox-project-graph` owns the model. The load-bearing types:

```text
GraphDescriptor { descriptor_version, scope, graph_id, authority,
                  schema_id, schema_version, projection_version,
                  source_connector, retention_policy, generation }
GraphAuthority  { Project, Connector }
GraphSource     { Committed, LocalScratch, ConnectorManaged }
GraphGeneration { key, descriptor, schema, vertices, edges, fingerprint,
                  source_root, authored_vertex_count, authored_edge_count }
ProjectGraphVertex { id, type_name, label, properties }
ProjectGraphEdge   { from, type_name, to, properties }
```

`GraphGeneration.fingerprint` is the incremental-indexing key this
milestone needs and does not have to invent. `build_generation` is the
single reflection function shared by both authority planes, which is why a
connector source graph and a project-authored graph present identically to
anything downstream, retrieval included.

`authored_vertex_count` versus the projected vertex map is the distinction
that keeps schema-as-data meta vertices out of retrieval. The fixed floor
(`meta:VertexType`, `meta:EdgeType`, `meta:INSTANCE_OF`, `meta:FROM_TYPE`,
`meta:TO_TYPE`) and the per-type `meta:VertexType` / `meta:EdgeType`
vertices `build_generation` projects in are structure, not meaning. M9
never indexes them.

Note the three vocabularies the kernel keeps deliberately apart, because
M9 has to pick one for its result shape:

- storage plane: `GraphSource::{Committed, LocalScratch, ConnectorManaged}`;
- descriptor authority: `GraphAuthority::{Project, Connector}`;
- read-surface label: `published | provisional | connector`, minted by
  `source_label()` in `src/project_graph_read.rs` and already carried on
  `GraphSummary.source` from `bbox_project_graph_list`.

`GraphSource::authority_label()` carries a similar-looking triple and its
doc comment says explicitly not to wire it into tool output. M9 uses the
read-surface label and nothing else (section 6.1).

### 1.2 The retrieval annotations (M2)

The per-property annotations this milestone was waiting for are already in
the schema model, structural but inert. From
`crates/bbox-project-graph/src/model.rs`:

```rust
pub struct GraphIndexPolicy {
    /// Whether any property in this graph may participate in embeddings.
    /// A property that opts into embedding while this is false is a schema
    /// error, not a silent downgrade.
    pub embeddings_enabled: bool,
}

pub enum PropertyIndexMode { None, Word, Text }

pub struct PropertyAnnotations { pub index: PropertyIndexMode, pub embed: bool }
```

`GraphSchema.index_policy` is the per-graph gate those annotations sit
under, and its doc comment already says the quiet part: *"Structural only in
M2: nothing indexes or embeds from it yet."* M9 is what makes it live.

Four properties of the landed annotation surface constrain this design:

1. **The discriminator is narrow on purpose.** An annotated property term is
   an object whose keys are a subset of `{type, index, embed}`, carrying
   `type` and at least one of `index` / `embed`. A bare `{"type": "string"}`
   keeps its existing meaning. No shipped schema changes meaning when M9
   lands.
2. **Unannotated is conservative.** `property_annotations` returns
   `{index: none, embed: false}` for every unannotated term. Silence means
   "do not index", not "index by default".
3. **The per-graph gate dominates the per-property annotation.** With
   `embeddings_enabled: false` no property embeds however it is annotated,
   and a property asking for it is `schema.embedding_not_enabled` rather
   than a silent downgrade. M9 preserves that direction: policy refuses,
   annotation requests.
4. **Annotating never changes validation.** `property_term_body` is what
   value validation runs against, so the annotation is orthogonal to the
   type system. M9 must keep that orthogonality; a retrieval concern that
   starts constraining legal values has leaked.

This is the shape operator decision `b1a11d7cf59f2545` ratified: *"by
default only vertex labels are word-indexed; graph schemas declare
per-property annotations for text indexing and embedding participation, and
embeddings are strictly per-kind opt-in under per-graph policy. No property
is embedded or text-indexed implicitly."*

### 1.3 The read plane and its three authority planes (M2)

`crates/bbox-indexing/src/project_graph_view.rs` is the read seam, and it is
already plane-aware:

```text
ProjectGraphViewCatalog {
    published:   ProjectId                -> PublishedProjectGraphView
    provisional: (ProjectId, WorkspaceId) -> ProvisionalProjectGraphOverlay
    connector:   (ProjectId, GraphId)     -> ConnectorProjectGraphView
}
```

with `list_published` / `list_own` / `load_published` / `load_own` /
`list_connector` / `load_connector` / `visible_connector`, and the collision
rule already settled: a project-authored graph shadows a connector graph of
the same id, refused at install time where it can be
(`error.graph_authority_conflict`) and resolved at read time where it
cannot.

`ProjectGraphViewEntry` carries `ProjectGraphGenerationIdentity
{ accepted_generation, accepted_commit, source_generation, workspace_id,
content_hash }`. That is the identity an index lane keys on and a result
carries; M9 does not mint a second one.

Two operational facts from this seam bind the indexing design directly:

- **There is no lazy rebuild on read.** Whoever commits a generation must
  install it into the catalog (`refresh_provisional_graph_views` and the
  publication paths in `src/server/knowledge_source.rs`). Indexing must
  therefore hang off the same installation, not off a reader.
- **Read context is resolved before the catalog lock is taken.**
  `src/project_graph_read.rs` documents that re-deriving it under the lock
  re-enters the same `RwLock` through `validate_project_selection` and
  deadlocks the moment a writer is queued between the two acquisitions. Any
  retrieval path that consults the catalog inherits that ordering.

### 1.4 The evidence lane (M3)

`crates/bbox-project-graph/src/evidence.rs` owns bindings. The endpoint
shapes:

```rust
pub enum EvidenceEndpointV1 {
    GraphVertex { graph_id, vertex_id },
    ProjectFile { rel_path_hash, chunk_hash, occurrence_idx },
    Knowledge   { id },
    Ref         { entity_ref },
}
```

and the freshness algebra:

```rust
pub enum EvidenceEndpointStatus { Current, Stale, Missing, Unauthorized, Unresolved }

impl EvidenceEndpointStatus {
    /// Only `Unauthorized` refuses. Stale and missing endpoints stay
    /// traversable and labeled [...] an unauthorized one is a scope
    /// violation and is never an answer.
    pub fn traversable(self) -> bool { !matches!(self, Self::Unauthorized) }
}
```

This is the precedent M9 extends rather than replaces. The rule "stale is an
answer, unauthorized is not" already exists, and the observation/scoring
split (`EvidenceEndpointObservation` observes, `resolve_endpoint_status`
scores, aggregation precedence `Unauthorized > Stale > Missing > Unresolved
> Current`) is already unit-testable without a daemon.

Two landed details do a large amount of work in section 5:

- `EvidenceBindingSet` already carries `forward` and `reverse` maps keyed by
  `EntityRef::render()`. The reverse lookup section 5.4 needs is not a new
  index; it is a call to `EvidenceBindingSet::reverse`.
- `evidence.graph_endpoint_required` means at least one endpoint of every
  binding is a graph vertex. A placed file chunk therefore always has a
  graph vertex on the other side of any binding that names it, which is why
  the back-pointer is well defined rather than best-effort.

### 1.5 The existing hybrid pipeline

`crates/bbox-mcp-tools/src/mcp_tools/hybrid_search.rs`, entered only through
`hybrid_search_typed_with_active_selectors_and_searcher` and its string
wrapper. The unpinned convenience wrappers were deliberately deleted: the
caller supplies both the active-selector map and the searcher from
`SharedState::code_read_view`, so a commit landing mid-call cannot filter
vector hits against a different index generation than produced them. M9
inherits that discipline and extends the pin to the graph view (open
question Q5).

The constants and the merge:

```text
RRF_K = 60.0    VECTOR_WEIGHT = 0.6    DEFAULT_FETCH = 50    MAX_LIMIT = 50
fuse_rrf(lists, k, limit):  contribution = list.weight / (k + rank)
```

The ranked lists handed to `fuse_rrf` are **more numerous than the docs
say**. `docs/graph-retrieval-internals.md` describes three lanes; the code
builds:

| List | Weight | Source |
|---|---|---|
| `bm25` | `1 - vector_weight` | Tantivy, with `exclude_knowledge = true` |
| `knowledge` | `1 - vector_weight` | The in-memory authorized knowledge view |
| `bm25_file` | `1 - vector_weight` | `aggregate_bm25_by_file` over the untruncated BM25 fetch; empty when fewer than two files |
| `vector:<partition>` | `vector_weight` | One list **per vector partition** |

Then, in order: `fuse_rrf` -> model rerank (cross-encoder, default on) ->
per-hit heuristic rerank (type x temporal, capped at
`DEFAULT_COMBINED_CAP = 1.75`) -> project filter -> `doc_type` backstop
retain -> per-file collapse -> modal diversification -> truncate. That
`docs/graph-retrieval-internals.md` omits both the knowledge lane and the
entire rerank stage is a documentation debt this milestone should clear
(section 8).

The **knowledge lane is the precedent that matters most for M9**, because
knowledge is the existing entity family whose visibility could not be left
to the index alone. Its treatment has three parts, and they are worth
naming because M9 chooses a different combination:

1. the BM25 lane excludes knowledge outright (`exclude_knowledge = true`)
   and, when it does not, adds `Occur::MustNot` on
   `knowledge_visibility == "provisional"`;
2. a pre-authorized in-memory lane is injected in its place;
3. the vector lane, which cannot carry a Tantivy predicate, is filtered
   per hit by `retain_authorized_knowledge_vectors` **before** fusion.

Part 3 is the piece M9 reuses verbatim in spirit. Parts 1 and 2 are the
piece M9 declines, for the reasons in section 4.2.

The schema already carries `knowledge_visibility`, `knowledge_scope_hash`,
and `knowledge_checkout_id`, which is proof that per-checkout visibility is
expressible as index terms. Graph vertices are strictly easier: their
visibility key is already in the ref family.

## 2. Invariants this milestone adds

These sit under the campaign's own invariant list and do not amend it.

1. **No implicit participation.** A vertex property enters the word index or
   an embedding only because a schema author annotated it and a per-graph
   policy permits it. Vertex labels are the single default-on field, and a
   per-graph kill switch can turn even that off.
2. **Meta vertices are never retrievable.** The fixed floor and the
   schema-as-data projection describe the graph; they are not facts about
   the world. Retrieval indexes authored vertices only.
3. **Authority filtering precedes ranking, in every lane.** The word lane
   filters with an index predicate; the vector lane filters per hit before
   fusion. Neither filters after fusion. An unreadable document must not
   consume a rank position.
4. **Authority filtering precedes traversal expansion.** Path expansion
   selects readable graphs before it enumerates neighbors, so an
   unauthorized graph is never walked and never appears as a truncated
   path.
5. **Results retain plane identity.** Graph id, read-surface source label,
   generation, and vertex type travel with every graph-bearing result and
   every path hop. A result that cannot carry them is not returned.
6. **Bounded expansion.** Evidence expansion returns paths and refs under an
   explicit budget. No surface dumps a whole source graph into model
   context.
7. **Retrieval is a projection.** Everything M9 writes (index documents,
   vectors) is rebuildable from accepted graph generations and accepted
   binding sets, with no remote contact. This follows the campaign's
   reprojection rule and the corpus's existing rebuildable-vs-durable split.
8. **Read paths still do not invent facts.** The reflective graph's semantic
   rule survives contact with search: *read paths may traverse project graph
   facts; read paths must not invent project graph facts.* A retrieval hit is
   not an assertion.

## 3. Meaning-bearing vertex kinds and per-graph policy

### 3.1 The split

The corpus already runs a meaning-bearing versus mechanical split for
embeddings: claim-shaped and decision-shaped content embeds, mechanical
content is word-indexed. Graphs need the same split, but Blackbox cannot
make it, because the vocabulary is project-owned by construction. A
`repo:DesignClaim` is meaning-bearing and a `xero:FileAssociation` is a join
row, and nothing structural distinguishes them: both are a typed id with a
label and a property bag.

The split is therefore **declared, never inferred**. Blackbox contributes
the vocabulary and the defaults; the schema author contributes the judgment.
The concrete guidance for schema authors, which belongs in the schema
authoring docs rather than in code:

| Vertex shape | Typical annotation | Rationale |
|---|---|---|
| Subject entities (a contact, a project, a filing, a channel) | label default-on; one or two `index: word` identity properties | The label is the human handle; identity properties make exact lookups work |
| Claim-shaped entities (a design claim, a requirement, a finding) | `index: text` on the claim body; `embed: true` under an enabling policy | Paraphrase recall is the whole point |
| Documents and files (a remote file vertex) | label plus `index: word` on filename and content type | The bytes are indexed separately once placed; the vertex is a handle |
| Association and join vertices | nothing | They carry no meaning a query can want; they are reached by traversal |
| Enumeration and lookup vertices (tracking options, categories) | label only | Small, mechanical, exact-match shaped |
| Anything credential-shaped, personal, or free-form remote payload | nothing, and the graph policy should refuse | The reason the default is conservative |

### 3.2 What the per-graph policy must gain

`GraphIndexPolicy` currently carries one field. M9 needs three, and the
extension stays additive under `#[serde(default)]` so no shipped schema
changes meaning:

```rust
pub struct GraphIndexPolicy {
    /// Existing. Per-graph gate over every property `embed` annotation.
    pub embeddings_enabled: bool,

    /// New. Whether this graph participates in text retrieval at all.
    /// Default true: the conservative default lives in the per-property
    /// annotations, not here, and a graph whose properties are all
    /// unannotated already contributes only labels.
    pub text_retrieval_enabled: bool,

    /// New. Vertex types excluded from retrieval regardless of annotation.
    /// The operator escape hatch for a shipped connector schema the tenant
    /// does not own.
    pub retrieval_excluded_types: BTreeSet<String>,
}
```

Three notes on the shape.

`text_retrieval_enabled` defaults **true**, which reads backwards until you
notice where the conservatism already lives. With no property annotations a
graph contributes labels and nothing else, which is exactly what decision
`b1a11d7cf59f2545` asked for. A default-false gate would mean every schema
author must opt in twice to get the documented default behavior, and the
second opt-in would become boilerplate that stops meaning anything. The
field exists to be turned OFF.

`retrieval_excluded_types` exists because a connector source schema is a
versioned corpus artifact shipped through the artifact catalog (decision
`7650b743fb23c265`), not something the tenant edits. Without a policy-side
exclusion, a tenant who wants one vertex type out of retrieval has to fork
a vendor schema. Exclusion is coarse and per-type on purpose: per-property
tenant overrides would fork the annotation authority in two, and then no
one could answer "why is this indexed?" from one document.

Neither new field can widen participation beyond what annotations request.
Policy subtracts; annotations request. That direction is already how
`embeddings_enabled` behaves and it is worth keeping uniform, because it
makes the audit question one-directional: to prove a property is not
indexed, read the annotation; to prove one IS indexed, read both.

New validation codes follow the existing `schema.*` family:
`schema.invalid_retrieval_policy` for a malformed block,
`schema.unknown_excluded_type` for an exclusion naming a type the schema
does not declare. The second is deliberately an error rather than a no-op:
a silently ignored exclusion is a policy the operator believes is in force
and is not.

### 3.3 Authority plane defaults

The three read-surface planes get different defaults because they carry
different trust:

- **published** (project-authored, accepted): participates by default under
  the rules above. This is the tenant's own reviewed state.
- **provisional** (project-authored, a checkout's own overlay):
  participates, but only for the checkout that authored it, under the
  existing `published | own | all` semantics. See section 4.5.
- **connector** (connector-managed source projection): participates by
  default for labels and annotated properties, but a connector graph is a
  projection of a third-party system and its policy should be reviewed at
  enablement rather than assumed. The operator-facing status surface reports
  retrieval participation per graph (section 6.5) so this is visible without
  reading the schema artifact.

Local scratch graphs (`GraphSource::LocalScratch`,
`RetentionPolicy::LocalScratch`) do **not** participate. This follows the
kernel's settled choice that scratch graphs do not participate in traversal
by default; making them searchable by default would be a strictly larger
exposure than the traversal case, since a caller does not have to name them
to reach them.

## 4. The indexing seam

### 4.1 The retrieval unit is one document per vertex

An indexed graph vertex is one Tantivy document whose `entity_id` is the
canonical ref:

```text
project_graph_vertex:<project_id>:<graph_id>:<vertex_id>
provisional_project_graph_vertex:<scope_hash>:<checkout_id>:<graph_id>:<vertex_id>
```

Both ref families already exist in
`crates/bbox-corpus-core/src/entity_ref.rs`. M9 introduces no ref family and
no domain-specific variant, which keeps campaign invariant 1 intact.
`entity_id` is the only join key across Tantivy, the vector store, and the
edge index, so a graph vertex needs nothing else to participate.

The alternative, one document per `(vertex, text property)`, gives sharper
BM25 term statistics and would let a hit name the property that matched. It
is rejected for v1: there is no ref for a property, so the surface would
return hits it cannot address; per-file collapse and modal diversification
would need a per-vertex analogue immediately; and the corpus's existing
"one document per content block" rule is per addressable unit, and the
addressable unit here is the vertex.

Field mapping, using fields the schema already has rather than minting
graph-specific text fields:

| Source | Field | Why |
|---|---|---|
| `vertex.label` | `content` | Default-on, the human handle, prose-tokenized |
| `vertex.type_name` | new `graph_vertex_type` (`STRING \| STORED`) | Filterable and reportable; the namespaced form (`repo:Module`) is a term, not prose |
| properties with `index: word` | `path_tokens` (code tokenizer) | Identity-shaped values; the tokenizer already splits identifiers and paths, which is what an id-like property wants |
| properties with `index: text` | `content` | Full BM25 over the body, same field as the label |
| `graph_id` | new `graph_id` (`STRING \| STORED`) | Section 5.1 needs it filterable |
| read-surface source label | new `graph_source` (`STRING \| STORED`) | `published` / `provisional` / `connector` |
| `source_connector` | new `graph_source_connector` (`STRING \| STORED`) | Connector plane only |
| generation identity | new `graph_generation` (`STRING \| STORED`) | Result field and staleness signal |
| `project_id` | existing `project_id` | Reuses the existing project filter term |
| `properties` (whole bag) | not indexed, not stored | Unannotated content never reaches a term dictionary or a response |

`doc_type` is `project_graph_vertex` for both planes; the plane is carried
by `graph_source`, not by `doc_type`. That keeps the existing `doc_type`
parameter meaningful (one value scopes to graphs) and puts the plane where a
filter can combine it with visibility.

Plumbing that must move with the fields, all of it already single-sited:

- `properties_from_doc` (`crates/bbox-corpus-index/src/index/mod.rs`) is the
  projection surface the whole graph layer reads; the new fields are added
  there or they are invisible to `bbox_inspect_entity`'s index path.
- `hybrid_title` picks a title in a fixed order; a graph document's title is
  its label.
- `candidate_document` in the rerank stage builds the text sent to the
  cross-encoder; a graph vertex contributes label plus `index: text`
  properties.
- `file_dedup_key` and `aggregate_bm25_by_file` must **exclude** graph
  documents explicitly rather than relying on absent path fields. A graph
  vertex has no file, and a dedup key that falls back to `entity_id` would
  make every vertex its own file and pollute the file-aggregate lane with
  singleton groups.

The excerpt for a graph result is composed from the label plus the
`index: text` properties, truncated. It is never the raw property JSON: a
property bag serialized into an excerpt is how unannotated values leak into
a response after being correctly kept out of the index.

### 4.2 Graph vertices do not add an RRF lane

This is the central query-path decision and it is forced by the indexing
choice above.

Graph vertex documents live in the same Tantivy index as every other
content block. They therefore appear in the **existing** `bm25` lane
automatically, and once embedded in the existing per-partition vector lanes.
M9 adds **no new ranked list** to the fusion.

The tempting alternative is the knowledge treatment: exclude graph vertices
from the BM25 lane and inject a pre-authorized graph lane beside it. That
is a real precedent in this exact pipeline, so declining it needs a reason.

Knowledge took that shape because its authorization lives in an in-memory
session view that no index predicate could reproduce, and because the
knowledge store is small enough to rank in memory. Neither holds for
graphs. A graph vertex's visibility key is already materialized in its ref
and can be a Tantivy term (`project_id`, `graph_id`, `graph_source`, plus
the scope and checkout segments of the provisional form), exactly as
`knowledge_scope_hash` and `knowledge_checkout_id` already are for the
knowledge documents that DO get indexed. And a connector source graph can
carry tens of thousands of vertices, which is the wrong size to rank
outside Tantivy.

The cost of a new list is also concrete rather than aesthetic. `fuse_rrf`
sums `weight / (k + rank)` across lists; it does not normalize by list
count. Adding a list at BM25 weight raises the total contribution available
to BM25-shaped evidence for **every** query, including queries that touch no
graph at all. `RRF_K` and `VECTOR_WEIGHT` were swept empirically and the
crate note is explicit that they change with the metrics harness
(`bbox-corpus-core` `search/metrics.rs`) and not by feel. A corpus-wide
ranking shift shipped as a side effect of a connector milestone is the
wrong trade.

If graph results later need their own lane, it is a sweep scored with MRR
and recall@k, and it is its own change.

The `bm25_file` lane is unaffected, given the explicit exclusion in section
4.1.

### 4.3 The write path and generation churn

Graph index documents are written by the same `IndexWriterActor` that owns
every other index write. There is no second writer and nothing in the graph
lane touches Tantivy directly. The natural shape is one new `IndexWriteOp`
variant per plane operation, alongside the existing
`ReplaceKnowledgeScope` / `StageConnectorGeneration` family, so admission,
retry (`IndexWriterRetryableError`), and the post-commit searcher hook are
inherited rather than reimplemented.

The trigger is activation, not a poll. The same installation that puts a
generation into `ProjectGraphViewCatalog` (`install_published`,
`install_provisional`, `install_connector`) enqueues the corresponding index
work. Section 1.3 makes this mandatory rather than merely tidy: the catalog
has no lazy rebuild on read, so an index lane that waited for a reader would
never be built. A rejected validation produces no churn:
`ProjectGraphViewEntry::invalid` carries no `GraphGeneration`, so there is
nothing to index.

**Replacement is per lane, whole.** A generation flip replaces the entire
`(project_id, graph_id, plane)` document set before re-emitting, rather than
upserting the delta, using a `delete_term` on a composite lane key exactly
as knowledge replacement does today. This mirrors the typed-history
discipline the indexing crate already documents for commit publication: a
complete replacement source can REMOVE items, and entity-only upsert strands
them. A connector reprojection that drops a vertex must drop its document; a
per-vertex upsert would leave a searchable ghost pointing at a ref that no
longer resolves.

Incremental work is keyed on `GraphGeneration.fingerprint` plus the
`ProjectGraphGenerationIdentity`. An accepted generation whose fingerprint
matches the indexed one is a no-op, which makes an idempotent connector
refresh (the campaign's exact-replay case, already idempotent in
`SourceProjectionStore::accept`) free rather than a full lane rewrite.

A `ProjectGraphOverlayValue::Tombstone` removes that graph's provisional
lane; `remove_provisional` removes the whole workspace's provisional lanes.
Neither touches the published lane.

The index schema gains the next tag in the established series
(`INDEX_SCHEMA_VERSION` is currently
`agentic-corpus-g12-conversation-projection`), so the one-time rebuild runs
through the existing marker-mismatch drop-and-rebuild path rather than a
bespoke migration.

`bbox_reindex(full=true)` rebuilds graph documents from accepted
generations. This is a corpus-side operation with no producer contact, which
is exactly the property the campaign's reprojection rule was designed to
preserve: the corpus retains accepted observations and can reproject
without asking any satellite to re-observe.

### 4.4 The vector lane

Embedding participation requires all three of: `embeddings_enabled` true on
the graph policy, `embed: true` on the property annotation, and the vertex
type not excluded. Failing any of them is non-participation, and a property
that requests `embed` under a policy that forbids it is
`schema.embedding_not_enabled`, which is already the landed behavior.

The embedded text is the composed embed-eligible projection: the label plus
the `embed: true` property values, in schema order, joined. It is never the
raw vertex JSON. Two reasons: the JSON carries unannotated values that were
deliberately excluded, and key names would dominate the embedding for small
vertices. The composition needs its own version constant in the style of
`EMBED_TEXT_VERSION`, so changing the composition invalidates the dedup
probe and forces a re-embed rather than leaving stale vectors keyed to old
text.

Route selection: one new `Bucket::Graph` variant added to the closed
`Bucket` enum, keyed like every other route by provider alias, document
model, dimension, and dtype through `Route::vector_route_id`. Per-graph
routes are deferred (open question Q3). A deployment that configures no
graph route gets word indexing only, and the lane degrades per route exactly
as it does today: an unmapped partition already falls through to
`degraded.skipped_partitions` with "no configured bucket maps to this
partition".

`bbox_reembed(route=...)` and `bbox_embed_status()` need no new shapes.

### 4.5 Provisional lanes

Provisional graph vertices index under the
`provisional_project_graph_vertex` ref family with the
`(scope_hash, checkout_id)` key the grammar already carries, and with
`graph_source = "provisional"`. This is what makes the visibility filter a
filter rather than a scan: `published` requires
`graph_source = "published"`, `own` admits the caller's own
`(scope_hash, checkout_id)` in addition, and `all` admits every valid
provisional variant. `HybridSearchParams.provisional` already carries this
parameter, parsed by `ProvisionalMode::parse`, whose default flips to `Own`
when the session has checkout authority.

The overlay semantics carry over unchanged from `list_own`: a provisional
upsert shadows the published document for that graph id, and a provisional
tombstone hides it. In index terms that is a filter-time preference, not a
write: the published document stays indexed (other checkouts still see it)
and the query-time union prefers the caller's own overlay. Resolving
shadowing at write time would make one checkout's in-flight edit mutate
what every other checkout retrieves, which is the exact failure the
provisional lane exists to prevent.

Two existing accommodations must be preserved so the provisional form stays
an implementation detail for callers:

- `resolve_published_form_vertex` already accepts the logical
  `project_graph_vertex:` form and materializes the provisional compound ref
  when the hit came from an overlay. A search result should present the
  same courtesy: the logical ref is what a caller pastes into the next tool.
- `find_paths`'s `TargetTypeFilter { admit_provisional_graph_vertex }`
  already widens `to_type = "project_graph_vertex"` to match the overlay
  form under `own` / `all`, one-directionally (gap-e41499a9). Search needs
  the same widening in `scope_lists_to_doc_type`, which today special-cases
  exactly one pair (`project_file` also keeping `project_file_v2:`). Graph
  is the second such pair and should be written as one, not as a growing
  chain of prefix special cases.

## 5. The query path

### 5.1 Authority filtering before ranking, per lane

The two lanes need two mechanisms, and both run before `fuse_rrf`.

**Word lane.** The filter is a Tantivy `Occur::Must` conjunct on the
authority fields from section 4.1, composed into the same `BooleanQuery`
that already carries the `doc_type` term and the active-code-selector
clause. Graph documents that fail it never enter the ranked list.

**Vector lane.** Vector hits carry only `entity_id` and a distance, so no
index predicate reaches them. They are filtered per hit by a
`retain_authorized_graph_vectors` pass sitting beside the two that already
exist (`retain_authorized_knowledge_vectors`,
`retain_active_code_vectors`), in the same pre-fusion block. This is not a
compromise; it is the established shape for exactly this problem, and it
runs before fusion, which is the property that matters.

The reason "before ranking" is stated as an invariant rather than an
implementation preference: filtering after fusion is not merely slower, it
is **wrong for ranking**. A caller asking for ten results, half of whose top
candidates are unreadable, gets five. Worse, the unreadable documents
consumed rank positions in each lane, so the RRF scores of the surviving
results were computed against a candidate set the caller could not see.
Post-filtering turns an authorization boundary into a silent relevance
perturbation, and the perturbation varies with how much unreadable content
happens to match.

`retain_active_code_vectors` remains what it is: a **consistency pin**
against the caller's pinned selector map, answering "does this vector belong
to the pinned index generation?". It is not an authorization gate. Both
survive; they answer different questions.

The filter's inputs in v1:

| Input | Source | Effect |
|---|---|---|
| Project scope | `HybridSearchParams.resolved_project_id`, installed daemon-side by `resolve_hybrid_project_filter` | Graph documents are project-scoped and join the filtered set |
| Provisional visibility | `HybridSearchParams.provisional` via `ProvisionalMode::parse` | Section 4.5 |
| Read-surface source | new `graph_source` parameter (section 6.1) | `published` / `provisional` / `connector`, defaulting to all three |
| Named graphs | new `graph_ids` parameter | Scoping within a project |
| Per-graph retrieval policy | `text_retrieval_enabled`, `retrieval_excluded_types` | Enforced at index time AND re-checked at query time, so a policy change takes effect before the lane is rewritten |
| Local scratch exclusion | `GraphSource::LocalScratch` | Never indexed; the query-side check is belt and braces |

Two wrinkles the implementation must handle rather than discover:

**`keep_under_project_filter` needs graph arms.** It currently matches only
`project_file`, `project_file_v2`, and `thread`. Adding
`project_graph_vertex` is mechanical (segment 1 is the project id). Adding
`provisional_project_graph_vertex` is not: that ref carries `scope_hash` and
`checkout_id`, no project id. Resolving it to a project requires the view
catalog, which section 1.3 says must be consulted with the read context
already resolved. The clean answer is to stamp `project_id` into the
provisional graph document at index time (the installer knows it) and filter
on the field rather than parsing the ref. See open question Q6.

**The `project` parameter changes meaning slightly** and the tool doc must
say so. Today it keeps `project_file` and project-scoped `thread` entries
and passes commits, knowledge, and transcripts through unfiltered. Graph
vertices are project-scoped and join the filtered set.

Tenant-level and identity-derived authorization is **not** in scope here. It
is M10, and it binds to the estate's deployed identity plane. M9's
contribution is that the filter is composed in the right place and takes a
policy input, so M10 adds a term to an existing conjunct rather than
retrofitting a filter into a pipeline that ranks first.

### 5.2 Traversal expansion

`bbox_find_paths` expands neighbors. With graph vertices in the graph, an
expansion step can cross from a knowledge entry into a record vertex, from
a record vertex into a source vertex, and from a source vertex into a placed
file chunk. Each of those hops crosses an authority boundary.

The rule, extending the landed `evidence_step_is_traversable` precedent one
layer out: **graph selection precedes neighbor enumeration**. Before
expanding out of a vertex, the traversal resolves which graphs the caller
may read (the same inputs as section 5.1) and enumerates only within that
set. An unreadable graph is not walked, its vertices never enter the
frontier, and no truncated path is emitted implying a path exists.

The asymmetry with the evidence-status algebra is deliberate and worth
stating, because at first read the two look inconsistent. An evidence
binding to an unauthorized endpoint is **retained for diagnosis** in
inspection and **refused** in traversal: the tenant asserted that binding
and deserves to know it exists and cannot be followed. An unreadable GRAPH,
by contrast, is not something the caller asserted; reporting "there are
three more hops behind a graph you cannot read" is itself a disclosure. So:
unauthorized endpoints of the caller's own bindings are labeled; unreadable
graphs are absent.

Budgets change character here and the change is easy to miss. Today
`graph_neighborhood` walks a project-authored graph that a human wrote by
hand. A connector source graph can carry tens of thousands of vertices and
association vertices with very high fan-out, which makes an unbounded
expansion catastrophic in a way the current corpus never exercises. The
existing `max_depth` plus a new per-hop fan-out cap are the bound, and a
truncated expansion says so explicitly in the response rather than silently
returning a prefix.

### 5.3 Evidence-path expansion alongside results

Search answers "which entities are relevant". The campaign's actual question
is "which entities are relevant, and what are they evidence for". Evidence
expansion closes that gap without a second round trip.

When requested, each graph-bearing result carries up to `k` bounded evidence
paths rooted at it, drawn from the accepted binding set for its project
(`ProjectGraphViewCatalog::evidence_published` and the `own` / `all`
variants). Each path carries the same `path_id` machinery
`bbox_find_paths` produces, so the caller hands it directly to
`bbox_bundle_evidence` without restating anything. The corpus already has a
hard rule about this: do not restate paths from memory, pass `path_id`s,
because the server holds the validated graph.

Every hop in an expanded path carries its `EvidenceEndpointStatus` and the
aggregate `evidence.freshness`. A stale chain is still the answer to "what
did we assert", and hiding staleness behind a clean-looking path is how a
connector reprojection silently invalidates a citation.

Note that `evidence_all` deliberately returns **separate** binding sets
rather than a merged one, because two checkouts can contradict on the same
binding id. Expansion under `all` therefore returns paths grouped by
asserting checkout, never a union that silently picks a winner.

Expansion is off by default. It is real context cost per result, most
queries do not need it, and the response's `next_steps` breadcrumbs are the
right place to pull an agent toward it when the top seeds are graph
vertices.

### 5.4 Placed chunks pointing back at source vertices

A connector action with `PlacementPolicy::ProjectOwned` publishes remote
bytes into the corpus as ordinary project-owned source content. The existing
pipeline chunks, indexes, and embeds it with no graph awareness at all, and
that is correct: the campaign's placement rule is that placement means the
bytes enter the corpus like collected code bytes do.

The consequence is a broken citation. A search hit on a placed invoice PDF
is a `project_file` chunk with no indication that it came from a remote
system, which remote object it is attached to, or when it was observed.

The fix needs no new index. `EvidenceBindingSet` already carries a `reverse`
map keyed by `EntityRef::render()`, and the M3 `ProjectFile` endpoint is
keyed by `(rel_path_hash, chunk_hash, occurrence_idx)`, which is exactly the
tuple a `project_file` search hit's `entity_id` already encodes. Result
shaping for a `project_file` hit parses its ref and calls
`EvidenceBindingSet::reverse`. Because `evidence.graph_endpoint_required`
guarantees at least one endpoint of every binding is a graph vertex, a hit
on the file side always resolves to a graph vertex on the other side.

The result gains a bounded list of source back-pointers, each carrying the
source vertex ref, its graph and read-surface source label, the binding's
assertion authority and asserted time, and the endpoint status. Bounded
because one chunk can be evidence for many records, and an unbounded list
would make a single file hit unpredictably large.

The binding sets are already in memory in the view catalog. The only cost is
the lookup, and the only new state is none.

## 6. Tool-surface deltas

M9 adds **no new tool**. Every delta is additive on an existing surface,
which is the right shape for a milestone whose thesis is "one query path".

### 6.1 `bbox_hybrid_search`

New optional parameters:

| Parameter | Meaning |
|---|---|
| `graph_source` | `published` \| `provisional` \| `connector`, repeatable; unset means all. Filter, applied before ranking |
| `graph_ids` | Restrict to named graphs within the resolved project |
| `expand_evidence` | Bounded evidence paths per graph-bearing result; off by default |

Existing parameters that gain graph meaning without changing shape:
`doc_type` accepts `project_graph_vertex` (and widens to the provisional
form under `own` / `all`, section 4.5); `project` now filters graph
documents; `provisional` now governs provisional graph vertices.

`HybridResult` gains, all `skip_serializing_if` absent so non-graph results
are unchanged on the wire:

```text
graph_id                Option<String>
graph_source            Option<"published" | "provisional" | "connector">
graph_source_connector  Option<String>   // connector plane only
graph_vertex_type       Option<String>   // the schema type name
graph_generation        Option<String>   // ProjectGraphGenerationIdentity content_hash
graph_logical_ref       Option<String>   // the project_graph_vertex form, for provisional hits
evidence_paths          Vec<...>         // only when expand_evidence
source_bindings         Vec<...>         // project_file hits, section 5.4
```

`graph_source` reuses the vocabulary `source_label()` already mints and
`GraphSummary.source` already returns, rather than introducing a fourth
naming of the same three planes. `graph_logical_ref` is what makes a
provisional hit pasteable: the compound ref is a correct identity and a
poor handle, and `resolve_published_form_vertex` already accepts the logical
form.

### 6.2 `bbox_discover_seed_entities`

Inherits everything. It reuses `hybrid_search_typed` verbatim and differs
only in post-processing, and the crate note is explicit that ranking changes
land in one place. `notable_edges` derives its priority order from the
provider's `recommended_next_hops`, so making evidence edges and
schema-declared graph edges outrank structural ones for a graph seed is a
change to `ProjectGraphVertexProvider::recommended_next_hops` (currently a
bare edge-kind count) and not a ranking fork.

### 6.3 `bbox_inspect_entity`

No argument change. Graph vertices are already inspectable by exact ref,
which is the v1 capability. Two additions:

- properties are presented through the annotation lens, so `property_mode`
  summary shows annotated properties first and does not lead with an
  unannotated blob;
- `recommended_next_hops` for a graph vertex orders evidence edges and
  schema edges ahead of structural ones, matching the semantic-first
  ordering that `project_file.rs` already documents as load-bearing.

### 6.4 `bbox_find_paths` and `bbox_bundle_evidence`

`find_paths` gains the graph-selection gate and the per-hop fan-out cap
(section 5.2), plus per-hop source labeling. `bundle_evidence` already
accepts graph vertex refs from M3; M9's change is that bundled graph
vertices render through the annotation lens and carry plane identity, so a
bundle a caller re-reads later still says which authority asserted what.

### 6.5 Operator surfaces

`bbox_project_graph_describe` reports retrieval participation per graph:
policy flags, excluded types, indexed vertex count, embedded vertex count,
and the indexed generation versus the accepted generation. This is the
surface that answers "why is my graph not showing up in search" without
reading a schema artifact, and it is the review point section 3.3 assumes
for connector graphs.

`bbox_describe_schema` reports `project_graph_vertex` and
`provisional_project_graph_vertex` populations alongside the existing entity
types, since it already returns live population counts and currently has no
graph awareness at all.

One consistency note while these surfaces are being touched: the
`bbox_project_graph_*` family names its visibility parameter `visibility`
while `bbox_inspect_entity`, `bbox_find_paths`, and `bbox_bundle_evidence`
name the identical policy `provisional`. Same values, same parser. M9 should
not add a third spelling, and aligning the existing two is a cheap
correction to make while the family is in hand.

## 7. Phasing

### 7.1 M9a: word-index participation and the authority filter

The shippable slice. Scope:

- graph vertex documents in the word index, one per authored vertex, for the
  published plane;
- the `GraphIndexPolicy` extension and its validation codes;
- activation-triggered whole-lane replacement keyed on generation
  fingerprint, through a new `IndexWriteOp`;
- authority filtering composed before ranking in the word lane;
- graph-selection gating and the fan-out cap before traversal expansion;
- result identity fields and `graph_logical_ref`;
- `bbox_project_graph_describe` participation reporting.

Explicitly out: vectors, the provisional and connector planes, evidence
expansion, placed-chunk back-pointers.

**Exit gate.** A query that names no ref finds a project-authored record
vertex, carrying its graph id, source label, and generation. A graph whose
policy disables text retrieval returns zero hits and its documents are
absent from the index, not merely filtered out of the response, proven by a
direct index assertion rather than by a search. A traversal that would cross
into an excluded graph does not enumerate its vertices. A generation flip
that removes a vertex removes its document, proven by a search that returned
it before and returns nothing after. The non-graph corpus's MRR and
recall@k are unchanged.

### 7.2 M9b: the connector plane

Scope: connector-managed source graphs in the index, the `graph_source`
filter, the collision rule (`visible_connector`) honored at query time, and
the operator review surface for connector retrieval policy.

Separated from M9a for a sequencing reason rather than a design one:
`crates/bbox-source-graph` is a workspace member but is **not** a dependency
of the `blackbox` crate or of `bbox-indexing`, and no daemon path
constructs a `SourceProjectionStore` today. The connector read lane exists
in `ProjectGraphViewCatalog` and is exercised only by its own tests. The
transport that feeds it is M4. M9b is therefore gated on M4 landing, and
carving it out keeps M9a shippable in the meantime.

**Exit gate.** A connector source graph is retrievable under
`graph_source="connector"` and under an unset filter, and a
project-authored graph of the same id shadows it in search exactly as it
does in `bbox_project_graph_list`. A reprojection that drops a vertex drops
its document. `retrieval_excluded_types` on a vendor-shipped schema removes
a type from search without editing the artifact.

### 7.3 M9c: provisional lanes

Scope: provisional graph vertex documents, the
`published | own | all` union at query time, overlay shadowing resolved at
filter time, tombstone handling, the `scope_lists_to_doc_type` widening, and
the `project_id` stamp that makes `keep_under_project_filter` work for the
provisional form.

Separated because provisional visibility is the part most likely to leak one
checkout's in-flight state into another's results, and it deserves its own
gate rather than riding along.

**Exit gate.** Two checkouts of one project author conflicting provisional
vertices; each retrieves its own under `own` and neither under `published`;
`all` returns both, labeled by asserting checkout. A provisional tombstone
hides the published vertex for the authoring checkout and for no one else.
Removing the workspace removes its provisional documents. A provisional hit
carries a pasteable `graph_logical_ref`.

### 7.4 M9d: evidence expansion and placed-chunk back-pointers

Scope: `expand_evidence` on search results via
`EvidenceBindingSet::forward`, the `project_file` back-pointer via
`EvidenceBindingSet::reverse`, per-hop endpoint status on expanded paths,
grouped-by-checkout results under `all`, and the bounding budgets.

**Exit gate.** The campaign's own M9 gate: a record query finds the
appropriate project, expands to its source objects and files, and returns
bounded cited content with provenance intact. Additionally: a search hit on
a placed file names the source vertex that placed it and the binding's
assertion authority; a connector reprojection that moves the source vertex
changes the back-pointer's status to stale without deleting it.

### 7.5 M9e: schema-directed embeddings

Scope: `Bucket::Graph`, the composed embed-eligible projection and its
version constant, `retain_authorized_graph_vectors`, the three-way
participation gate, and `bbox_reembed` coverage.

Last because it is the only slice that is purely additive to result quality
rather than to capability, and because it is the slice whose ranking effect
must be measured rather than assumed.

**Exit gate.** A paraphrase query that misses on BM25 finds an
`embed: true` claim-shaped vertex through the vector lane. A property that
requests `embed` under a policy with `embeddings_enabled: false` fails
schema validation rather than being silently skipped. A vector hit for a
graph the caller cannot read is dropped before fusion, proven by the score
of the surviving results being unchanged. Turning the route off degrades to
word-only with `degraded.skipped_partitions` populated and no error.

### 7.6 Deferred beyond M9

- **Cross-project graph retrieval.** The kernel defers cross-project graph
  references entirely; retrieval cannot lead it.
- **A graph query language.** Structural predicates over vertex properties
  ("every invoice over X in state Y") are a different capability from
  ranked retrieval and should not be smuggled in as search parameters.
- **A dedicated graph RRF lane.** Section 4.2. Revisit only with a metrics
  sweep.
- **Per-property tenant overrides of a vendor schema's annotations.**
  Section 3.2.
- **Identity-derived and tenant-scoped authorization.** M10.
- **Automatic promotion of graph facts into knowledge.** Out of scope for
  the whole campaign, not just this milestone.

## 8. Verification strategy

Layered, extending the campaign's matrix rather than replacing it:

- **Policy and annotation tests.** Every combination of graph policy and
  property annotation resolves to a determinate participation decision, and
  a contradictory combination is a validation error with a stable code, in
  the style of the existing `schema.*` inventory.
- **Index lane tests.** Whole-lane replacement on generation flip; no-op on
  fingerprint match; removal on tombstone; meta vertices never emitted;
  unannotated property values absent from the term dictionary. The last of
  these must be asserted directly against the index rather than through
  search, because a value can be indexed and merely not matched by the
  queries a test happens to run.
- **Filter-order tests.** The authorization filter is proven to run before
  ranking by asserting that the fused scores of visible results are
  identical with and without unreadable content present in the corpus. A
  post-filtering implementation fails this and a pre-filtering one passes,
  which no result-set assertion can distinguish. The same assertion covers
  the vector lane's per-hit retain.
- **Traversal gate tests.** An unreadable graph produces no frontier entry,
  no truncated path, and no count that implies its existence. A high-fan-out
  synthetic source graph terminates within the cap and says it truncated.
- **Ranking regression.** The non-graph corpus's MRR and recall@k are
  measured before and after each slice with `bbox-corpus-core`
  `search/metrics.rs`. A graph milestone that degrades code and transcript
  retrieval has failed regardless of its own numbers.
- **Rebuild tests.** Full reindex from accepted generations reproduces the
  index with no producer contact.
- **Isolation.** Per the repo's test invariants: per-test tempdirs,
  canonicalized roots before path assertions, `SharedState::for_test`, never
  the real index or the prod daemon.

Public-safe fixtures only. No live tenant data in fixtures, snapshots, or
examples. `crates/bbox-source-graph/tests/synthetic_api_dataset.rs` is the
existing synthetic API-dataset connector and is the natural fixture source
for the connector-plane slices.

**Documentation debt this milestone should clear.**
`docs/graph-retrieval-internals.md` currently describes three ranked lanes
(it omits the knowledge lane), states the RRF formula without the per-lane
weight, and does not mention the model rerank stage at all.
`docs/index-embedding-internals.md` lists six embedding routes against eight
buckets and stops its schema-tag table at `g5` against a current `g12`. A
milestone that adds a lane-adjacent entity family and a route to those
surfaces should correct them rather than adding to a stale account.

## 9. Open questions

Triaged in the program doc's style: each carries a standing recommendation
so the absence of a decision does not block the slice that needs it.

**Q1. Do graph vertices need reserved slots in the top-k?**
A query whose answer is one record vertex can be swamped by file chunks that
share vocabulary. *Recommendation: no reservation in v1.* Modal
diversification already exists and already balances code, docs, and commits;
adding a second unswept constant before measuring is how ranking chains rot.
Decide from M9a's metrics, not from feel. If it is needed, the cheapest form
is adding `project_graph_vertex` to `diversify_by_chunk_kind`'s
`TARGET_KINDS` rather than a new mechanism.

**Q2. One document per vertex, or per (vertex, text property)?**
*Recommendation: per vertex,* for the reasons in section 4.1. Revisit only
if measurement shows large property bags diluting BM25 term statistics
enough to matter.

**Q3. One `Bucket::Graph` route, or per-graph routes?**
*Recommendation: one route in v1.* Per-graph routes multiply partitions, and
partitions are the thing whose compatibility family (model plus dimension
plus dtype) has to be managed. A graph that genuinely needs a different
model is a real signal, and the route config is already per-route, so the
extension is available when a case exists.

**Q4. Should connector graphs be word-indexed by default?**
*Recommendation: yes for labels, per decision `b1a11d7cf59f2545`, with
`retrieval_excluded_types` as the tenant's recourse.* The alternative,
requiring per-graph opt-in for the connector plane, makes the common case
("I connected a source and now I want to search it") require an edit to a
vendor-shipped artifact.

**Q5. What pins the graph view during a query?**
*Recommendation: the graph view pin travels with the searcher pin, resolved
before the catalog lock is taken.* The pipeline already refuses to mint a
searcher mid-call, for exactly the reason that a commit landing between
lanes filters against a different generation. Reading the graph view live
while ranking a pinned index reintroduces the bug in a new place, and doing
it under the lock reintroduces the re-entrancy deadlock
`src/project_graph_read.rs` already documents.

**Q6. How does the project filter reach a provisional graph vertex?**
The `provisional_project_graph_vertex` ref carries `scope_hash` and
`checkout_id`, not `project_id`, so `keep_under_project_filter` cannot parse
its way to a project. *Recommendation: stamp `project_id` into the
provisional graph document at index time and filter on the field.* The
installer knows the project id; deriving it at query time would mean a
catalog lookup inside the filter, which is both a lock-ordering hazard and a
per-hit cost. This is the same shape as the existing
`knowledge_scope_hash` / `knowledge_checkout_id` fields.

**Q7. Where does authorization input come from before M10?**
*Recommendation: v1 authorization is project scope plus provisional
visibility plus per-graph policy, and nothing else.* The design names the
M10 seam (a term added to the filter conjunct) rather than inventing a
placeholder tenant claim, because a placeholder that is never exercised is
indistinguishable from a bypass when it is finally replaced.

**Q8. Should evidence expansion be on by default?**
*Recommendation: off,* with `next_steps` pulling toward it when the top
seeds are graph vertices. The campaign's own concern is not dumping whole
source graphs into model context, and a default-on expansion is that failure
mode with extra steps.

**Q9. What happens to a search hit whose graph generation advances between
ranking and result shaping?**
*Recommendation: the pin from Q5 makes this unrepresentable within one
call.* Across calls, a result's `graph_generation` is what tells a caller an
earlier hit is now stale, which is why it is a result field rather than a
diagnostic.

**Q10. Does the `visibility` / `provisional` parameter split get fixed
here?**
Section 6.5. *Recommendation: align on `provisional` and accept `visibility`
as a deprecated alias on the `bbox_project_graph_*` family.* It is a small
correction, it is cheapest while that family is being touched for
participation reporting, and a third spelling arriving with M9 would make it
permanent.

## 10. Relationship

- **Owned by:**
  [Graph-native connector campaign](reflective-graph-connector-program.md),
  section 15 (Milestone 9). That document owns sequencing, release
  increments, and the campaign invariants; this document owns the retrieval
  contracts.
- **Extends:**
  [Reflective Project Graph](../corpus/agentic-corpus/reflective-project-graph.md),
  which deferred full-text and vector indexing of graph vertices out of v1
  and points at the connector program for the follow-on.
- **Builds on:** the M2 source projection contracts
  (`crates/bbox-source-graph`) and the M3 evidence binding lane
  (`crates/bbox-project-graph/src/evidence.rs`), both landed on
  `beta/blackbox-v2`; the existing hybrid retrieval pipeline described in
  [`docs/graph-retrieval-internals.md`](../../docs/graph-retrieval-internals.md)
  and [`docs/index-embedding-internals.md`](../../docs/index-embedding-internals.md),
  with the corrections noted in section 8.
- **Gated on:** M4 for the connector plane (section 7.2), because the source
  projection store is not yet wired into the daemon.
- **Constrained by:**
  [Locality-first decomposition](../daemon-runtime/locality-first-decomposition.md).
  Everything M9 writes is a corpus-side projection of already-accepted
  state; no retrieval path contacts a producer or a remote system.
- **Hands off to:** M10 for identity-derived and tenant-scoped
  authorization, which adds a term to the filter this milestone composes.
- **Gap ledger:** resolves the design frame for `gap-5d57d2bb` (graph
  participation in unified retrieval). The gap closes on the M9 slices
  landing, not on this document.
