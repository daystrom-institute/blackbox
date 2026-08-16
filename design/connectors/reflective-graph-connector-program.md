---
title: "Graph-native connector campaign"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - connectors
  - corpus
  - agentic-corpus
  - operations
tags:
  - implementation-strategy
  - reflective-graph
  - connectors
  - xero
  - slack
  - evidence
  - custody
  - locality
brief: "The connecting delivery program for a reflective graph kernel, graph-native connector projections, a versioned Xero source schema, Files API evidence actions, tenant-owned record graphs, and unified retrieval, re-grounded on the locality architecture."
date: 2026-08-11
---

# Graph-native connector campaign

Status: partial; grounded 2026-08-11 on the post-locality architecture,
status refreshed 2026-08-16. Landed on `beta/blackbox-v2`:

- **M1 (2026-08-12)**: the reflective graph kernel,
  `crates/bbox-project-graph` - descriptors, schema-as-data validation,
  atomic generations, the `project_graph_vertex` ref family,
  `bbox_project_graph_list/describe/validate`, provisional visibility -
  proven live by `examples/graph-live-exercise.sh` and a real external
  authoring exercise (findings on thread-a2062843).
- **M2-M3 (2026-08-13)**: source projections in
  `crates/bbox-source-graph` (dedicated connector source-projection
  store, atomic snapshot acceptance, `SourceProjectionStatus`) and
  cross-entity evidence bindings in
  `crates/bbox-project-graph/src/evidence.rs` with the
  `.bbox/evidence/bindings.json` checkout lane.
- **Connector identity (phase 0, 2026-08-12/13)**: operator-minted
  `connector_source_id` grants in `crates/bbox-config` and
  `ProjectScope::Connector(ConnectorScope)` in
  `crates/bbox-corpus-core`.
- **File-source transport (phase 1, 2026-08-13)**: `bbox-file-source`
  wire crate, `bbox-file-collector` satellite, `bbox-file-source-store`
  generation store.
- **Slack conversation lane (M5b corpus lane, 2026-08-13)**:
  `bbox-conversation-source`, `bbox-conversation-source-store`,
  `bbox-slack-collector`, projected into the word index through the
  transcript adapter (schema `agentic-corpus-g12-conversation-projection`).

M4's API-dataset profile is designed, not landed; its contracts are
owned by [API-Dataset Connector](api-dataset-connector.md)
(`gap-0378c305`). M9 unified retrieval is designed and under active
implementation by a sibling lane
([Unified retrieval for reflective graph vertices](unified-retrieval.md),
`gap-5d57d2bb`); treat neither as shipped. The pre-locality
`campaign/reflective-graph-r2-projection` implementation is history: it
was ported milestone by milestone against current contracts, never
merged wholesale. Reverify contract names against code before building.

## 0. Outcome

Deliver one coherent path from remote API observation to project-native
evidence:

- pull the reflective project graph forward as a small, generic semantic
  substrate;
- let connectors project remote systems into versioned source-owned graphs;
- keep tenant-authored record graphs separate from connector-owned source
  graphs;
- bind records to source facts, remote files, and placed bytes with
  explicit evidence edges;
- make Xero the first worked API-dataset connector profile, with its Files
  API as the premier action, and Slack message ingestion the first
  transcript-shaped profile riding the same runtime;
- expose graph vertices and evidence relationships through ordinary Blackbox
  inspection, traversal, and retrieval;
- harden hosted credential, identity, custody, and witness boundaries before
  production enablement.

The outcome is not "store Xero JSON in files," and it is not "make every
connector a graph database." The graph owns normalized semantic projection.
The connector runtime continues to own remote observation, checkpoints,
actions, byte transfer, placement, and operational evidence - and it does all
of that on the producer plane, never inside the corpus daemon.

## 1. Why this is one campaign

Four designs currently meet at the same boundary:

1. Remote-source connectors define how Blackbox observes remote document
   stores and publishes their content to the corpus host, but their profile
   is a file tree.
2. The reflective project graph defines project-owned schema and facts, but
   its proposed first release deliberately omits cross-entity evidence and
   unified retrieval.
3. The first API-dataset use case (Xero) needs both a typed business-system
   projection and a targeted Files API action that can return evidence
   without importing the whole remote system.
4. The Slack ingestion connector needs conversation-shaped corpus ingestion
   now and graph projection of channels, threads, and authors later.

Building the API connector first would force its domain objects into either
project files or connector-specific core types. Building the graph in complete
isolation would prove structural reflection without testing the authority,
versioning, evidence, and retrieval contracts required by a real source.

The sequencing decision is therefore unchanged from the original program:

> Pull the reflective graph kernel forward first, then land a generic
> graph-projection connector profile, then make Xero the first versioned source
> schema and Files action on that profile.

What changed since the original writing is the substrate underneath: the
locality program decomposed the monolith, moved the daemon into the cluster
with zero checkout authority, and established the producer/collector transport
as the only way bytes enter the corpus. Every milestone below now assumes that
substrate; the pre-locality implementation of M1-M3 was ported onto it
milestone by milestone, and the milestone notes retain its salvage-era
wording as history.

## 2. Program invariants

1. Blackbox core gains no Xero-specific (or Slack-specific) enum variants,
   entity variants, or traversal branches.
2. A connector source graph and a tenant record graph are distinct authority
   planes, even when they describe the same real-world subject.
3. Connector-owned source facts are replaceable projections of remote state.
   Tenant-authored record facts are not overwritten by connector refresh.
4. Cross-plane relationships are explicit evidence edges, not identifier
   coincidence or property-name convention.
5. The reflective fixed floor stays small:
   `meta:VertexType`, `meta:EdgeType`, `meta:INSTANCE_OF`,
   `meta:FROM_TYPE`, and `meta:TO_TYPE`.
6. Project and connector schemas are data validated by the same graph kernel.
   They are not compiled into Blackbox.
7. Remote observation and graph projection are separate contracts. A raw
   response can be reprojected when schema or projection logic changes.
8. Checkpoints advance only with an atomic accepted observation batch.
9. Normalized graph state is not an audit log. Signed or hash-chained witness
   records remain a separate append-only plane.
10. Connector actions can be useful without full replication. An action may
    resolve, fetch, and place a bounded evidence set.
11. Placement policy controls whether bytes are ephemeral, project-owned,
    connector-cache-owned, or external-only.
12. Secrets are references across connector configuration and status surfaces.
    Credential material never enters graph facts, manifests, logs, or witness
    payloads.
13. Search results preserve source graph, remote identity, observation time,
    projection version, and placement provenance.
14. Hosted enablement fails closed when required identity or secrets providers
    are unavailable.
15. Public artifacts use only generic scenarios and public vendor vocabulary.
16. Connectors observe and publish from the producer plane. The corpus daemon
    never fetches remote bytes, never holds connector credentials, and never
    materializes remote content into its own filesystem. This inherits the
    locality split axis (`design/daemon-runtime/locality-first-decomposition.md`)
    and the onboarding trust model
    (`design/daemon-runtime/remote-project-onboarding.md`): no agent
    self-service source enrollment, no MCP-triggered fetch or sync.

## 3. Authority and data planes

### 3.1 Source graph

A source graph is a connector-managed, rebuildable projection of remote state.
For Xero it can contain entities such as contacts, invoices, tracking
categories, tracking options, files, and file associations. For Slack it can
later contain channels, threads, and authors.

The connector owns:

- source identity and remote identifiers;
- observation and checkpoint metadata;
- source schema and projection version;
- refresh, deletion, and reconciliation behavior;
- source-specific fidelity rules.

Users can inspect and relate source vertices, but do not edit the connector's
authoritative projection in place.

### 3.2 Record graph

A record graph is tenant-authored project state. It can describe the domain in
the tenant's own language, for example projects, filings, work packages,
approvals, or evidence requirements.

The tenant owns:

- schema vocabulary and lifecycle;
- facts and record identifiers;
- business meaning;
- mappings to source systems;
- retention and export decisions.

The record graph can exist without a connector, and it survives connector
replacement or source reprojection.

Record graphs are checkout-plane state: committed files under the project's
`.bbox/` tree, reaching the corpus through the same transport lane as other
repo-owned knowledge, with the `published | own | all` provisional-visibility
semantics the knowledge lane already carries
(`design/corpus/knowledge/checkout-identity-and-provisional-knowledge.md`).
Graph facts must not become a weaker back door around publication authority.

### 3.3 Evidence bindings

Evidence edges bind authority planes without collapsing them:

```text
record:Project
  -> record:CORRESPONDS_TO
  -> xero:TrackingOption

record:Filing
  -> record:EVIDENCED_BY
  -> xero:File

xero:File
  -> bbox:PLACED_AS
  -> project_file
```

Edges carry enough provenance to answer who asserted the relation, from which
observation or operator action, under which mapping version, and whether it is
still current.

### 3.4 Witness plane

The witness plane records operational truth:

- remote requests and bounded response digests;
- checkpoint transitions;
- projection batches and schema versions;
- action invocation and result summaries;
- byte-placement decisions;
- refresh, deletion, and reconciliation outcomes.

It is append-only and tamper-evident where the deployment requires audit
assurance. It is not reconstructed from the current graph and is not modeled as
ordinary mutable graph facts. Witness emission happens on both planes: the
producer records what it asked the remote system and what it shipped; the
corpus host records what it accepted, projected, and activated.

## 4. Core contracts

The campaign introduces contracts in layers rather than one oversized
connector trait.

### 4.1 Connector observation (producer plane)

`ConnectorSource` discovers remote changes and returns bounded observations:

```text
ConnectorSource
  observe(checkpoint_set, scope, limits) -> ObservationBatch
  fetch_content(content_ref, limits) -> ContentStream
  invoke(action, input, limits) -> ActionObservation
```

An `ObservationBatch` contains stable source identities, remote versions,
deletion markers, typed payloads or payload references, observation metadata,
and a proposed checkpoint transition.

Checkpoints form a named set rather than one opaque cursor. This supports APIs
whose entities, files, associations, and reports have different refresh
semantics.

Observation runs in a connector satellite on a producer host. Accepted
observation batches and their payloads travel to the corpus host over the
connector's transport lane (the collector-style manifest wire for file bytes,
a typed observation endpoint for dataset payloads), authenticated by a
scope-bound producer token. The remote system's credentials never leave the
producer.

### 4.2 Graph projection (corpus plane)

`GraphProjection` turns accepted observations into an atomic `GraphDelta`:

```text
GraphProjection
  schema_descriptor() -> GraphSchemaDescriptor
  project(observation_batch, prior_generation) -> GraphDelta
```

The delta declares:

- inserted, replaced, and removed vertices;
- inserted and removed edges;
- projection version;
- source observation references;
- resulting graph generation;
- reconciliation mode.

Projection is deterministic for the same accepted observation, schema, and
projection version. A projection can be replayed without contacting the remote
system, which is why projection lives corpus-side: the corpus retains accepted
observations (by deployment policy) and can reproject on schema or logic
change without asking any satellite to re-observe, mirroring the collector's
content-addressed blob cache.

### 4.3 Graph descriptor and identity

Every graph has a descriptor:

```text
scope
graph_id
authority
schema_id
schema_version
projection_version
source_connector
retention_policy
generation
```

The graph kernel gains one generic entity-reference family:

```text
project_graph_vertex:<scope-id>:<graph-id>:<vertex-id>
```

Domain-specific reference variants are forbidden. Existing Blackbox entity
references and project-graph vertices meet through the generic edge layer.

Connector sources need durable catalog identity. The project catalog's
`PublishedScope` derives from a committed `.bbox/config.toml` in a git
checkout; a Xero tenant or Slack workspace has neither. Scope minting for
non-git sources is a shared open question owned by the remote-source
connector design (`remote-source-connectors.md`); this program consumes
whatever durable connector-scope family that design lands and forbids path-
or host-derived identity in the interim.

### 4.4 Placement

`PlacementPolicy` decides what happens to remote bytes:

- `ephemeral`: return a bounded handle or stream and retain no durable copy;
- `project_owned`: publish into an owned project scope through the connector
  transport and index normally;
- `connector_cache`: retain under connector lifecycle and retention controls
  (producer-side working state or corpus-side observation retention, named
  explicitly per source class);
- `external_only`: retain remote reference and metadata only.

Placement is selected per action or source class. It is not implied by graph
membership. `project_owned` placement never means "the daemon writes files":
it means the bytes enter the corpus as published, content-addressed source
content under a connector scope, exactly like collected code bytes do.

### 4.5 Connector actions

Actions are declared capabilities with typed inputs, output descriptors,
limits, and placement choices. They cover targeted workflows that do not map
cleanly to whole-source synchronization.

The first action is:

```text
xero.files_for_project_code
```

It resolves a tenant mapping, discovers relevant business objects, follows
both accounting attachment and Files API association lanes, deduplicates
remote files, and returns or places a bounded evidence bundle.

Actions are invoked through the daemon (which owns authorization, limits, and
witness acceptance) but execute their remote legs on the producer satellite
that holds the credentials. An action invocation is a corpus-to-producer
request riding the same authenticated channel discipline as dispatch, never a
daemon-side HTTP call to the vendor.

## 5. Dependency graph

```text
M0 authority, custody, and version contracts
                       |
                       v
M1 reflective graph kernel
                       |
          +------------+-------------+
          v                          v
M2 source-managed projections   M3 evidence endpoints
          |                          |
          +------------+-------------+
                       v
M4 graph-aware connector runtime
                       |
          +------------+-------------+
          v                          v
M5 Xero source schema           M5b Slack ingestion
   and projection                    (corpus lane first)
          |                          |
          v                          |
M6 Xero Files evidence action        |
          |                          |
          +------------+-------------+
          v                          v
M7 durable books overlay       M8 tenant record mappings
          |                          |
          +------------+-------------+
                       v
M9 unified retrieval and evidence bundles
                       |
                       v
M10 hosted identity, secrets, and witness hardening
```

The kernel precedes the connector projection, but M1 is intentionally narrow.
The Xero profile then pressures the generic contracts before broad connector
catalog work begins. Slack ingestion (M5b) is deliberately decoupled: its v1
is corpus ingestion without graph projection
(`slack-ingestion-connector.md`), so it can land as soon as the connector
runtime's transport and identity pieces exist, and it adopts graph projection
later as a second worked example.

## 6. Milestone 0: contracts and custody matrix

Status: accepted. The 2026-08-13 operator ratification settled the open
M0 decision points (retained observation, schema shipping, index
eligibility, scope identity); see the decision ledger.

Deliverables:

- authority table for schema, facts, edges, checkpoints, bytes, and witness
  records, split by plane (producer vs corpus) for every row;
- graph descriptor and version compatibility rules;
- source identity and deletion semantics, including the non-git scope
  contract consumed from the remote-source design;
- checkpoint-set transaction rules;
- placement and retention matrix;
- action capability descriptor;
- failure taxonomy for auth, rate limit, partial observation, projection,
  placement, and checkpoint commit;
- public-safe Xero fixtures covering entity, association, file, and deletion
  cases.

Exit gate: a design-level trace can follow one remote file from API
observation on the producer, through source vertex and evidence edge, to
either an ephemeral result or a published project-owned file, with the
authority, plane, and retention owner named at every step.

## 7. Milestone 1: reflective graph kernel

Status: LANDED 2026-08-12 in `crates/bbox-project-graph`.

Implement the smallest accepted slice of the reflective project graph:

- graph descriptors and schema documents;
- the fixed meta-schema floor;
- project-owned vertex and edge types;
- graph validation;
- atomic graph generation updates;
- exact list, describe, inspect, and traversal;
- one generic project-graph vertex reference family;
- explicit inclusion policy for local scratch graphs.

Keep lifecycle systems, epistemic frameworks, graph query languages, automatic
knowledge promotion, and full-text indexing out of this milestone.

History note: the kernel was first implemented on the pre-locality
salvage branch as `crates/bbox-project-graph` (model, validation, store)
with daemon wiring, a `project_graph_vertex` ref family in the
entity-ref grammar, entity-provider integration, and
`bbox_project_graph_list/describe/validate` tools. The port re-cut the
wiring against the current crate tree (the entity-ref and provider
surfaces moved during the locality decomposition) and placed graph-fact
reads behind the same published-vs-provisional visibility the knowledge
lane uses.

Exit gate: two unrelated schemas can be loaded, validated, inspected, and
traversed with no new Rust domain variants.

## 8. Milestone 2: source-managed graph projections

Status: LANDED 2026-08-13 in `crates/bbox-source-graph`.

Add connector authority without weakening project ownership:

- connector-owned graph descriptor mode;
- schema and projection version tracking;
- deterministic observation-to-delta projection;
- atomic delta plus checkpoint commit;
- deletion, replacement, and full-reconciliation semantics;
- reprojection from retained observations or deterministic fixtures;
- operator-visible generation and freshness status.

Connector refresh cannot edit tenant-authored record graphs. Source graph
replacement cannot silently delete tenant evidence edges; such edges become
stale or unresolved and remain diagnosable.

Exit gate: a synthetic API-dataset connector advances a source graph through
create, update, delete, checkpoint resume, and schema reprojection.

Accepted implementation contract (first proven on the pre-locality
branch, landed in `bbox-source-graph`):

- connector-managed generations live in a dedicated source projection store,
  not under either project-authored graph root;
- one atomic snapshot accepts the descriptor, schema, normalized graph facts,
  source observation references, and named checkpoint transition together;
- generations advance by exactly one, and only an exact replay of the most
  recently accepted batch is idempotent;
- rejected graph validation, checkpoint conflict, schema rollback, or snapshot
  integrity failure leaves the accepted generation and checkpoint set
  unchanged;
- status exposes generation, schema and projection versions, graph
  fingerprint, latest observation time, reconciliation mode, and named
  checkpoints without credential material.

This mirrors the code-source activation discipline: stage, validate, flip
atomically, retain the prior generation for diagnosis.

## 9. Milestone 3: cross-graph and cross-entity evidence

Status: LANDED 2026-08-13 in
`crates/bbox-project-graph/src/evidence.rs`, with the
`.bbox/evidence/bindings.json` checkout lane.

Resolve the evidence endpoint limitation tracked by `gap-616857f8`:

- graph vertices can be endpoints of generic Blackbox edges;
- edges can cross source and record graphs within an authorized scope;
- project files and other existing Blackbox entities can be evidence
  endpoints;
- edge provenance records assertion authority, observation, mapping version,
  and freshness;
- traversal reports missing, stale, and unauthorized endpoints explicitly.

This milestone does not make all Blackbox entities editable graph vertices. It
only gives the graph and evidence layers a stable generic reference boundary.

Exit gate: a tenant record vertex traverses through a source vertex to a
published project file, and the reverse traversal preserves provenance.

Accepted implementation contract (landed in
`crates/bbox-project-graph/src/evidence.rs`):

- tenant-owned bindings live in `.bbox/evidence/bindings.json`, outside both
  project-authored graph facts and connector-managed source snapshots; as
  committed checkout-plane state they reach the corpus through the repo-owned
  state lane with `built_from` stamps and provisional visibility;
- bindings use canonical `EntityRef` endpoints and generic edge kinds, with no
  connector-specific entity variants;
- each binding records assertion authority, asserted time, observation or
  mapping provenance, and optional endpoint generations;
- a complete valid document replaces one scope's accepted binding set; an
  invalid candidate leaves the prior accepted set intact;
- endpoint status is resolved at traversal and bundle time as current, stale,
  missing, unauthorized, or unresolved;
- inspection retains non-current bindings for diagnosis, while path traversal
  never crosses an unauthorized endpoint;
- connector reprojection or deletion changes freshness status but cannot
  delete tenant-owned bindings.

One deviation from that contract was taken deliberately during implementation
and is now the settled shape. Runtime endpoints are canonical `EntityRef`s
with generic edge kinds, exactly as specified. The COMMITTED authoring form
stops one step short of canonical for project-scoped endpoints: a canonical
`project_file` or `project_graph_vertex` ref embeds a `project_id`, and
`project_id` is assigned by whichever host registered the checkout, so baking
one into a committed repo file makes that file wrong on every other host that
clones it. A project-scoped endpoint therefore names only what the repo owns
(graph id, vertex id, path and chunk hashes) and the loader materializes the
canonical ref with the project id supplied by the lane the document arrived
on. Endpoints that carry no project scope are authored as canonical ref
strings directly, through a literal-ref form that refuses project-scoped types
for this reason. Campaign invariant 1 is unaffected: no connector-specific
entity variant is introduced, and the endpoint types are the existing
canonical ones.

Two further notes on the implemented shape:

- the evidence lane rides the same repo-owned state transport as
  `.bbox/knowledge` and `.bbox/graphs`, but carries no `built_from` stamp of
  its own. Graph-shaped lanes are identified by generation identity rather
  than by `BuiltFromStamp`, and the stamp variant set is frozen by an
  acceptance scan; evidence follows the graph precedent;
- adding a lane moves the working-pair capture commitment and both generation
  identities, so the single pre-graphs legacy special case became an explicit
  append-only vintage ladder. Each rung is the exact preimage some shipped
  binary used, and an older rung stays admissible only for the absent-lane
  shape a binary of that vintage could have produced.

## 10. Milestone 4: graph-aware connector runtime

Status: partial. The shared transport and identity layers are landed for
the file-tree and conversation profiles (`bbox-file-source`,
`bbox-file-collector`, `bbox-file-source-store`,
`bbox-conversation-source`, `bbox-conversation-source-store`,
`bbox-slack-collector`). The API-dataset profile is designed, not
landed; [API-Dataset Connector](api-dataset-connector.md) owns its
contracts.

Generalize the connector shape into composable profiles on the producer/corpus
split, resolving the connector-class limitation tracked by `gap-0378c305`:

- observation and checkpoint-set runtime (producer satellite);
- typed observation acceptance endpoint and retention (corpus);
- graph projection hook (corpus);
- content fetch and placement hook (producer fetch, corpus placement
  acceptance);
- declared actions with corpus-side authorization and producer-side
  execution;
- bounded retries, rate limits, and degradation status;
- witness emission on both planes;
- source graph refresh scheduling (producer cadence, corpus acceptance);
- shared onboarding shape with the file-tree profile: two-sided operator
  config, producer grant, find-or-create idempotent transport, no agent
  self-service.

The remote-source design remains the file-tree profile and owns the transport
the profiles share. API-dataset connectors add semantic projection and action
surfaces without making file-tree connectors synthesize business graphs, and
the Slack profile adds conversation-corpus ingestion without either.

The API-dataset profile's own contracts (its wire lane, schema-directed
projection, action surface, grant discriminant, and status shape) are owned by
[API-Dataset Connector](api-dataset-connector.md), which is the profile design
this milestone names.

Exit gate: one file-tree fixture and one API-dataset fixture run through the
same orchestration, checkpoint, status, secret-reference, and witness
boundaries.

## 11. Milestone 5: Xero source schema and projection

Define a versioned Xero schema package as graph data. The first slice should
cover the concepts required to locate and explain file evidence:

- tenant and organization scope;
- contacts;
- invoices and credit notes;
- bank transactions;
- tracking categories and tracking options;
- accounting attachments;
- Files API files, folders where relevant, and associations;
- target-specific association edges back to business objects;
- remote URLs, content types, sizes, timestamps, and source versions.

Projection fidelity rules:

- remote identifiers remain opaque strings;
- money uses exact decimals or minor units, never binary floating point;
- absent optional fields remain absent rather than becoming guessed defaults;
- unknown enum values are preserved safely;
- report outputs are query-shaped observations, not silently merged canonical
  entities;
- endpoints without reliable delta support use explicit bounded
  reconciliation;
- raw payload retention follows deployment policy and is not required for
  graph traversal.

Exit gate: recorded public-safe fixtures project deterministically, preserve
unknown fields and enum values according to policy, reconcile deletion, and
survive a schema-version replay.

### Milestone 5b: Slack ingestion (corpus lane)

Status: LANDED 2026-08-13 (corpus lane):
`bbox-conversation-source`, `bbox-conversation-source-store`,
`bbox-slack-collector`, projected into the word index through the
transcript adapter.

The Slack connector's v1 ships message ingestion into the conversation corpus
without graph projection; its design, producer shape, cursors, and privacy
posture live in `slack-ingestion-connector.md`. Within this program it serves
as the transcript-shaped proof that the connector runtime's transport,
identity, and policy layers are not file-tree-specific. Its later graph
projection (channels, threads, authors as a source graph) is a post-M9
adoption, not a v1 requirement.

## 12. Milestone 6: Xero Files evidence action

Implement `xero.files_for_project_code` as the first end-to-end action:

1. Resolve the project code through an explicit tenant mapping, initially a
   configured tracking category and option.
2. Find relevant accounting objects through supported queries or bounded
   reconciliation.
3. Follow the Accounting API attachment lane.
4. Follow the Files API association lane.
5. Normalize both lanes to Xero source vertices and association edges.
6. Deduplicate files by remote identity and version.
7. Apply caller-selected placement and byte limits.
8. Return an evidence bundle with files, source objects, association paths,
   observation metadata, and unresolved cases.

Ephemeral placement is the default for the first action. Durable project
placement is explicit.

Exit gate: fixtures prove attachment-only, association-only, both-lane
duplicate, missing mapping, partial authorization, pagination, rate-limit
resume, and bounded-download behavior.

## 13. Milestone 7: durable books overlay

Once the targeted action is sound, enable a durable Xero source graph for a
configured accounting scope:

- scheduled incremental or reconciled refresh;
- persisted checkpoint sets;
- graph generation and freshness status;
- tombstone and stale-edge behavior;
- optional metadata-only or selected-content placement;
- complete removal and retention behavior.

This is an overlay on the connector runtime, not a requirement for the Files
action. The action-first release produces user value while the durable sync
surface remains deliberately narrow. A durable overlay is a continuous
publisher: it inherits the corpus host's per-cycle activation and index-churn
cost profile, and its cadence and caps must be justified against that cost,
not assumed free.

Exit gate: daemon restart, expired cursor, schema reprojection, remote deletion,
and source removal all have tested and operator-visible outcomes.

## 14. Milestone 8: tenant record schema and mappings

Define the tenant-owned overlay separately from the Xero schema:

- record types and lifecycle vocabulary stay project-owned;
- mapping rules relate record keys to Xero tracking options or other source
  identifiers;
- mappings have versions and assertion authority;
- connector refresh never rewrites record facts;
- ambiguous or missing mappings remain explicit;
- source replacement can rebind evidence without migrating record identity.

The worked example should demonstrate a record-level project and filing model,
but that vocabulary must remain fixture-local rather than Blackbox canon.

Exit gate: a record graph survives a complete Xero source graph rebuild and
retains either valid, stale, or unresolved evidence bindings without fact loss.

## 15. Milestone 9: unified retrieval and evidence bundles

Status: designed, under active implementation by a sibling lane
(`gap-5d57d2bb`); treat as proposed, not landed. Do not document that
lane's in-flight code as shipped behavior.

Owned in detail by [Unified retrieval for reflective graph vertices](unified-retrieval.md),
which resolves the design frame for `gap-5d57d2bb`: meaning-bearing vertex
kinds and the per-graph policy extension, the indexing seam and its
interaction with graph generations, the query path and its
before-ranking authority filter, the result shape, the tool-surface
deltas, and a five-slice phasing with exit gates. This section states the
milestone; that document states the contracts.

Resolve graph retrieval limitations tracked by `gap-5d57d2bb`:

- reflective graph vertices participate in exact and text retrieval according
  to graph policy;
- source and record results retain graph and authority identity;
- evidence-path expansion can accompany search results;
- placed file chunks link back to remote source vertices;
- bounded evidence bundles expose graph paths, files, and provenance without
  dumping whole source graphs into model context;
- authorization filters apply before ranking and traversal expansion.

Vector indexing is optional and schema-directed. The graph kernel must not
assume every property is useful or safe to embed.

Exit gate: a record query can find the appropriate project, expand to its Xero
objects and files, and return bounded cited content with provenance intact.

## 16. Milestone 10: hosted identity, secrets, and witness hardening

Production hosted deployment binds this program to the estate's deployed
identity and secrets plane through
[Secret custody across the checkout and corpus planes](../operations/config-artifacts/secrets-provider.md),
which is a sibling design of this campaign (the original program's secrets
prerequisite never landed and was modernized alongside it):

- identity-plane OIDC identities (the estate runs Keycloak) establish
  workload identity and, where Blackbox exposes multi-principal hosted
  surfaces, human identity, tenant membership, and role claims at the service
  boundary;
- the in-cluster secrets plane (the estate runs OpenBao behind External
  Secrets Operator, rooted in an operator vault) holds connector client
  credentials, rotating refresh tokens, signing material, and connector
  secret versions; producer satellites hold their own vendor credentials as
  file-sourced references and never forward them;
- services exchange identity-plane JWTs for policy-scoped secrets-plane
  credentials rather than shipping static tokens;
- graph, action, and placement authorization is derived from verified
  identity and tenant scope;
- secret leases, rotation, revocation, and unavailable-provider behavior are
  tested;
- connector records contain secret references only;
- witness records are signed or hash-chained according to deployment policy;
- audit queries correlate actor, tenant, connector, action, graph generation,
  placement, and result without exposing secret or file content.

Local development may use the local identity and secrets profiles. Hosted
Xero enablement fails closed until the identity and secrets integration
contracts are implemented and exercised. This is a delivery gate, not a claim
that the Blackbox side of the integration already exists.

Exit gate: cross-tenant denial, token rotation, lease expiry, identity-provider
outage, secrets-provider outage, connector revocation, and audit verification
all pass integration tests.

## 17. Release increments

| Release | Useful capability | Included milestones | Status |
|---|---|---|---|
| R0 | Accepted contracts and fixtures | M0 | Accepted |
| R1 | Project-defined reflective graphs | M1 | LANDED |
| R2 | Rebuildable source graphs and generic evidence endpoints | M2-M3 | LANDED |
| R3 | File-tree and API-dataset profiles on one runtime | M4 | Partial (file-tree and conversation transport landed; API-dataset proposed) |
| R3b | Slack messages corpus-searchable | M5b | LANDED (corpus lane) |
| R4 | Inspectable Xero semantic projection from fixtures | M5 | Proposed |
| R5 | Project-code to bounded Xero file evidence | M6 | Proposed |
| R6 | Durable Xero books overlay | M7 | Proposed |
| R7 | Tenant record overlay with explicit mappings | M8 | Proposed |
| R8 | Unified graph, evidence, and content retrieval | M9 | In progress (design landed, implementation in flight) |
| R9 | Hosted, auditable Xero operation | M10 | Proposed |

R5 is the first product-shaped release for the Xero lane; R3b is the first
product-shaped release overall (searchable Slack history has standalone
value). R1 through R4 are independently useful substrate releases and must
not be hidden behind the completion of hosted auth.

## 18. Verification strategy

Use a layered test matrix:

- graph contract tests for schema reflection, validation, identity, and atomic
  generations;
- projection golden tests for deterministic deltas and version replay;
- connector contract suites shared by file-tree, API-dataset, and
  conversation profiles;
- recorded, public-safe Xero fixtures for pagination, optional fields, unknown
  values, associations, attachments, and rate limits;
- property tests for checkpoint monotonicity and graph-delta idempotence;
- failure injection between observation, transport, projection, placement,
  witness, and checkpoint commit;
- authorization tests for graph scope, action scope, byte access, and
  cross-tenant denial;
- restart tests around in-flight actions and refresh, on both planes;
- retrieval tests that verify evidence paths and provenance, not only result
  text;
- live vendor smoke tests kept outside committed fixtures and scrubbed of
  customer identity.

No live tenant data belongs in the repository, test snapshots, design examples,
or durable knowledge artifacts.

## 19. Parallelism and sequencing

Safe parallel work after M0:

- graph storage and validation can proceed alongside connector observation and
  checkpoint contracts;
- Xero fixture/schema design can proceed alongside the generic projection
  implementation;
- Slack ingestion (M5b) can proceed as soon as M4's transport and identity
  pieces exist, independent of the Xero lane;
- placement and witness mechanisms can proceed alongside evidence endpoints;
- identity/secrets integration probes can begin early, but they do not define
  graph semantics.

Hard ordering constraints:

- source projections depend on stable graph generations and descriptors;
- Xero implementation depends on the generic API-dataset profile;
- tenant mappings depend on stable cross-graph evidence edges;
- unified retrieval depends on stable generic graph references;
- hosted enablement depends on identity, secrets, authorization, and witness
  gates, even if local functionality ships earlier.

## 20. Decision ledger

Decided by this campaign (original program, still standing):

- Pull the reflective graph kernel forward before the generalized API
  connector.
- Keep the first graph release narrow, then add source authority and evidence
  endpoints as explicit follow-on milestones.
- Model Xero as a versioned source schema and projection, not core code types.
- Keep source graphs and tenant record graphs separate.
- Treat graph projection as normalized current state, not operational witness.
- Make targeted connector actions first-class and ship the Files evidence
  action before requiring a durable whole-books overlay.
- Require explicit placement policy for remote bytes.
- Commit source graph state and named checkpoint transitions in one atomic
  source-projection snapshot.
- Store cross-plane evidence as a tenant-owned binding overlay, separate from
  both graph authority planes.

Added by the 2026-08-11 re-grounding:

- Connectors observe and publish from the producer plane; the daemon accepts,
  projects, and activates. This replaces the original program's daemon-side
  connector runtime and follows the locality split rather than amending it.
- The salvage-branch implementation of M1-M3 was donor code, ported
  milestone by milestone against current contracts and never merged
  wholesale; that port completed on 2026-08-13.
- Record graphs and evidence bindings are checkout-plane committed state with
  publication visibility semantics, not daemon-local files.
- The Slack ingestion connector joins the program as the transcript-shaped
  profile (M5b), corpus lane first, graph projection later.
- Hosted identity and secrets bind to the estate's deployed Keycloak/OpenBao
  plane through the modernized secrets-provider design rather than a
  hypothetical future seam.

Decided 2026-08-13 (operator ratification of the standing recommendations;
knowledge decisions referenced):

- Retained-observation policy (98d9f430f62ad8ca): accepted observation
  batches are retained content-addressed, defaulting to current plus prior
  generation and everything younger than a retention window, per source
  class and widenable by deployment policy; reprojection beyond the horizon
  degrades honestly to re-observe.
- Source schema shipping (7650b743fb23c265): versioned corpus artifacts
  through the existing artifact catalog, not compiled into connector
  binaries; signed distribution deferred to M10-era hardening.
- Index eligibility (b1a11d7cf59f2545): conservative and schema-directed;
  labels word-indexed by default, per-property annotations for text and
  embedding participation, embeddings strictly per-kind opt-in under
  per-graph policy. Schema authors annotate as they write.
- Durable scope identity for non-git sources: resolved 2026-08-12
  (operator-minted `connector_source_id`, coordinates as observations);
  the catalog implementation landed as `ProjectScope::Connector` in
  `crates/bbox-corpus-core` with grants in `crates/bbox-config`
  (`gap-0c7ec76c`).

Still open (backburner, re-evaluate at decision time):

- the minimum Xero entity set needed before the Files action can avoid
  broad reconciliation: decide empirically from M5 fixtures; starting
  hypothesis is the set the action's two lanes touch (contacts, invoices,
  tracking categories and options, attachments, files, associations);
- tenant mapping approval (decide at M8): standing recommendation is v1
  requires operator approval for every mapping change (low volume,
  authority-bearing), relaxing later to auto-accepted unambiguous
  exact-key matches with audit once volume demands.

## 21. Relationship

- Continues: the original graph-native connector campaign authored on the
  pre-locality branch `campaign/reflective-graph-r2-projection`, which
  carried the pre-locality implementation of M1-M3, since ported onto
  this campaign.
- Extends: [Reflective Project Graph](../corpus/agentic-corpus/reflective-project-graph.md),
  which owns the project-defined schema and fact model; this campaign pulls
  its kernel forward and adds the delivery sequence for source graphs,
  evidence endpoints, and retrieval.
- Companion of: [Remote Source Connectors](remote-source-connectors.md), the
  file-tree profile and owner of the shared connector transport and non-git
  scope identity; [Slack Ingestion Connector](slack-ingestion-connector.md),
  the transcript-shaped profile;
  [API-Dataset Connector](api-dataset-connector.md), the API-dataset profile
  this program's M4 names, which owns that profile's wire lane, projection,
  action, and status contracts;
  [Secret custody across the checkout and corpus planes](../operations/config-artifacts/secrets-provider.md),
  which owns credential custody across both planes.
- Constrained by:
  [Locality-first decomposition](../daemon-runtime/locality-first-decomposition.md)
  (plane split, producer transport, no daemon checkout authority) and
  [Remote project onboarding](../daemon-runtime/remote-project-onboarding.md)
  (two-sided operator onboarding, no agent self-service).
- Gap ledger: API-dataset connector class `gap-0378c305` (designed,
  owned by `api-dataset-connector.md`); cross-entity evidence endpoints
  `gap-616857f8` (landed 2026-08-13 with M3); graph participation in
  unified retrieval `gap-5d57d2bb` (design landed, implementation in
  flight, owned by `unified-retrieval.md`); durable scope identity for
  non-git sources `gap-0c7ec76c` (landed; owned by the remote-source
  design, consumed here).

The campaign coordinates these designs and gaps. It does not duplicate their
lower-level contracts or close them by documentation alone.
