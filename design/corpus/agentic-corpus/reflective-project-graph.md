---
title: "Reflective Project Graph Overlay"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
  - graph
  - project-locality
---

# Reflective Project Graph Overlay

## 1. Thesis

Blackbox needs a project-scoped graph layer that users and agents can extend
without promoting every project-specific ontology into core bbox.

The motivating case is:

> Build a knowledge graph about this repo.

That graph may contain concepts like "orchestration choke point", "storage
invariant", "module cluster", "decision pressure", "migration hazard", or
"design-doc claim". Some of those concepts may later deserve promotion into
core bbox knowledge or typed corpus edges. Most should remain project-local
working epistemics: useful for navigation, audit, planning, and repeated agent
work, but not globally binding.

The proposal is a simpler reflective graph overlay:

- A tiny code-owned floor.
- User-defined vertex kinds and edge endpoint schemas as data.
- Project-scoped JSON/JSONL persistence under `.bbox/graphs/` or
  `.bbox/local/graphs/`.
- Projection into existing `EdgeIndex` traversal so `bbox_inspect_entity`,
  `bbox_find_paths`, and evidence bundles can use it.
- Clear separation between asserted project knowledge and core bbox ontology.

This deliberately borrows the useful part of the Erlang substrate's reflective
schema, not the full Daystrom epistemic model.

## 2. Donor Shape

The Erlang substrate in `../erlang-test/` proves a stronger pattern:

- `EntityTypeDef` rows declare entity kinds, required fields, status fields,
  legal transitions, active statuses, and validators.
- `EdgeTypeDef` rows declare legal `(edge_type, from_type, to_type)` triples.
- A code-resident `MetaSchema` is the recursion floor for those meta-types.
- The schema validator rejects undeclared endpoint triples before graph
  mutation.

The transferable idea is endpoint-typed schema as graph content.

The non-transferable part is the full epistemic ontology:

- `Decision`, `Concept`, `Consideration`, `Inquiry`, `Evidence`
- lane semantics
- projection into live BEAM modules
- hard lifecycle gates

Blackbox already has durable lanes for strong claims: knowledge, decisions,
threads, notes, roadmap items, artifacts, packets, and provenance. The overlay
should not become a second knowledge store with weaker discipline. It should be
a project-local graph scratchpad with enough schema to keep traversal coherent.

## 3. Fit With Current Blackbox

Current bbox graph state is a mix of closed and open pieces.

Closed pieces:

- `EntityType` is a Rust enum.
- `EntityRef` grammar is closed over known entity types.
- entity providers are statically registered.
- `bbox_describe_schema` renders the provider registry and fixed edge families.

Open pieces:

- `Edge.kind` is a string.
- edges already carry `provenance`, `confidence`, and metadata.
- `EdgeIndex` is rebuilt from stores plus JSONL sidecars.
- `.bbox/` already exists as a project-local home for blackbox artifacts.
- sidecar lanes already distinguish derived, explicit, observed, and
  materialized data.

Therefore the cheapest coherent path is not arbitrary runtime `EntityType`
extension. It is one generic core vertex entity whose properties carry the
user-defined kind. Graph schemas remain project-graph metadata in v1, exposed by
project-graph tools, not a second queryable `EntityType`.

## 4. Goals

1. Let users and agents define project-local vertex kinds and edge kinds.
2. Validate overlay facts against a declared schema before loading them.
3. Traverse overlay facts with existing bbox graph tools.
4. Keep overlay data easy to review, commit, diff, and delete.
5. Preserve provenance and confidence on every asserted fact.
6. Make promotion to core knowledge explicit.
7. Avoid turning every repo-specific ontology into core Rust enum variants.
8. Avoid hidden daemon-state mutation when the user wanted repo-owned data.

## 5. Non-goals

- No replacement for `bbox_learn`, `bbox_decide`, or `bbox_remember`.
- No automatic promotion from overlay facts to durable knowledge.
- No arbitrary Rust provider plugins in v1.
- No full graph database query language.
- No LLM in the synchronous search path.
- No attempt to enforce Erlang-style lifecycle transitions in v1.
- No global user ontology shared across all projects in v1.
- No automatic writes from ambient agent reasoning without an explicit tool call
  or committed file change.

## 6. Core Model

### 6.1 Core Entity Type

Add one minimal code-owned ref:

```text
project_graph_vertex:<project_id>:<graph_id>:<vertex_id>
```

`project_graph_vertex` is a generic vertex. Its stored properties include:

```json
{
  "project_id": "d723917f",
  "graph_id": "repo-kg",
  "vertex_id": "module:src/tools/graph.rs",
  "kind": "module",
  "label": "graph tools module",
  "properties": {
    "path": "src/tools/graph.rs",
    "summary": "MCP graph tool adapter layer"
  },
  "provenance": {
    "mode": "asserted",
    "source": "manual|agent|import|derived",
    "refs": ["project_file:..."]
  }
}
```

The `kind` is user-defined data, not a Rust enum variant.

`project_id` is the registered bbox project id. It is supplied by the project
registry and must not contain `:`.

`graph_id` is a filesystem-safe identifier matching `[A-Za-z0-9][A-Za-z0-9._-]*`.
It must not contain `:`, `/`, or `\`, and it must match the graph directory
name.

`vertex_id` is the opaque tail segment of the ref. The parser splits the ref
after `<project_id>` and `<graph_id>`, then treats the remainder as the raw
vertex id. This mirrors the existing `symbol:<project_id>:<qualified_name>:...`
pattern where one segment may contain `::`. The canonical renderer does not
percent-encode `:` or `/` in the tail:

```text
project_graph_vertex:d723917f:repo-kg:module:src/tools/graph.rs
```

`project_id` and `graph_id` are therefore delimiter-safe. `vertex_id` is not.
The parser uses two `split_first(':')`-style operations after the prefix;
`vertex_id` is the remainder. `vertex_id` must be non-empty and must not contain
`\n` or `\r`, so one JSONL row remains one graph fact. `EntityRef::try_render`
must reject delimiter characters in `graph_id` and round-trip raw `vertex_id`
exactly.

### 6.2 Edge Records

Overlay edges reuse the existing `Edge` shape:

```json
{
  "source": "project_graph_vertex:d723917f:repo-kg:module:src/tools/graph.rs",
  "kind": "repo_kg:DECLARES_TOOL",
  "target": "project_graph_vertex:d723917f:repo-kg:tool:bbox_inspect_entity",
  "provenance": "explicit",
  "confidence": "exact",
  "metadata": {
    "project_graph.asserted_by": "codex",
    "project_graph.source_refs": "project_file:d723917f:...",
    "project_graph.created_at": "2026-05-17T00:00:00Z",
    "project_graph.from_kind": "module",
    "project_graph.to_kind": "tool"
  }
}
```

Overlay edges may only use existing `EdgeProvenance` and `EdgeConfidence` enum
values. They cannot invent new provenance lanes.

`Edge.metadata` is currently `BTreeMap<String, String>`, so structured values
such as `source_refs` are serialized as compact strings in v1. If richer
metadata is needed later, that is a separate `Edge` schema migration.

Overlay edge kinds must be namespaced. The prefix before `:` must match the
graph namespace declared in `graph.json`; unqualified kinds are rejected. This
single rule prevents accidental collisions with reserved core edge names such
as `SUPERSEDES`, `DERIVED_FROM`, `IN_FILE`, `CALLS`, `COMMIT_TOUCHED_FILE`, and
other fixed bbox edge families.

### 6.3 Endpoint Schema

Each overlay graph declares endpoint legality:

```json
{
  "edge_type": "repo_kg:DECLARES_TOOL",
  "from_kind": "module",
  "to_kind": "tool",
  "directed": true,
  "description": "Module exposes a named bbox MCP tool"
}
```

The loader validates:

- both endpoints exist,
- both endpoints belong to the same project graph unless explicitly allowed,
- endpoint vertex `kind` values match the declared edge endpoint schema,
- edge type is declared,
- required vertex properties are present,
- reserved core edge names are not used without a namespace.

When materializing an overlay edge, the loader copies `from_kind` and `to_kind`
into `metadata` under the `project_graph.*` namespace. This lets a restart-time
rebuild detect stale or orphaned materialized edges even before the source
graph is fully re-read.

## 7. Persistence Layout

### 7.1 Repo-owned Graphs

Committed overlay graphs live under:

```text
<project>/.bbox/graphs/
  repo-kg/
    graph.json
    vertices.jsonl
    edges.jsonl
```

`graph.json` contains schema and metadata. `vertices.jsonl` and `edges.jsonl`
are append/diff-friendly data files.

### 7.2 Local Scratch Graphs

Private or generated graphs live under:

```text
<project>/.bbox/local/graphs/
  repo-kg-draft/
    graph.json
    vertices.jsonl
    edges.jsonl
```

`.bbox/local/` is already gitignored by project init. This is the right place
for "agent built a temporary map while investigating" output.

### 7.3 Daemon Materialization

The daemon materializes validated overlay edges through the manifest-managed
edge loader, not by placing nested files under the legacy explicit lane.

```text
~/.local/state/blackbox/edges/materialized/project-graph/<project_id>/<graph_id>.jsonl
```

and registers that path in `materialized/manifest-index.json` as a
project-graph materialization for the workspace. This implies a small
`ManifestIndex` extension, for example a `project_graph_materializations` list
on each workspace entry. Reusing `active_snapshot`, `dirty_overlay`, or
`repo_materialization` would blur workspace-code snapshots with user-authored
overlay graphs.

Example manifest-index shape:

```json
{
  "version": 1,
  "workspaces": {
    "d723917f": {
      "manifest": "workspace/d723917f/manifest.json",
      "active_snapshot": "snapshots/d723917f/clean-abc123",
      "project_graph_materializations": [
        {
          "graph_id": "repo-kg",
          "path": "project-graph/d723917f/repo-kg.jsonl",
          "manifest": "project-graph/d723917f/repo-kg.manifest.json"
        }
      ]
    }
  }
}
```

The current legacy explicit-lane loader only scans top-level
`edges/explicit/<project_id>.jsonl` files. Nested explicit paths would be
silently skipped, so the project graph loader must not use them.

The source of truth remains the project graph files for repo-owned overlays.
Materialized state is rebuildable.

Each materialized graph records source file hashes in a small sidecar manifest:

```text
~/.local/state/blackbox/edges/materialized/project-graph/<project_id>/<graph_id>.manifest.json
```

If a source graph directory is deleted or its hashes no longer match, the next
project-graph refresh removes or rewrites the materialized path and updates the
manifest index. `EdgeIndex` rebuilds load only active manifest-index paths.

## 8. Example Graph

`graph.json`:

```json
{
  "version": 1,
  "graph_id": "repo-kg",
  "namespace": "repo_kg",
  "title": "Repository knowledge graph",
  "vertex_kinds": {
    "module": {
      "required": ["label", "path"],
      "properties": {
        "path": "string",
        "summary": "string"
      }
    },
    "tool": {
      "required": ["label", "mcp_name"],
      "properties": {
        "mcp_name": "string"
      }
    },
    "invariant": {
      "required": ["label", "claim"],
      "properties": {
        "claim": "string",
        "strength": "string"
      }
    }
  },
  "edge_types": [
    {
      "edge_type": "repo_kg:DECLARES_TOOL",
      "from_kind": "module",
      "to_kind": "tool",
      "directed": true
    },
    {
      "edge_type": "repo_kg:CONSTRAINED_BY",
      "from_kind": "module",
      "to_kind": "invariant",
      "directed": true
    }
  ]
}
```

`vertices.jsonl`:

```jsonl
{"vertex_id":"module:src/tools/graph.rs","kind":"module","label":"graph tools module","properties":{"path":"src/tools/graph.rs","summary":"MCP adapter for graph tools"}}
{"vertex_id":"tool:bbox_inspect_entity","kind":"tool","label":"bbox_inspect_entity","properties":{"mcp_name":"bbox_inspect_entity"}}
{"vertex_id":"invariant:canonical-entity-refs","kind":"invariant","label":"canonical entity refs","properties":{"claim":"Graph tools must use canonical EntityRef strings","strength":"hard"}}
```

`edges.jsonl`:

```jsonl
{"source":"module:src/tools/graph.rs","kind":"repo_kg:DECLARES_TOOL","target":"tool:bbox_inspect_entity","provenance":"explicit","confidence":"exact"}
{"source":"module:src/tools/graph.rs","kind":"repo_kg:CONSTRAINED_BY","target":"invariant:canonical-entity-refs","provenance":"explicit","confidence":"heuristic"}
```

Relative vertex ids in the project file are expanded by the loader into
canonical `project_graph_vertex:<project_id>:<graph_id>:<vertex_id>` refs.
`project_id` is inferred from the registered project. `graph_id` is inferred
from the graph directory and checked against `graph.json`. Rows do not repeat
those fields.

## 9. MCP Surface

The overlay is core graph functionality, not a workspace-tool file operation, so
the surface should stay under `bbox_*`.

Proposed v1 tools:

```text
bbox_project_graph_list(project?)
bbox_project_graph_validate(project, graph_id?, source?)
bbox_project_graph_import(project, graph_id, source?)
bbox_project_graph_describe(project, graph_id?)
bbox_project_graph_put_vertex(project, graph_id, vertex)
bbox_project_graph_put_edge(project, graph_id, edge)
bbox_project_graph_export(project, graph_id, target?)
```

Read integration:

- `bbox_describe_schema` remains parameterless and lists the fixed
  `project_graph_vertex` entity type unconditionally once implemented.
  It should also include a "Project graph" edge-family pointer that tells
  callers to use `bbox_project_graph_describe` for per-project edge kinds.
- `bbox_project_graph_describe(project, graph_id?)` returns user-defined
  vertex kinds, endpoint schema, graph namespace, counts, source paths, and
  validation status for project graphs.
- `bbox_inspect_entity` supports `project_graph_vertex:*`.
- `bbox_find_paths` traverses overlay edges because they are in `EdgeIndex`.
- `bbox_bundle_evidence` renders project graph vertices as normal refs.

The write tools should be explicit. Passive search, inspect, and path-finding
must never invent graph facts.

If `project` is omitted from `bbox_project_graph_list`, it lists graphs for all
registered projects. Mutating tools require an explicit project.

`put` means idempotent upsert by `vertex_id` or edge key. It rewrites the JSONL
file atomically rather than blindly appending duplicate rows. Append-only import
can be added later as a separate bulk-ingest mode.

## 10. Loader And Admission

The loader is owned by a `ProjectGraphStore`.

`ProjectGraphStore` responsibilities:

- scan registered project roots for `.bbox/graphs/*` and, when enabled,
  `.bbox/local/graphs/*`,
- parse and validate graph source files,
- hold an in-memory map of graph schemas and vertices,
- materialize validated edges into manifest-managed edge sidecars,
- expose vertex lookup for the `project_graph_vertex` provider,
- expose graph summaries for `bbox_project_graph_describe`,
- refresh on explicit write/import and on project register.

`EdgeStoreRefs` gains a `project_graphs` reference so `EdgeIndex::rebuild` can
load active project-graph materializations and reject stale materialized rows
whose source graph is no longer valid. The provider registry gains a
`ProjectGraphVertexProvider`; `provider_for(EntityType::ProjectGraphVertex)`
must never see a ref that the store cannot resolve without returning
`error.not_found`.

The source loader has three phases:

1. Read `graph.json` and validate schema shape.
2. Read `vertices.jsonl`, validate required fields, and build vertex map.
3. Read `edges.jsonl`, expand endpoint refs, validate endpoint legality, then
   emit `Edge` records.

Validation failures should be reported per row with stable row identifiers:

```json
{
  "status": "error.invalid_project_graph",
  "graph_id": "repo-kg",
  "errors": [
    {
      "file": "edges.jsonl",
      "line": 12,
      "code": "undeclared_endpoint_pair",
      "edge_type": "repo_kg:DEPENDS_ON",
      "from_kind": "module",
      "to_kind": "decision"
    }
  ]
}
```

Default policy: an invalid graph does not partially load. This keeps traversal
semantics simple.

Optional future policy: `load_valid_rows=true` for scratch graphs, surfaced as
degraded output.

Fatal validation errors:

- malformed `graph.json`,
- graph id charset violation,
- graph id does not match the graph directory name,
- duplicate vertex ids with incompatible rows,
- unknown vertex kind,
- missing required property,
- undeclared edge type,
- endpoint vertex missing,
- endpoint kind mismatch,
- namespace mismatch,
- cap exceeded.

Duplicate vertex rows are incompatible when they declare different `kind` values
or different values for required properties. Identical rows are ignored.
Optional-property additions may merge during import, but `put` should normalize
to one row on disk.

Warnings:

- unused vertex kind,
- declared edge type with no observed edges,
- stale materialized file removed,
- local graph shadows a committed graph.

Breaking schema edits are therefore fail-closed in v1. If a kind is removed
while vertices or edges still use it, the graph does not load until the facts
are migrated or the kind is restored.

V1 caps:

```text
max_graphs_per_project = 32
max_vertices_per_graph = 10000
max_edges_per_graph = 50000
max_graph_json_bytes = 512 KiB
max_vertices_jsonl_bytes = 10 MiB
max_edges_jsonl_bytes = 25 MiB
```

Cap failures return `error.project_graph_cap_exceeded` with the graph id, file,
cap name, observed value, and configured limit.

At the default caps, worst-case in-memory state is intentionally bounded but not
free: 32 graphs x 10k vertices plus 32 graphs x 50k edges is a large daemon
resident set. Operators with memory-constrained hosts should lower these caps.

Startup and refresh ordering is:

```text
ProjectGraphStore.load
  -> reconcile project_graph_materializations in manifest-index
  -> mark stale/orphaned materializations inactive or remove them
  -> EdgeIndex::rebuild
```

`EdgeIndex::rebuild` should not load project-graph materialized paths before
`ProjectGraphStore` has reconciled them.

## 11. Query Semantics

The overlay participates in graph traversal, but not necessarily in full-text or
vector search at first.

V1:

- inspect by exact ref,
- path traversal through overlay edges,
- `bbox_project_graph_describe` with graph namespace summary,
- bundle evidence with vertex previews.

V1 intentionally does not return overlay vertices from `bbox_hybrid_search` or
`bbox_discover_seed_entities`. Without Tantivy documents there is no label
search lane. Callers discover overlay vertices either from exact refs, from
paths connected to known code/knowledge refs, or from `bbox_project_graph_list`
and `bbox_project_graph_describe`.

V1.5:

- index overlay vertex labels/properties into Tantivy through a new
  `project_graph_vertex` document type,
- search returns `project_graph_vertex` hits,
- per-graph project filter applies.

V2:

- optional embeddings for overlay vertices,
- graph-aware ranking boosts when overlay vertices connect to code chunks,
- import/export from external graph tools.

## 12. Provenance And Epistemics

Every vertex and edge should carry enough metadata to answer:

- who or what asserted this,
- what source refs support it,
- whether it is manual, agent-authored, imported, or derived,
- confidence level,
- creation/update timestamp.

Suggested common metadata keys:

```text
project_graph.asserted_by
project_graph.assertion_source
project_graph.source_refs
project_graph.created_at
project_graph.updated_at
project_graph.review_status
project_graph.promoted_to
```

Important distinction:

- Overlay fact: "this repo graph says module A is constrained by invariant B."
- Core knowledge: "future agents in this repo must obey invariant B."

Promotion from overlay to core knowledge should be deliberate:

```text
bbox_project_graph_promote(
  project="/repo/x",
  graph_id="repo-kg",
  vertex_id="invariant:canonical-entity-refs",
  target="knowledge",
  require_user_approval=true
)
```

This is not a v1 requirement, but the model should leave room for it.

Promotion is proposal-only. It must not call `bbox_learn`, `bbox_remember`, or
`bbox_decide` on an agent's behalf. A promotion helper may emit a reviewable
candidate or draft command, but durable memory writes remain operator-gated.
Promotion targets existing durable lanes such as `knowledge:`; promotion does
not mint new core `EntityType` variants.

## 13. Conflict And Lifecycle

V1 lifecycle is file-level:

- committed `.bbox/graphs/*` is repo-owned and reviewable in git,
- `.bbox/local/graphs/*` is private scratch,
- daemon materialization is rebuildable and manifest-managed,
- deletion of project graph files removes their materialized edges on next
  project-graph refresh.

V1 does not need per-vertex lifecycle transitions.

Schema changes are handled by versioned graph files:

- adding a vertex kind is additive,
- adding an edge type is additive,
- removing a kind or required field is breaking and should fail validation until
  facts are migrated,
- changing endpoint legality is breaking when existing edges violate it.

Future lifecycle fields can be data-level conventions:

```json
{"status":"draft|active|deprecated|superseded"}
```

but they should not be gate-enforced until real use shows which lifecycle
matters.

## 14. Security And Trust

Project graph files are input data. They must not execute code.

Rules:

- JSON only.
- No embedded scripts.
- No dynamic validators in repo files.
- No inline secrets.
- Bounded file sizes and row counts in v1.
- Namespaced edge kinds to prevent accidental core edge impersonation.
- Source refs are parsed and validated before materialization.
- Cross-project overlay edges are not supported in v1.

If schema-level validators beyond required fields are needed, they should be
code-owned named validators, not arbitrary expressions in project files.

Validation is structural. It can prove that a graph is well-formed; it does not
prove that an agent-authored claim is semantically true.

## 15. Implementation Sketch

### Phase 1: Read-only Overlay Loader

- Add `ProjectGraph` and `ProjectGraphVertex` data structs.
- Add `EntityType::ProjectGraphVertex`.
- Add `ProjectGraphStore`.
- Add a generic provider for `ProjectGraphVertex`.
- Add parser/rendering for project graph refs.
- Extend `bbox_project_init` to scaffold `.bbox/graphs/` and
  `.bbox/local/graphs/`.
- Load `.bbox/graphs/*` during project register and daemon startup. Load
  `.bbox/local/graphs/*` only when local overlays are enabled for that project.
- Validate endpoint schemas.
- Materialize valid overlay edges through the manifest-managed edge loader.
- Project active overlay edges into `EdgeIndex`.
- Add tests for invalid endpoint pairs, missing vertices, and namespacing.

### Phase 2: Explicit Write Tools

- Add `bbox_project_graph_put_vertex`.
- Add `bbox_project_graph_put_edge`.
- Add atomic JSONL upsert behavior.
- Add validate-before-write.
- Rebuild the graph overlay after successful writes.

### Phase 3: Search Integration

- Add a `project_graph_vertex` Tantivy document type keyed by canonical
  `EntityRef`.
- Index vertex labels and selected properties.
- Return `project_graph_vertex` from `bbox_hybrid_search`.
- Render notable overlay edges in search results.

### Phase 4: Promotion Workflow

- Add proposal-only promotion helpers.
- Convert selected overlay vertices into `bbox_remember`, `bbox_learn`, or
  `bbox_decide` candidates with evidence refs attached.
- Require operator approval before durable memory writes.

## 16. Open Questions

1. Should `.bbox/local/graphs` be included in default graph traversal, or only
   when the caller opts in?
2. Should graph ids be globally unique per project, or should branches be
   allowed to carry conflicting graph ids with branch-specific materialization?
3. Should overlay graph manifests record current git branch/head and warn when
   loaded on a different branch, or is project-root scope enough for v1?
4. Should user-defined vertex kinds support JSON Schema fragments, or keep v1 to
   required-field lists and string properties?
5. How should duplicate vertex ids across committed and local graphs shadow each
   other?

## 17. Recommendation

Ship this as a small project-graph overlay, not as a generalized reflective
core ontology.

The key design move is one generic vertex entity plus endpoint-typed user schema.
That keeps Rust's core corpus stable while giving users a useful graph-shaped
scratchpad for repo-specific epistemics. The existing sidecar and `.bbox/`
machinery already provide most of the storage shape, but the materialization
must use the manifest-managed loader rather than nested legacy explicit paths.
The real work is the schema/admission contract, `ProjectGraphStore`, and the
provider integration.

Gap note: `note-dc9ddb16`.
