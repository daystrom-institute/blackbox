---
title: "Graph-native connector campaign"
kind: design
lifecycle: proposed
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
  - evidence
  - custody
brief: "The connecting delivery program for a reflective graph kernel, graph-native connector projections, a versioned Xero source schema, Files API evidence actions, tenant-owned record graphs, and unified retrieval."
---

# Graph-native connector campaign

## 0. Outcome

Deliver one coherent path from remote API observation to project-native
evidence:

- pull the reflective project graph forward as a small, generic semantic
  substrate;
- let connectors project remote systems into versioned source-owned graphs;
- keep tenant-authored record graphs separate from connector-owned source
  graphs;
- bind records to source facts, remote files, and materialized bytes with
  explicit evidence edges;
- make Xero the first worked connector profile, with its Files API as the
  premier action;
- expose graph vertices and evidence relationships through ordinary Blackbox
  inspection, traversal, and retrieval;
- harden hosted credential, identity, custody, and witness boundaries before
  production enablement.

The outcome is not "store Xero JSON in files," and it is not "make every
connector a graph database." The graph owns normalized semantic projection.
The connector runtime continues to own remote observation, checkpoints,
actions, byte transfer, placement, and operational evidence.

## 1. Why this is one campaign

Three designs currently meet at the same boundary:

1. Remote-source connectors define how Blackbox observes and materializes
   external content, but their first profile is a file tree.
2. The reflective project graph defines project-owned schema and facts, but its
   proposed first release deliberately omits cross-entity evidence and unified
   retrieval.
3. The first API-dataset use case needs both a typed business-system projection
   and a targeted Files API action that can return evidence without importing
   the whole remote system.

Building the API connector first would force its domain objects into either
project files or connector-specific core types. Building the graph in complete
isolation would prove structural reflection without testing the authority,
versioning, evidence, and retrieval contracts required by a real source.

The sequencing decision is therefore:

> Pull the reflective graph kernel forward first, then land a generic
> graph-projection connector profile, then make Xero the first versioned source
> schema and Files action on that profile.

This preserves a small generic core while giving the graph design a demanding
worked example early enough to correct it.

## 2. Program invariants

1. Blackbox core gains no Xero-specific enum variants, entity variants, or
   traversal branches.
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
    projection version, and materialization provenance.
14. Hosted enablement fails closed when required identity or secrets providers
    are unavailable.
15. Public artifacts use only generic scenarios and public vendor vocabulary.

## 3. Authority and data planes

### 3.1 Source graph

A source graph is a connector-managed, rebuildable projection of remote state.
For Xero it can contain entities such as contacts, invoices, tracking
categories, tracking options, files, and file associations.

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
  -> bbox:MATERIALIZED_AS
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
ordinary mutable graph facts.

## 4. Core contracts

The campaign introduces contracts in layers rather than one oversized
connector trait.

### 4.1 Connector observation

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

### 4.2 Graph projection

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
system.

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

### 4.4 Placement

`PlacementPolicy` decides what happens to remote bytes:

- `ephemeral`: return a bounded handle or stream and retain no durable copy;
- `project_owned`: materialize under an owned project root and index normally;
- `connector_cache`: retain under connector lifecycle and retention controls;
- `external_only`: retain remote reference and metadata only.

Placement is selected per action or source class. It is not implied by graph
membership.

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
                       v
M5 Xero source schema and projection
                       |
                       v
M6 Xero Files evidence action
                       |
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
catalog work begins.

## 6. Milestone 0: contracts and custody matrix

Deliverables:

- authority table for schema, facts, edges, checkpoints, bytes, and witness
  records;
- graph descriptor and version compatibility rules;
- source identity and deletion semantics;
- checkpoint-set transaction rules;
- placement and retention matrix;
- action capability descriptor;
- failure taxonomy for auth, rate limit, partial observation, projection,
  placement, and checkpoint commit;
- public-safe Xero fixtures covering entity, association, file, and deletion
  cases.

Exit gate: a design-level trace can follow one remote file from API
observation, through source vertex and evidence edge, to either an ephemeral
result or a project-owned file, with the authority and retention owner named at
every step.

## 7. Milestone 1: reflective graph kernel

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

Exit gate: two unrelated schemas can be loaded, validated, inspected, and
traversed with no new Rust domain variants.

## 8. Milestone 2: source-managed graph projections

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

Accepted implementation contract:

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

## 9. Milestone 3: cross-graph and cross-entity evidence

Resolve the evidence endpoint limitation tracked by `gap-161aedc6`:

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
materialized project file, and the reverse traversal preserves provenance.

Accepted implementation contract:

- tenant-owned bindings live in `.bbox/evidence/bindings.json`, outside both
  project-authored graph facts and connector-managed source snapshots;
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

## 10. Milestone 4: graph-aware connector runtime

Generalize the current file-tree connector shape into composable profiles:

- observation and checkpoint-set runtime;
- graph projection hook;
- content fetch and placement hook;
- declared actions;
- bounded retries, rate limits, and degradation status;
- witness emission;
- source graph refresh scheduling;
- compatibility adapter for file-tree mounts.

The existing remote-source design remains the file-tree profile. Its logical
paths, materialization manifest, chunking, and normal project indexing stay
valid. API-dataset connectors add semantic projection and action surfaces
without making file-tree connectors synthesize business graphs.

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
materialization is explicit.

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
surface remains deliberately narrow.

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

Resolve graph retrieval limitations tracked by `gap-fcd5b72e`:

- reflective graph vertices participate in exact and text retrieval according
  to graph policy;
- source and record results retain graph and authority identity;
- evidence-path expansion can accompany search results;
- materialized file chunks link back to remote source vertices;
- bounded evidence bundles expose graph paths, files, and provenance without
  dumping whole source graphs into model context;
- authorization filters apply before ranking and traversal expansion.

Vector indexing is optional and schema-directed. The graph kernel must not
assume every property is useful or safe to embed.

Exit gate: a record query can find the appropriate project, expand to its Xero
objects and files, and return bounded cited content with provenance intact.

## 16. Milestone 10: hosted identity, secrets, and witness hardening

Production hosted deployment requires the proposed auth and custody seams in
[Pluggable Secrets Providers](../operations/config-artifacts/secrets-provider.md)
to become concrete:

- Keycloak-issued OIDC identities establish workload identity and, if Blackbox
  exposes multi-principal hosted surfaces, human identity, tenant membership,
  and role claims at the service boundary;
- the OpenBao provider and token-store profiles hold Xero client credentials,
  rotating refresh tokens, signing material, and connector secret versions;
- services exchange Keycloak JWTs through OpenBao's JWT auth method for
  policy-scoped credentials rather than shipping static OpenBao tokens;
- graph, action, and materialization authorization is derived from verified
  identity and tenant scope;
- secret leases, rotation, revocation, and unavailable-provider behavior are
  tested;
- connector records contain secret references only;
- witness records are signed or hash-chained according to deployment policy;
- audit queries correlate actor, tenant, connector, action, graph generation,
  placement, and result without exposing secret or file content.

Local development may use the existing local identity and secrets profiles.
Hosted Xero enablement fails closed until the Keycloak and OpenBao integration
contracts are implemented and exercised. This is a delivery gate, not a claim
that either integration already exists in the current runtime.

Exit gate: cross-tenant denial, token rotation, lease expiry, identity-provider
outage, secrets-provider outage, connector revocation, and audit verification
all pass integration tests.

## 17. Release increments

| Release | Useful capability | Included milestones |
|---|---|---|
| R0 | Accepted contracts and fixtures | M0 |
| R1 | Project-defined reflective graphs | M1 |
| R2 | Rebuildable source graphs and generic evidence endpoints | M2-M3 |
| R3 | File-tree and API-dataset profiles on one runtime | M4 |
| R4 | Inspectable Xero semantic projection from fixtures | M5 |
| R5 | Project-code to bounded Xero file evidence | M6 |
| R6 | Durable Xero books overlay | M7 |
| R7 | Tenant record overlay with explicit mappings | M8 |
| R8 | Unified graph, evidence, and content retrieval | M9 |
| R9 | Hosted, auditable Xero operation | M10 |

R5 is the first product-shaped release. R1 through R4 are independently useful
substrate releases and must not be hidden behind the completion of hosted auth.

## 18. Verification strategy

Use a layered test matrix:

- graph contract tests for schema reflection, validation, identity, and atomic
  generations;
- projection golden tests for deterministic deltas and version replay;
- connector contract suites shared by file-tree and API-dataset profiles;
- recorded, public-safe Xero fixtures for pagination, optional fields, unknown
  values, associations, attachments, and rate limits;
- property tests for checkpoint monotonicity and graph-delta idempotence;
- failure injection between observation, projection, placement, witness, and
  checkpoint commit;
- authorization tests for graph scope, action scope, byte access, and
  cross-tenant denial;
- restart tests around in-flight actions and refresh;
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
- placement and witness mechanisms can proceed alongside evidence endpoints;
- Keycloak and OpenBao deployment probes can begin early, but they do not
  define graph semantics.

Hard ordering constraints:

- source projections depend on stable graph generations and descriptors;
- Xero implementation depends on the generic API-dataset profile;
- tenant mappings depend on stable cross-graph evidence edges;
- unified retrieval depends on stable generic graph references;
- hosted enablement depends on identity, secrets, authorization, and witness
  gates, even if local functionality ships earlier.

## 20. Decision ledger

Decided by this campaign:

- Pull the reflective graph kernel forward before the generalized API
  connector.
- Keep the first graph release narrow, then add source authority and evidence
  endpoints as explicit follow-on milestones.
- Model Xero as a versioned source schema and projection, not core code types.
- Keep source graphs and tenant record graphs separate.
- Treat graph projection as normalized current state, not operational witness.
- Make targeted connector actions first-class and ship the Files evidence
  action before requiring a durable whole-books overlay.
- Preserve the existing remote-source connector design as the file-tree
  profile.
- Require explicit placement policy for remote bytes.
- Gate hosted Xero operation on concrete Keycloak and OpenBao integration.
- Commit source graph state and named checkpoint transitions in one atomic
  source-projection snapshot.
- Store cross-plane evidence as a tenant-owned binding overlay, separate from
  both graph authority planes.

Still open:

- the exact retained-observation policy needed for offline reprojection;
- whether source schemas ship with the connector binary, as versioned corpus
  artifacts, or through a signed package registry;
- the minimum Xero entity set needed before the Files action can avoid broad
  reconciliation;
- which graph properties are eligible for text or vector indexing by default;
- whether tenant mapping assertions require human approval for every change or
  only for ambiguous matches.

## 21. Relationship to existing designs and gaps

- [Reflective Project Graph](../corpus/agentic-corpus/reflective-project-graph.md)
  owns the project-defined schema and fact model. This campaign pulls its
  kernel forward and adds the delivery sequence for source graphs, evidence
  endpoints, and retrieval.
- [Remote Source Connectors](remote-source-connectors.md) remains the concrete
  file-tree profile and supplies established sync, placement, secret-reference,
  and materialization constraints.
- [Agentic Corpus Platform](../corpus/agentic-corpus/agentic-corpus-platform.md)
  owns indexing, retrieval, and evidence-bundle integration.
- [Pluggable Secrets Providers](../operations/config-artifacts/secrets-provider.md)
  owns connector secret custody and currently treats the Keycloak-to-OpenBao
  path as a future seam. This campaign turns that seam into a hosted Xero
  release gate without making it a local-runtime prerequisite.
- The API-dataset connector-class gap remains tracked as `gap-94473f0c`.
- Cross-entity evidence endpoints remain tracked as `gap-161aedc6`.
- Graph participation in unified retrieval remains tracked as
  `gap-fcd5b72e`.

The campaign coordinates these designs and gaps. It does not duplicate their
lower-level contracts or close them by documentation alone.
