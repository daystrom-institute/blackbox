---
title: "API-Dataset Connector"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - connectors
tags:
  - connectors
  - api-dataset
  - locality
  - producer
  - source-graph
  - projection
  - checkpoints
  - actions
  - artifact-catalog
  - xero
brief: "The third connector profile: a typed business system becomes corpus-inspectable as a connector-owned source graph. A producer satellite observes typed records against named checkpoint sets and ships them over a dedicated dataset lane; the corpus projects accepted observations deterministically into the M2 source-projection store under a versioned source schema shipped as a catalog artifact; declared actions carry typed inputs, a placement policy, and their own grant, and execute producer-side on a claimed work item so no corpus process ever calls a vendor."
date: 2026-08-14
---

# API-Dataset Connector

> **Status: proposed. The profile itself is new work.** What is landed and what
> this design consumes rather than invents: connector scope identity
> (`ProjectScope::Connector(ConnectorScope { connector_source_id,
> connector_kind })`, operator-minted, provider coordinates recorded as
> observations); the `[source_connectors]` grant family with its per-lane
> `ConnectorProfile` discriminant and its one-producer-per-`connector_source_id`
> rule; the file lane (`bbox-file-source` wire crate, `bbox-file-collector`
> satellite, manifest generations, stage-then-flip activation); the conversation
> lane (`bbox-conversation-source` wire crate, server-owned per-channel cursors,
> journaled acceptance with held revisions); and the M2 source-projection
> substrate `bbox-source-graph` (`SourceProjectionStore`, `GraphDelta`,
> `NamedCheckpointSet`, `ReconciliationMode`, content-addressed observation
> retention with `ReplayPlan`, and `SourceProjectionStatus`). The dataset wire
> lane, its satellite, the schema-directed projection, and the action surface
> are all new. Reverify every contract name against code before building.

Resolves the design frame for `gap-0378c305`. Owned by
[the graph-native connector campaign](reflective-graph-connector-program.md);
this document is the profile design that campaign milestone M4 names, and it
does not restate the campaign's invariants, authority planes, or release
sequence.

## 1. Thesis

A typed business system (an accounting ledger, a CRM, a ticketing system, a
billing platform) is not a tree of named blobs and it is not an append-only
message log. It is a set of **entity collections behind a paginated, rate
limited API**, each collection refreshing on its own cadence with its own
delta support, related to the others by vendor-issued identifiers.

Such a system becomes corpus-inspectable by running a **dataset satellite** on
a producer host. The satellite holds the vendor credential, reads a
**versioned source schema shipped as a catalog artifact**, normalizes vendor
JSON into typed observation records declared by that artifact, and publishes
bounded batches with a proposed **named checkpoint transition** over a
dedicated authenticated lane. The corpus accepts the batch, retains it content
addressed, and interprets the same schema artifact to project it
deterministically into a **connector-owned source graph** through the landed
`SourceProjectionStore`.

Three properties are the point, and each is a property the other two profiles
cannot supply for this shape:

- **The domain objects never become blackbox types.** Xero contributes zero
  Rust variants, zero entity-reference families, and zero traversal branches.
  A vendor's semantics live in a schema artifact and a producer-side
  normalizer, both replaceable without a daemon deploy. This is campaign
  invariant 1, made mechanical.
- **The domain objects never become files.** The file-tree profile's bargain
  (exact deletes bought with a cheap full re-walk, whole-set manifest digest,
  generation as snapshot) inverts on an API dataset the same way the Slack
  design shows it inverting on a message log, and forcing invoices into
  synthetic documents would move entity-shaping decisions into the producer in
  violation of the enforced no-shaping-in-the-producer invariant.
- **The corpus never calls the vendor.** Observation legs and action legs are
  both producer-to-vendor. Every network initiation in this design is producer
  to corpus or producer to vendor; nothing ever reaches inward. That is what
  makes the satellite deployable behind NAT with no inbound listener, and it
  is what keeps the corpus host free of third-party egress.

## 2. Why a third profile and not a variation of an existing one

The three profiles share a spine and differ in exactly one axis: **what the
unit of durable acceptance is, and therefore what the wire quantum is.**

| | File-tree | Conversation | API-dataset |
|---|---|---|---|
| Remote shape | mutable tree of named blobs | append-only turn log | entity collections behind a paginated API |
| Wire quantum | whole-set manifest plus missing blobs | ordered per-channel record batch | typed observation batch per checkpoint transition |
| Whole-set digest | `manifest_sha256`, cheap | none | none |
| Cursor authority | producer-held cursor, `cursor_epoch` on invalidation | server-owned per-channel cursor | server-owned named checkpoint set, producer proposes compare-and-set advances |
| Deletion | falls out of the manifest diff | explicit tombstone record | `deleted: true` observation, or absence under `ReconciliationMode::Full` |
| Revision | new content hash, new generation | a separate revision record against a landed turn | an ordinary observation with a newer `remote_version` |
| Durable unit | one activated generation of chunked content | one landed conversation document | one accepted `SourceProjectionSnapshot` generation |
| Corpus landing | chunkers, Tantivy, vectors | transcript projection | schema-directed graph projection |

Two entries in that table are the load-bearing arguments.

**Revisions need no endpoint on this lane.** The conversation lane carries a
dedicated `POST /internal/conversation-source/v1/revisions` because a
conversation record is an immutable turn: an edit is a distinct fact *about* a
landed record, and the store journals it, holds it when the message has not
landed, and reports the held backlog. An API-dataset record is not immutable.
An updated invoice is simply the invoice, observed again, with a newer
`remote_version`. It replaces the prior vertex under the same identity. Adding
a revision endpoint here would give a producer two ways to say one thing, and
the second way would be the one that drifts.

**Deletion needs two mechanisms, not one.** The landed `ObservationRecord`
already carries `deleted: bool`, which covers a vendor that reports
tombstones. Many vendors do not: an entity that is voided or removed simply
stops appearing. That is why `ReconciliationMode` exists in the substrate with
`Full` as the only mode in which a projection may conclude that an unmentioned
vertex is gone, and why `GraphDelta::allow_empty_full_reconciliation` refuses
to let "the remote returned nothing" and "the remote is empty" look the same.
A checkpoint set declares which of the two mechanisms it can honestly use
(section 5.2); a set with neither is a set whose entities can never be
retired, and that fact belongs on the status surface rather than in a comment.

### 2.1 Rejected shapes, named

**Render each entity into a synthetic JSON file and ride the file lane.** This
is the shortcut the Slack design already rejected for messages, and every
reason it gave applies here plus one more. It pays a full re-enumeration per
cycle to compute the whole-set digest, which for a paginated rate-limited API
is the difference between one delta call and a multi-hour sweep. It moves
entity-shaping into the producer. It loses per-entity identity, so an
association between two entities becomes a textual coincidence between two
documents. And the added reason: the whole value of this profile is
*traversal*, and a file tree of JSON blobs has no edges, so the corpus would
have to reconstruct the graph from document content, which is projection with
the type information thrown away first.

**Ride the conversation lane with entities as pseudo-messages.** Its identity
is `(workspace_id, channel_id, message_ts)` with strictly increasing
timestamps as the uniqueness proof. An entity collection has no monotone
ordering key, entities update in place, and the lane's held-revision machinery
would fire on every ordinary update. The two lanes look adjacent because both
are cursor-shaped; they are not, because one landing is append-and-supersede
and the other is replace-in-place under a generation.

**Compile a per-vendor projection into the daemon behind `GraphProjection`.**
Tempting because the trait already exists and a hand-written Rust projection
is the easiest thing to write. Rejected as the default because it puts vendor
knowledge on the corpus plane in the immutable image, makes a schema fix a
daemon deploy, and makes the ratified decision to ship source schemas as
catalog artifacts decorative: an artifact nothing generically consumes is a
document, not a contract. The trait survives as the escape hatch (section 6.4)
and the generic schema-directed interpreter is the shipped path.

**Let the daemon call the vendor for actions.** Discussed and resolved in
section 7.1.

## 3. Vocabulary

| Term | Meaning |
|---|---|
| dataset satellite | Producer-host binary running one or more dataset adapters: observation, normalization, publication, action execution |
| adapter | Per-vendor code inside the satellite (`xero`); owns auth, pagination, rate limits, and vendor-JSON normalization |
| source schema | A versioned artifact declaring entity sets, checkpoint sets, the graph schema, the projection mapping, and the declared actions |
| entity set | One named collection of remote records (`invoice`, `file_association`) with an identity rule and a mapping onto one vertex type |
| checkpoint set | One named refresh unit with its own cursor value and its own delta support; a source has several |
| observation batch | The wire quantum: bounded typed records plus the checkpoint transition they justify |
| source graph | The connector-owned, rebuildable graph projection held by `SourceProjectionStore` |
| projection version | The identity of the mapping that produced a generation; equal to the schema artifact version |
| action | A declared, typed, bounded remote operation executed producer-side and authorized corpus-side |

A dataset source maps to exactly one durable corpus scope, exactly one source
schema artifact at a time, and one or more source graphs under that scope.

## 4. Architecture

```
producer host                             corpus host (the cage)

source schema artifact  <--------------  artifact catalog (authority)
  | pins schema_id + schema_version
  v
vendor API
  | delta / watermark / bounded sweep per checkpoint set
  v
typed observation records
  | normalize against entity-set declarations, bound and cap
  v
observation batch + proposed  --------->  authenticate, validate envelope,
checkpoint transition                     check schema pin, retain batch
                                          content addressed
                                                   |
                                                   v
                                          schema-directed projection
                                          (deterministic, no remote access)
                                                   |
                                                   v
                                          SourceProjectionStore::accept
                                          (graph facts + checkpoint set +
                                           observation refs, one snapshot)
                                                   |
status poll  <--------------------------  SourceProjectionStatus
action claim  <-------------------------  authorized action request queue
  | execute vendor legs
  v
action result  ------------------------>  bounded result, placement applied
```

Every arrow originates on the producer. The corpus is a receiver and an
authority; it holds the schema artifact, the accepted generation, the accepted
checkpoint set, and the action authorization, and it initiates nothing.

## 5. The dataset wire lane

A dedicated authenticated endpoint family mounted beside the existing two,
never reachable from model or shell authority:

```text
GET  /internal/dataset-source/v1/schema         accepted schema pin + projection version
GET  /internal/dataset-source/v1/checkpoints    accepted named checkpoint set
POST /internal/dataset-source/v1/batches        one observation batch + checkpoint transition
GET  /internal/dataset-source/v1/status         per-checkpoint-set freshness + generation
GET  /internal/dataset-source/v1/actions/claims  bounded long-poll for authorized action work
POST /internal/dataset-source/v1/actions/{request_id}/result
POST /internal/dataset-source/v1/catalog/onboard
```

Auth is the collector's, unchanged: a file-sourced bearer `ServiceToken` with
its owner, mode, symlink, hardlink, and shape checks; the header authenticates
a producer before the bounded body is parsed; the body's scope must be an
exact member of that producer's server-side allowlist before any request data
enters durable state; tokens never appear in query strings, bodies,
environment variables, MCP arguments, logs, or metrics; non-loopback plain
HTTP is refused producer-side and redirect following is off. One
authenticating middleware fronts the family, one handler per verb, each route
declaring its own body limit.

Onboarding mirrors the landed shape exactly: a `probed_*` request presented
over the authenticated channel, with the producer id **not** carried in the
body because it comes from the bearer, answered by a find-or-create receipt
carrying the project id, whether it was created, and the catalog epoch. A
configured scope with no catalog project is admitted at startup as
pending-onboarding, excluded from every publication lane, and acceptable only
to the onboard route.

**Route naming.** The lane token is `dataset-source`, matching the wire crate
stem `bbox-dataset-source` exactly as `file-source` matches `bbox-file-source`.
The operator-facing grant discriminant is `api_dataset` (section 9), because
config is operator text where "dataset" alone is ambiguous against a file
dataset. The asymmetry is deliberate and is named here once rather than
repeated at every mention.

### 5.1 Wire types

`bbox-dataset-source` is the leaf wire crate: dependency-clean, no corpus
edge, `V1`-suffixed types, bounded everything, in the house style of the two
landed wire crates. That style is specific and this lane inherits all of it:

- `#[serde(deny_unknown_fields)]` on producer-to-server payloads and durable
  records, deliberately omitted on server-to-producer responses, so a newer
  daemon can add a response field without breaking an older satellite while a
  producer can never smuggle one in;
- `#[serde(rename_all = "snake_case")]` on enums, `tag = "kind"` on tagged
  unions;
- two version constants, `SCHEMA_VERSION: u32 = 1` and
  `DATASET_POLICY_VERSION: &str = "dataset-source-connector-policy-v1"`, with
  skew a typed `unsupported_contract` rejection rather than a merge;
- digests domain tagged and length prefixed, so `batch_digest` folds under
  `b"bbox-dataset-source-batch-v1"` exactly as the two landed lanes fold under
  their own tags, and no digest is reusable across lanes by accident;
- ids that are corpus authority are derived server-side. `batch_id` is
  producer-supplied because it is an idempotency key the producer must be able
  to repeat; the generation number is the store's, derived as exactly one past
  the accepted generation, and never accepted from a caller.

```rust
pub struct DatasetObservationRecordV1 {
    pub observation_id: String,
    /// The source-side entity class, declared by the schema artifact
    /// (`invoice`, `file_association`). Source vocabulary, never a
    /// Blackbox enum.
    pub source_entity: String,
    pub remote_id: String,
    pub remote_version: String,
    pub observed_at: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

pub struct DatasetObservationBatchV1 {
    pub schema_version: u32,
    pub dataset_policy_version: String,
    pub scope: ConnectorScope,
    /// The schema artifact this producer normalized against. A mismatch
    /// against the corpus's accepted artifact is a typed refusal, never a
    /// best-effort merge.
    pub source_schema_id: String,
    pub source_schema_version: u64,
    pub graph_id: String,
    pub batch_id: String,
    pub reconciliation_mode: ReconciliationMode,
    /// Ordered by `(source_entity, remote_id)`; duplicates refused.
    pub records: Vec<DatasetObservationRecordV1>,
    /// Covers every field of every record, so a batch rewritten in flight is
    /// a rejected batch rather than a silently landed lie. The file lane's
    /// `manifest_sha256` and the conversation lane's `batch_digest` at this
    /// lane's quantum.
    pub batch_digest: String,
    pub checkpoint_transition: NamedCheckpointTransition,
    pub observed_at: String,
}
```

`DatasetObservationRecordV1` mirrors the corpus-side `ObservationRecord` field
for field, and `DatasetObservationBatchV1` mirrors `ObservationBatch` plus the
envelope. They are separate types for the same reason the conversation lane's
onboard request is a separate type from the file lane's: sharing would put a
corpus crate into the satellite's dependency tree and break the ceiling the
acceptance script enforces. A mapping test pins the two shapes field for field
so the mirror cannot silently drift.

The receipt:

```rust
pub struct DatasetBatchReceiptV1 {
    pub graph_id: String,
    /// `accepted` on a new generation, `duplicate` on an exact replay of the
    /// most recently accepted batch.
    pub outcome: BatchOutcomeV1,
    pub generation: u64,
    pub graph_fingerprint: String,
    pub checkpoints: NamedCheckpointSet,
    /// Association edges whose endpoint has not landed yet; see section 6.3.
    pub pending_associations: u64,
}
```

Errors ride the shared `ErrorResponse { code, message }` shape, and the
existing two-taxonomy split is preserved rather than blurred:

- **route-level codes are flat snake_case**, matching the landed lanes'
  `scope_forbidden`, `scope_wrong_lane`, `scope_pending_onboarding`,
  `unsupported_contract`, `limit_exceeded`, `catalog_unavailable`, and
  `storage_unavailable`, plus this lane's `invalid_dataset_source_input`,
  `schema_pin_mismatch`, `action_not_granted`, `action_input_invalid`, and
  `action_lease_expired`;
- **acceptance-level codes are dotted**, because they come from the
  substrate's own `ProjectionFailure` taxonomy (`projection.batch_conflict`,
  `projection.schema_rollback`, `projection.empty_full_reconciliation`,
  `checkpoint.conflict`, `observations.not_retained`). A refused acceptance
  surfaces its dotted code as the diagnostic inside a flat route response
  rather than being rewritten, so a test can assert on the contract clause
  that actually refused.

Message bodies are bounded, per-route body limits are declared, and every
store call crosses `spawn_blocking`, as on both landed lanes.

### 5.2 Named checkpoint sets and cursor semantics

Named checkpoint sets are **not new to this profile**, and it is worth being
exact about what is. The file collector already carries a producer-side
`CheckpointSet` (bounded name and token lengths, bounded cardinality) and
already folds it into an opaque display-only `remote_watermark`, and
`CursorDegradationV1` already carries a `checkpoint_name` alongside its cause
and its epoch. The corpus-side substrate already models the accepted set as
`NamedCheckpointSet { values: BTreeMap<String, String> }` with advances as
`CheckpointAdvance { before: Option<String>, after: String }` and a
compare-and-set refusal on mismatch.

What is new here is that **the corpus accepts the checkpoint set as durable
authority.** On the file lane the set is producer working state and the corpus
sees only a digest of it, because the corpus has no use for a cursor whose
only consumer is a re-walk. On this lane the accepted set is committed in the
same snapshot as the graph facts it justifies, which is what makes "this graph
is at generation N and these five sets are at these values" one atomic,
inspectable, resumable fact. This design settles what the names mean and who
owns them.

**The corpus owns the accepted set; the producer proposes atomic advances.**
This is a deliberate midpoint between the two landed lanes. The conversation
lane makes the server the sole cursor authority and the producer asks where to
resume, which makes producer reinstalls free. The file lane keeps the cursor
producer-side because it is a vendor-opaque token the corpus can do nothing
with. A dataset checkpoint is both: opaque to the corpus, and something two
satellites (or one satellite and its own restarted self) can race on. So the
producer reads the accepted set from `GET .../checkpoints`, proposes an
advance carrying the value it believed accepted, and a stale producer is
refused with `checkpoint.conflict` rather than winning by writing last. There
is no producer-side durable cursor state, and there is no last-writer-wins.

**A checkpoint set is declared by the schema artifact**, with a name matching
the substrate's `valid_checkpoint_name` rule (ASCII alphanumerics plus `.`,
`_`, `-`), the entity sets it refreshes, and its delta support:

- `delta`: the vendor issues a since-token or change feed. Incremental
  batches, cheap, and deletions arrive as tombstones.
- `watermark`: the vendor supports a modified-since filter but reports no
  deletions. Incremental batches carry updates; retirement requires a
  periodic `Full` pass over that set.
- `reconcile_only`: no delta support at all. Every refresh is a bounded full
  sweep of that set, which is affordable exactly when the set is small
  (a tracking-category list, a chart of accounts) and is a design error when
  it is not.

The declaration is not decoration. It determines whether the corpus may treat
an unmentioned vertex as retired, it determines the honest refresh cadence,
and it is what the status surface renders so an operator can see that one set
is a day stale because it only reconciles weekly rather than because the
producer is broken.

**Degradation is reported, never absorbed.** A vendor invalidating a delta
token is the same event the file lane handles with `cursor_epoch`, and it gets
the same treatment: the satellite discards the token, increments a per-set
epoch, runs a bounded full sweep, and records the degradation with its cause
and cost on publication telemetry. The epoch rides telemetry rather than the
checkpoint *value*, mirroring the file lane's `CursorDegradationV1`, because a
checkpoint value is a vendor token and stuffing corpus bookkeeping into it
would make an opaque field partly ours. The shape is already available: the
file lane's `CursorDegradationV1 { checkpoint_name, cause, cursor_epoch,
entries_enumerated, blobs_refetched, documents_reexported, observed_at }` is
the same record with different cost counters, and this lane's variant swaps
the byte counters for record counters. Repeated epoch increments are an
operator signal (revoked consent, tenant policy change, an adapter bug).

### 5.3 Batch bounds and idempotency

`batch_id` is the idempotency key, and the substrate's rule is already exact:
an exact replay of the **most recently accepted** batch is idempotent and
anything older is `projection.batch_conflict`. The wire adds nothing to that
rule and simply reports which of the two happened, because a producer's only
real question is whether it must send again.

There is no spool. The vendor is the durable backlog and a resweep from an
older checkpoint value is always safe, so a satellite that crashes mid-cycle
re-observes rather than replaying from local state. Losing the satellite's
working state costs API calls, never data.

Bounds, enforced on both sides because authentication proves the configured
producer and not the truth of its bytes: records per batch, body bytes,
payload bytes per record, entity-set cardinality per batch, and vertices and
edges per accepted generation. A policy-version skew between satellite and
server is a typed rejection, not a merge, exactly as on the file lane.

**Payload retention, not payload storage.** A record's `payload` is retained
content addressed by the landed observation store and referenced from graph
facts by `SourceObservationRef`. It is never copied into vertex properties
wholesale, never rendered on a status surface, and never logged. Traversal
must not require the payload; reprojection is the only consumer.

### 5.4 The satellite

`bbox-dataset-collector` is **one generic satellite with per-vendor adapter
modules**, following the file collector rather than the Slack collector.

The two landed satellites made opposite choices for good reasons. The file
collector is generic because its per-store surface is a `RemoteSourceConnector`
implementation behind a stable trait and everything around it (policy,
journal, logical paths, the publication cycle, the wire client) is genuinely
shared. `bbox-slack-collector` is per-vendor because Slack's read-method
allowlist, enrollment policy, thread sweeps, and reconciliation window are the
substance of the design rather than parameters of it.

This lane resembles the file collector. A vendor contributes a schema artifact
(data, not code) plus a normalizer that turns vendor JSON into records the
artifact declares, and the observation loop, checkpoint arithmetic, batching,
bounds, publication, status polling, and action claiming are shared by
construction. A per-vendor binary would fork all of that per adapter.

Module shape, mirroring the file collector one for one:

- `adapter`: the `DatasetAdapter` trait and its shared types, including the
  producer-side checkpoint set and its bounds;
- `schema`: the pinned source schema artifact, its entity-set declarations,
  and the normalizer contract that consumes them;
- `journal`: producer working state, losable, keyed on the checkpoint set;
- `cycle`: one publication cycle (read checkpoints, observe per set,
  normalize, batch, publish, poll status), written against a sink trait whose
  verbs mirror the routes one for one;
- `action`: claim, lease renewal, execution, result post;
- `client`: the `/internal/dataset-source/v1/*` wire client;
- `config`: the operator-declared satellite configuration;
- `fixture`: a filesystem-backed fixture adapter whose "remote" is a local
  JSON corpus with synthetic ids, versions, tombstones, and associations. No
  network, no OAuth, no vendor code.

Binary shape, also mirrored: `onboard` (idempotent find-or-create), `publish`
(one cycle, cron shape), and `watch --interval-secs` (daemon shape, where a
failed cycle logs and waits rather than exiting).

The dependency ceiling is enforced the same way both landed satellites enforce
theirs, by an acceptance script over the resolved dependency graph rejecting
Tantivy, the chunker, corpus-index, indexing, embedding, vector, the edge
index, the root package, and the harness. Vendor SDKs and OAuth machinery are
allowed; corpus knowledge is not.

## 6. Source schemas as catalog artifacts, and schema-directed projection

Operator decision 7650b743fb23c265 already ratified the shipping mechanism:
versioned corpus artifacts through the existing artifact catalog, not compiled
into connector binaries, with signed distribution deferred to the hosted
hardening milestone. This section settles what the artifact contains and what
consumes it.

### 6.1 The artifact

A new artifact kind `source_schema` joins the catalog's existing kinds,
installed and superseded through the ordinary surfaces
(`bbox_artifact_install`, `bbox_artifact_list`, `bbox_artifact_supersede`), so
schema lifecycle needs no bespoke tooling. The document:

```text
schema_id                    stable identity, e.g. "xero-accounting"
schema_version               monotone integer; the projection_version
namespace                    graph namespace, e.g. "xero"

graph_schema                 a GraphSchema document: vertex_types,
                             edge_types, index_policy

entity_sets[]
  source_entity              wire vocabulary, e.g. "invoice"
  vertex_type                target type inside graph_schema
  identity                   which observed field is the stable remote id
  properties[]               observed field -> vertex property, with a
                             declared value discipline
  associations[]             observed field -> edge type + endpoint entity set

checkpoint_sets[]
  name                       valid_checkpoint_name shape
  entity_sets[]              which sets this checkpoint refreshes
  delta_support              delta | watermark | reconcile_only

actions[]                    see section 7
```

`graph_schema` is validated by the same kernel that validates a project graph:
schema as data, no compiled variants, the fixed meta-schema floor unchanged.
Its per-property retrieval annotations are the landed ones
(`{ "type": ..., "index": "none|word|text", "embed": true|false }`) sitting
under the per-graph `GraphIndexPolicy { embeddings_enabled }` gate, so a
property that opts into embedding while the graph forbids it is a schema
error rather than a silent downgrade. This design adds no annotation
vocabulary; it only requires that a source schema author annotates as they
write, because the unified-retrieval milestone consumes these annotations and
cannot retrofit them.

**Both planes read one artifact.** The satellite pins `(schema_id,
schema_version)` and normalizes against the artifact's `entity_sets`
declarations; the corpus interprets the same declarations to project. The
batch envelope carries the pin, and a mismatch against the corpus's accepted
artifact is `schema.pin_mismatch` before any durable write. Upgrading a schema
is therefore a two-sided operation with an explicit ordering (install the new
artifact, then roll the satellite, or the reverse with a refusal window), and
never a silent partial adoption. The substrate's `projection.schema_rollback`
refusal covers the other direction: an accepted generation's schema version
never goes backwards.

### 6.2 The projection is a generic interpreter

`SchemaDirectedProjection` implements the landed `GraphProjection` trait by
interpreting the artifact. Given a context, a batch, and the prior snapshot it
emits a `GraphDelta`:

- one observed record maps to one vertex of the declared `vertex_type`, with
  vertex id `"<source_entity>:<remote_id>"`, opaque, never a vendor URL, a
  display name, or a path;
- a record already present in the prior snapshot lands in `replaced_vertices`;
  an unseen one in `inserted_vertices`; a `deleted: true` record in
  `removed_vertex_ids`;
- declared `associations` emit edges of the declared type, with the endpoint
  resolved to `"<endpoint_entity>:<remote_id>"`;
- under `ReconciliationMode::Full` over a checkpoint set whose declared entity
  sets are fully covered by the batch, unmentioned vertices of those sets land
  in `removed_vertex_ids`. Under `Incremental` they never do;
- an accepted batch that changes nothing but a checkpoint value still produces
  a generation, using the substrate's `GraphDelta::empty`, because the graph
  and the checkpoint set are accepted together and a checkpoint that advances
  without an accepted snapshot would be a durable claim with no witness.

Determinism is a contract, not an aspiration: ordering inside the delta
vectors is part of the value because the commit fingerprint that makes replay
idempotent is taken over the serialized delta. The interpreter therefore emits
in a total order derived from `(source_entity, remote_id)` and edge triples in
`GraphEdgeKey` order, with no reliance on map iteration or input order.

**The mapping vocabulary is deliberately small**: field projection, rename,
type coercion into the schema's declared property types, safe passthrough of
unknown enum values, absence preserved as absence rather than defaulted, and
association fan-out. It has no conditionals, no arithmetic, no string
templating, and no code hook. Where a vendor needs a computed fact, the
computation belongs in producer-side normalization, which is already where
vendor archaeology lives and already redeployable without a daemon deploy.
The pressure to grow this vocabulary into a programming language is real and
is named in the open questions.

### 6.3 Held associations

An incremental lane observes associations and their endpoints on different
checkpoint sets with different cadences, so an association can legitimately
arrive before the entity it points at. The substrate refuses dangling edges,
correctly. The three available responses are: refuse the batch, drop the edge,
or hold it.

**Hold it, and report the backlog.** A declared association whose endpoint is
neither in this batch nor in the prior snapshot is recorded as a pending
association on the accepted snapshot and applied on the first generation where
its endpoint lands. It is never inserted as a dangling edge and never
discarded. The precedent is exact: the conversation lane journals a revision
for a message it has not landed as `held`, applies it when the message
arrives, and surfaces the running total on the cursor and the status read
precisely so an operator can watch the backlog close rather than infer it. A
standing nonzero pending-association count is a real signal: either a
checkpoint set is lagging its partner, or the schema declares an association
into an entity set nobody observes.

This requires an additive field on `SourceProjectionSnapshot` and a
corresponding bump of `SOURCE_PROJECTION_STORE_VERSION`, which the store
already gates with `projection.unsupported_store_version`. It is called out
as a substrate change rather than smuggled in, and it is phased (section 11,
A3) rather than assumed present.

### 6.4 Reprojection, and the trait as escape hatch

A schema artifact upgrade is a reprojection, not a re-observation. The corpus
asks the store for a `ReplayPlan` from the generation the old schema last
accepted, replays the retained batches through the new interpreter, and
accepts the resulting generations. Where the plan is not `complete`, because
the retention horizon has passed the requested baseline, the honest outcome is
already specified by the substrate: degrade to re-observe rather than project
a partial history. Retention defaults to the current plus prior generation
plus everything inside a window, per source class and widenable by deployment
policy, which means a schema author working iteratively should widen the
window before starting rather than discover the horizon mid-iteration.

The `GraphProjection` trait stays public and stays the escape hatch. A vendor
whose semantics genuinely cannot be expressed declaratively can carry a
compiled projection, at the explicit cost of a daemon deploy per schema fix
and a corpus-plane dependency on vendor knowledge. No shipped adapter should
take it, and taking it is a design escalation with a written reason, not an
implementation convenience.

## 7. Declared actions

An action is a **targeted, bounded, typed remote operation** that produces
evidence without replicating a whole source. It is the profile's answer to the
observation that a durable overlay is expensive and often unnecessary: the
question "which files support this project code" does not require the books.

### 7.1 The producer/collector axis, and why actions are polled

The campaign says actions are invoked through the daemon (which owns
authorization, limits, and witness acceptance) and execute their remote legs
on the producer satellite, riding an authenticated channel rather than a
daemon-side vendor call. It deliberately leaves the direction of the
corpus-to-producer leg open. **This design resolves it to a producer-polled
work queue.**

The daemon accepts an action *request*: authorized against the grant,
validated against the artifact's declared input schema, bounded, and journaled
with a `request_id`. It initiates nothing. The satellite claims work on its
own cycle through a bounded long-poll on
`GET /internal/dataset-source/v1/actions/claims`, executes the vendor legs,
and posts a bounded result.

The alternative, a daemon-to-satellite call, was rejected for three reasons.
It requires the satellite to run an inbound listener with its own auth
surface, which doubles the trust edges and makes a NAT-resident or
laptop-resident producer undeployable. It makes the corpus an initiator, which
is the property the locality split spent a decomposition removing. And it
turns an offline producer into a connection error at the caller instead of a
pending request with visible lag, which is strictly worse operationally: lag
is a number an operator can watch, and a stack trace is not.

The cost is honest and bounded: action latency is at least the claim poll
interval. The mitigation is a bounded long-poll (the claims route holds the
request open for a configured span and returns empty on expiry), which keeps
every initiation producer-side while making the practical latency the vendor's
rather than the scheduler's.

### 7.2 Typed inputs and declaration

An action is declared in the schema artifact:

```text
actions[]
  action_id                  e.g. "xero.files_for_project_code"
  input                      declared field types, required set, bounds
  output                     what the result carries: which entity sets,
                             which content refs, which unresolved cases
  placement_default          ephemeral | project_owned | connector_cache |
                             external_only
  limits                     max remote calls, max bytes, max results,
                             wall-clock ceiling
```

The daemon validates a request against `input` before journaling, refusing
unknown fields and out-of-bounds values with `action.input_invalid`. The
declaration is the entire vocabulary: there is no free-form passthrough
parameter, no vendor endpoint selector, and no query string. An action surface
that could express an arbitrary vendor call would be a vendor proxy with an
authorization table, which is precisely the shape the onboarding trust model
rejects.

### 7.3 Authorization posture

**Observation is read-only and needs no action grant. Actions are off unless
the grant declares them.** The `ConnectorScopeGrant` gains
`#[serde(default)] actions: Vec<String>`, an allowlist of `action_id`s, empty
by default. So an api-dataset grant with no `actions` key is observation-only,
which is the posture every existing grant already has and the posture a new
one gets by writing nothing.

Enabling an action is a second, separate operator declaration on the corpus
side, and it is meaningless without the corresponding vendor scope on the
producer side, so the two-sided property holds for actions exactly as it holds
for onboarding. An `action_id` present in the artifact but absent from the
grant is `action.not_granted` at request time, before journaling.

No agent tool creates a source, enables an action, or widens a grant. Where an
agent-facing invocation surface for an *already granted* action exists, it is
a request into the authorized queue and nothing more; it cannot name an
undeclared action, cannot exceed the artifact's limits, and cannot select a
placement the grant does not permit.

### 7.4 Execution, leases, and delivery semantics

A claimed request carries a **lease**. A satellite that dies mid-action loses
its lease on expiry and the request becomes re-claimable. That is at-least-once
against the vendor, and it is stated rather than hidden.

**v1 actions are read-shaped**, so at-least-once is harmless: a repeated
fetch costs quota. A write-shaped action is an explicit non-goal (section 10)
and must not be added without a per-action idempotency contract that the
vendor can actually honor. The satellite journals claim, call, and result in
that order, so a crash between call and result is re-executed and a crash
after result is not.

The result post is idempotent on `request_id`: a producer whose result post is
refused (a rotation window, a restart, a transient) retries the same post
rather than re-executing the action.

### 7.5 Placement

`PlacementPolicy` is the campaign's, unchanged in meaning:

- `ephemeral` returns a bounded result and retains no durable bytes. **This is
  the default for every v1 action.**
- `external_only` retains remote references and metadata on the source graph
  and no bytes.
- `connector_cache` retains bytes under connector lifecycle and retention
  controls, producer-side working state, losable.
- `project_owned` publishes the bytes into the corpus as content-addressed
  source content.

**Ruling: `project_owned` bytes do not ride the dataset lane. They ride the
file lane.** A dataset lane that could also ship blobs would be a second blob
transport with its own manifest, generation, and activation semantics, all
duplicating the file lane's for no gain. So an action that places bytes
publishes them through `/internal/file-source/v1/*` under a **file-profile
connector scope**, and the source graph binds the placed file to its source
vertex with an evidence edge.

The consequence is concrete and worth stating plainly: because the landed
grant validation binds one `connector_source_id` to one producer and one
profile, placing bytes today requires the operator to mint a **second**
connector source with `profile = file`, declared alongside the dataset one.
That is two scopes and two projects for one logical integration. It is
correct (the placed documents genuinely are an ordinary searchable file
project, and they survive the dataset source's removal) and it is clunky. The
alternative, relaxing uniqueness to `(connector_source_id, profile)` pairs
under a single producer, is a small config change with a real invariant cost,
and it is the open question in section 12.

## 8. Status surfaces

The status read is `bbox_project_publisher_status`, per the file lane's
precedent. That tool already renders a `connector` object for a
connector-scoped project carrying `connector_source_id`, `connector_kind`,
observations, and a `publication_lanes` list that today holds exactly
`["file_source"]` with a `file_source` sub-object beside it. This lane adds
`"dataset_source"` to that list and a sibling `dataset_source` object, using
the same read-failure discipline: an unreadable store renders
`{ readable: false, diagnostic }` with a bounded diagnostic and is never
fatal to the status read.

The body is the landed `SourceProjectionStatus` plus this lane's per-set
detail. It carries no credential material, no observation payload, and no
vendor response body, which the substrate already enforces by construction.

Per source graph, from the substrate: `generation`, `graph_fingerprint`,
`schema_id`, `schema_version`, `projection_version`, `source_connector`,
`last_batch_id`, `latest_observed_at`, `reconciliation_mode`,
`retained_observation_count`, and `prior_generation_available`.

Per checkpoint set, added by this lane:

```text
name
accepted_value            opaque, rendered as an opaque token
delta_support             delta | watermark | reconcile_only
last_advanced_at
lag_seconds               None when the set has never advanced
epoch                     increments on every degradation to a full sweep
degradations              bounded per-cause counters with last-seen cost
records_observed          bounded counters, by entity set
```

`lag_seconds` is the headline, for the reason the conversation lane already
established: a throttled producer must show up corpus-side as lag rather than
as errors, so lag is a first-class number rather than something inferred from
timestamps. And `None` is distinct from zero and must never render as healthy;
a checkpoint set that has never advanced is a set nobody has proven works.

Per source, added by this lane: `pending_associations` (section 6.3),
`pending_actions`, `leased_actions`, `last_action_at`, and the schema pin the
last accepted batch presented. Health and `bbox_doctor` render the same facts.
Removal is config removal plus an explicit retirement ceremony, never a tool
call that deletes bytes.

## 9. Grant family extension

`ConnectorProfile` gains a third variant:

```rust
pub enum ConnectorProfile {
    #[default]
    File,
    Conversation,
    ApiDataset,     // serializes as "api_dataset"
}
```

Everything about that addition is already settled by the landed shape and its
recorded reasoning, and this design changes none of it:

- the discriminant lives on `[source_connectors]` rather than in a parallel
  config family, because the one rule that protects the catalog is that a
  `connector_source_id` is granted to exactly one producer, and
  `validate_source_connectors` enforces it by walking a single table;
- `connector_kind` cannot carry the lane. It is the operator's open-ended
  declaration of which connector family serves the source (`xero`), it is
  durable catalog data, and a closed lane discriminant that a route layer
  switches on must not be inferred from an open-ended label;
- `profile` is `#[serde(default)]` to `File`, so every config written before
  this variant existed keeps its exact meaning, byte for byte;
- the scope family is unchanged. `ConnectorScope { connector_source_id,
  connector_kind }` is the durable identity, `remote_authority` remains an
  operator expectation that never reaches the catalog scope, and provider
  coordinates remain observations.

A grant example:

```toml
[[source_connectors.producers]]
producer_id = "dataset-sat-1"
token_file = "/var/lib/bbox/secrets/dataset-sat-1.token"

  [[source_connectors.producers.scopes]]
  connector_source_id = "xero-acme-0f3c9a21b4d7"
  connector_kind = "xero"
  remote_authority = "<vendor tenant, operator text>"
  profile = "api_dataset"
  # observation-only: no actions key
```

**Route admission is by profile, both ways, and the mechanism already
exists.** The runtime grant table already keeps a
`connector_source_id -> ConnectorProfile` map and already answers
`profile_for`, and both landed lanes already refuse a mismatched scope with
the flat code `scope_wrong_lane` before any request data enters durable state.
Adding the variant extends that map; it does not invent the check. A scope
granted with `profile = "file"` presenting on the dataset lane is refused, and
the converse. The lane check is the cheap mechanical half of keeping a source
honest about what it is.

### 9.1 Interaction with producer-grant rotation (`gap-bb84c77f`)

A grant names exactly one `token_file` per producer today, so every rotation
is a simultaneous two-sided cutover with a guaranteed refusal window. That gap
proposes an ordered list of accepted tokens per `producer_id` with
constant-time verification and matched-token observability.

The interaction with this profile is specific and worth recording rather than
discovering:

- **For observation, the existing failure mode is survivable.** A refused
  batch during a rotation window costs a retry, because the vendor is the
  durable backlog and a resweep from the accepted checkpoint value is always
  safe. This is the same "tolerate a brief red publish cycle" fallback the gap
  already documents.
- **For actions, it is worse, and bounded by the idempotency contract.** A
  refused *result post* would strand work the vendor already performed. That
  is why section 7.4 makes the result post idempotent on `request_id` and
  keeps the lease alive across the refusal: the producer retries the same post
  rather than re-executing. It is also a second reason v1 actions are
  read-shaped.
- **Ruling (operator, 2026-08-16).** Overlap-tolerant grants have landed
  (`token_files` on both grant families), so this is a validator rule, not a
  runbook caveat: an `api_dataset` grant that declares actions must carry
  `token_files`; declared actions with a single `token_file` refuse at config
  validation. The rotation runbook still names the action queue (drain to
  zero leased actions is the tidy cutover), but correctness no longer depends
  on it.

The gap's fix is additive to this design and needs nothing from it.

## 10. Non-goals

- **No daemon-side vendor call**, for observation or for actions, not as a
  fallback, not for small sources, not for tests. A fixture dataset adapter
  runs in the satellite like every other adapter.
- **No write-shaped actions in v1.** Every declared action reads. Adding a
  mutating action requires a per-action vendor idempotency contract and its
  own operator grant semantics, and it is not smuggled in behind the existing
  action surface.
- **No vendor proxy.** The artifact's declared action inputs are the whole
  vocabulary. No free-form endpoint, method, query, or body reaches an
  adapter from a request.
- **No agent self-service.** No MCP tool creates a dataset source, installs a
  schema artifact into a grant, enables an action, triggers observation, or
  removes bytes.
- **No vendor semantics in blackbox core.** No entity-reference family, enum
  variant, or traversal branch is added for any vendor. Source graph vertices
  reach the rest of the corpus through the existing generic
  `project_graph_vertex` reference family and generic evidence edges.
- **No blob transport on this lane.** Placed bytes ride the file lane
  (section 7.5).
- **No durable whole-source overlay as a prerequisite.** The action-first
  release produces value while the durable overlay stays deliberately narrow.
  An overlay is a continuous publisher and inherits the corpus host's
  per-cycle activation and index-churn cost profile; its cadence and caps are
  justified against that cost, not assumed free.
- **No tenant-authored record graphs here.** Record graphs, mappings, and
  cross-plane evidence bindings are separate campaign milestones with separate
  authority. A connector refresh never edits tenant-authored facts.
- **No graph query language, lifecycle system, or automatic knowledge
  promotion.** The kernel stays small.
- **No unified retrieval commitment.** Whether source graph vertices
  participate in text and hybrid retrieval is the campaign's M9 and
  `gap-5d57d2bb`. What this design owes it is annotation completeness at
  schema-authoring time, nothing more.

## 11. Phases and gates

**A0. Contracts and the artifact kind.** The `source_schema` artifact kind,
its document shape, its validation against the graph kernel, and the
`bbox-dataset-source` wire crate with its bounds and its mirror test against
the corpus observation types. No network, no lane mounted.
*Gate:* a fixture schema artifact installs, validates, declares entity sets,
checkpoint sets, and one action; a schema whose property annotation opts into
embedding under a policy that forbids it is a schema error; a wire type whose
field set drifts from its corpus mirror fails a test.

**A1. Grant and lane admission.** `ConnectorProfile::ApiDataset`, the
`/internal/dataset-source/v1/*` mount, the onboarding composite, and
pending-onboarding admission.
*Gate:* an api-dataset scope onboards, lists, and reports with no publication
yet; a scope granted under another profile presenting on this lane is refused
before any durable write; a config written before the variant existed parses
to exactly the same value; a non-granted scope is refused.

**A2. Satellite substrate against a synthetic dataset (no vendor, no OAuth).**
`bbox-dataset-collector` with the wire client, a fixture adapter whose
"remote" is a local JSON corpus with synthetic ids, versions, tombstones, and
associations, plus `SchemaDirectedProjection` and acceptance into
`SourceProjectionStore`.
*Gate:* the campaign's M2 exit gate driven end to end over the wire, meaning
create, update, delete, checkpoint resume, and schema reprojection; a stale
checkpoint advance is refused with `checkpoint.conflict`; an exact replay of
the most recent batch is a reported duplicate and an older replay is refused;
an `Incremental` batch never retires an unmentioned vertex and an empty `Full`
batch without explicit permission never empties the graph; the dependency
acceptance script passes with no corpus, chunker, index, vector, or harness
crate in the satellite tree; no vendor credential exists anywhere in the test.

**A3. Held associations and reprojection.** The additive snapshot field, the
store version bump, and the reprojection path over `ReplayPlan`.
*Gate:* an association observed before its endpoint is held, counted on
status, and applied on the generation where the endpoint lands, with no
dangling edge at any point; an artifact version bump reprojects deterministically
from retained observations; a bump beyond the retained horizon degrades to
re-observation with a named reason rather than projecting a partial history.

**A4. Actions, read-shaped, ephemeral only.** The claims long-poll, leases,
typed input validation, the grant allowlist, and the idempotent result post.
*Gate:* an `action_id` absent from the grant is refused before journaling; an
input violating the declared schema or bounds is refused before journaling; a
satellite killed mid-action has its request re-claimed after lease expiry and
the result lands exactly once against `request_id`; an offline producer shows
the request as pending with lag on the status surface rather than as an error
at the caller; an action exceeding its declared call, byte, result, or
wall-clock limit aborts and is reported.

**A5. The first vendor adapter.** OAuth through the secrets layer, real
pagination and rate limits, recorded public-safe fixtures, and the first
evidence action end to end (campaign M5 and M6).
*Gate:* recorded fixtures project deterministically and survive a schema
version replay; unknown enum values and absent optional fields are preserved
per policy; money is exact; the evidence action covers attachment-only,
association-only, both-lane duplicate, missing mapping, partial authorization,
pagination, rate-limit resume, and bounded-download behavior.

**A6. Durable placement.** `project_owned` through a paired file-profile
scope, with evidence edges binding placed files to source vertices.
*Gate:* placed bytes are searchable as an ordinary connector file project with
no dataset-specific branch on any read path; the binding survives a source
graph rebuild as valid, stale, or unresolved without fact loss; removing the
dataset source leaves the file scope diagnosable rather than orphaned.

A0 through A4 have no external-service dependency and no OAuth surface, which
is what makes them a safe substrate proof, exactly as the file lane's phase 1
is. A5 onward is one adapter plus its auth flow.

## 12. Open questions

**Operator ruling, 2026-08-16.** Items 1, 3, 4, 5, 6, and 7 were put to
the operator and each was ratified as its standing recommendation, with
item 6 tightened: because overlap-tolerant grants (`token_files`, the
`gap-bb84c77f` fix) landed on both grant families in the same fold as this
design, an `api_dataset` grant that declares actions and carries a single
`token_file` is a config validation refusal, not a runbook caveat. Items 2,
8, and 9 are design-internal rules and were not put to the operator. Each
ruled item carries a *Decided* line.

1. **One `connector_source_id` per profile, or `(id, profile)` pairs.**
   Placing bytes from an action today requires minting a second connector
   source with `profile = file` (section 7.5). *Recommendation:* keep the
   current rule for v1. Two scopes for one integration is clunky but honest,
   the file scope is a genuinely separate durable project that outlives the
   dataset source, and relaxing catalog uniqueness is the kind of invariant
   change that is cheap to make and expensive to unmake. Revisit when a second
   integration hits the same friction, not before.
   *Decided 2026-08-16: keep one id per profile; a dataset source that places bytes uses a separately minted file-profile scope.*
2. **Mapping-vocabulary creep.** The declarative mapping (section 6.2) will be
   asked for conditionals the first time a vendor's shape does not fit.
   *Recommendation:* refuse, and push the computation into producer-side
   normalization, which is already the home of vendor archaeology and already
   deploys independently of the daemon image. Named trigger for revisiting:
   the second vendor that cannot be expressed without one, at which point the
   right answer is probably a narrow, total, non-Turing expression form rather
   than an incremental feature.
3. **The held-association substrate change.** Section 6.3 needs an additive
   `SourceProjectionSnapshot` field and a store version bump.
   *Recommendation:* take it in A3 as designed. The alternatives (refuse the
   batch, or drop the edge) are both worse: refusing makes an ordinary
   multi-cadence dataset unpublishable, and dropping makes the graph quietly
   wrong.
   *Decided 2026-08-16: hold and report; additive snapshot field and store version bump taken in A3.*
4. **Checkpoint epoch home.** The degradation epoch rides telemetry rather
   than the checkpoint value (section 5.2). *Recommendation:* keep it on
   telemetry. A checkpoint value is a vendor token, and corpus bookkeeping
   inside it would make an opaque field partly ours, which is the mistake the
   file lane avoided by keeping `remote_watermark` explicitly non-authoritative.
   *Decided 2026-08-16: telemetry.*
5. **Action latency versus poll interval.** *Recommendation:* bounded
   long-poll on the claims route, and measure before adding anything
   push-shaped. Every push mechanism proposed so far reintroduces a
   corpus-initiated leg or an inbound listener on the producer.
   *Decided 2026-08-16: bounded long-poll on the claims route; measure before anything push-shaped.*
6. **Rotation ordering with actions enabled** (`gap-bb84c77f`, section 9.1).
   *Recommendation:* gate actions behind overlap-tolerant grants where
   rotation is frequent; otherwise name the action-queue drain explicitly in
   the rotation runbook.
   *Decided 2026-08-16: actions require overlap-tolerant grants; declared actions plus a single `token_file` refuse at config validation.*
7. **Multiple source graphs per source.** The wire carries `graph_id`, so one
   dataset source can hold several graphs (for example a slow-moving reference
   graph and a fast-moving transaction graph with different retention).
   *Recommendation:* support it on the wire from v1 (it costs one field) but
   ship one graph per source until a real second graph appears, because
   per-graph checkpoint set partitioning is a question best answered by a case
   rather than by a guess.
   *Decided 2026-08-16: `graph_id` on the wire from v1; one graph per source until a real second graph appears.*
8. **Whether an observation payload should ever be searchable.** Retained
   batches are content addressed and referenced from facts; they are not
   indexed. *Recommendation:* keep them unindexed. The schema's property
   annotations are the sanctioned path into retrieval, and indexing raw vendor
   payloads would put unannotated, unreviewed vendor text into the corpus
   through a side door.
9. **The minimum vendor entity set before an evidence action can avoid broad
   reconciliation.** Deliberately not decided here. The campaign defers it to
   M5 fixtures with a starting hypothesis of the set the action's own lanes
   touch, and this design does not pre-empt that with a guess.

## 13. Worked example: an accounting dataset

Xero is the campaign's first API-dataset connector and is used here as the
concrete shape, **illustratively**. The entity set is deliberately not fixed
by this document: it is decided empirically from M5 fixtures, per the
campaign's own open item, and pre-deciding it here would be exactly the guess
that item exists to avoid.

**Grant.** `connector_kind = "xero"`, `profile = "api_dataset"`,
`connector_source_id` operator-minted and opaque, `remote_authority` naming
the tenant as an operator expectation only.

**Schema artifact.** `schema_id = "xero-accounting"`, namespace `xero`, with
vertex types for the concepts required to locate and explain file evidence:
tenant scope, contacts, invoices and credit notes, bank transactions, tracking
categories and tracking options, accounting attachments, and Files API files
and associations. Edge types bind attachments and file associations back to
their business objects and bind invoices to contacts and tracking options.

**Checkpoint sets**, sketched to show the shape rather than to fix it:

| Set | Entity sets | Delta support | Why |
|---|---|---|---|
| `contacts` | contact | `watermark` | modified-since filter, no deletion feed |
| `transactions` | invoice, credit_note, bank_transaction | `watermark` | same, higher volume |
| `tracking` | tracking_category, tracking_option | `reconcile_only` | tiny set, no delta, full sweep is affordable |
| `files` | file | `watermark` | Files API listing |
| `associations` | file_association | `reconcile_only` | association listing is per-object; retirement needs a sweep |

Two things fall out of that table immediately, which is the point of declaring
it. The `watermark` sets cannot retire entities without a periodic `Full`
pass, so the source must schedule one and the status surface must show when it
last ran. And `associations` refreshing on a different cadence from `files` is
precisely the case that produces held associations (section 6.3), so this
profile's first real dataset exercises that path rather than leaving it
theoretical.

**Fidelity rules**, from the campaign and restated because a schema author
must apply them: remote identifiers stay opaque strings; money uses exact
decimals or minor units and never binary floating point; absent optional
fields stay absent rather than becoming guessed defaults; unknown enum values
are preserved safely; report outputs are query-shaped observations rather than
silently merged canonical entities; raw payload retention follows deployment
policy and is never required for traversal.

**Action.** `xero.files_for_project_code`, declared with a typed input (a
project code, an optional since bound, a maximum file count, and a placement
selector constrained to what the grant permits), `placement_default =
ephemeral`, and limits on remote calls, bytes, results, and wall clock. It
resolves the project code through an explicit tenant mapping, finds relevant
accounting objects, follows both the accounting attachment lane and the Files
API association lane, normalizes both to source vertices and association
edges, deduplicates by remote identity and version, and returns a bounded
evidence bundle with the unresolved cases named rather than dropped.

### 13.1 Slack is not re-homed here

Slack is already served by the **conversation profile**, and this design does
not move it. Its ingest lane is right for its shape: an append-only turn log
with per-channel cursors, immutable records, explicit revisions and
tombstones, and message-granular identity.

What is worth noticing is that the two halves of this design are separable.
The **ingest lane** is profile-specific; the **projection half** is not. A
later Slack graph projection (channels, threads, authors as a source graph)
can consume landed conversation records through the same
`SourceProjectionStore`, the same schema artifact mechanism, and the same
status shape, without changing the conversation wire and without a re-ingest.
That is exactly what the Slack design promises when it says v1 owes the later
projection field completeness rather than a lane change, and it is the
cleanest available proof that this design's projection contract is not
secretly an API-dataset contract wearing a general name.

## 14. Relationship

- **Owned by**
  [Graph-native connector campaign](reflective-graph-connector-program.md):
  this is the profile design its milestone M4 names, resolving the connector
  class limitation tracked by `gap-0378c305`. The campaign owns the invariants,
  the authority planes, the release sequence, and the Xero and evidence-action
  milestones; this document owns the profile's wire, projection, action, and
  status contracts.
- **Companion of**
  [Remote Source Connectors](remote-source-connectors.md): the file-tree
  profile and owner of the shared transport, onboarding, connector scope
  identity, and producer-satellite template. This design adopts all of it,
  adds a third lane beside it, and sends `project_owned` action bytes back
  through its lane rather than duplicating a blob transport.
- **Companion of**
  [Slack Ingestion Connector](slack-ingestion-connector.md): the conversation
  profile. Its held-revision, server-owned-cursor, and lag-as-headline
  patterns are cited directly here; its shape argument (why an append-only log
  is not a file tree) is the template for section 2's argument that an entity
  collection is neither.
- **Builds on** the M2 source-projection substrate `bbox-source-graph` and the
  reflective graph kernel `bbox-project-graph`: `SourceProjectionStore`,
  `GraphDelta`, `NamedCheckpointSet`, `ReconciliationMode`, content-addressed
  observation retention, `GraphSchema` with per-property retrieval
  annotations, and the `project_graph_vertex` reference family. This design
  proposes exactly one additive substrate change (held associations,
  section 6.3) and consumes the rest unchanged.
- **Depends on**
  [Secret custody across the checkout and corpus planes](../operations/config-artifacts/secrets-provider.md)
  for the satellite's vendor credentials, its writable token store, and the
  producer-grant custody story; `gap-bb84c77f` (overlap-tolerant grant
  rotation) interacts with the action lane as recorded in section 9.1.
- **Constrained by**
  [Locality-first decomposition](../daemon-runtime/locality-first-decomposition.md)
  and
  [Remote project onboarding](../daemon-runtime/remote-project-onboarding.md):
  every network initiation is producer-side, onboarding is two-sided operator
  config, and no agent tool creates a source or triggers acquisition.
- **Feeds** the campaign's evidence-binding and unified-retrieval milestones
  (`gap-616857f8`, `gap-5d57d2bb`): source graph vertices are generic
  endpoints, and schema property annotations are authored for a retrieval
  milestone this design does not itself deliver.
