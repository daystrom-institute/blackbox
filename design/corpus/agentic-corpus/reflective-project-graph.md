---
title: "Reflective Project Graph"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - corpus
  - agentic-corpus
  - graph
  - project-locality
---

# Reflective Project Graph

Status: proposed

## Thesis

Blackbox should let a project carry its own graph-shaped ontology without
requiring new Rust enum variants, new bbox core edge families, or a durable
knowledge-store promotion for every local concept.

The core idea is small:

```text
the project defines a schema as graph data
the project adds facts that instantiate that schema
bbox validates only enough to keep the graph coherent
bbox traversal can then walk those facts like any other graph edges
```

This is a reflective graph: the graph can describe the types and edge types
used by the graph.

The useful target prompt is:

```text
build a knowledge graph about this repo
```

A project might define concepts such as `Module`, `Invariant`, `Subsystem`,
`MigrationHazard`, `DecisionPressure`, or `DesignClaim`. Those are project
ontology, not bbox ontology. Bbox should not need to understand their domain
meaning to store, validate, inspect, and traverse them.

## Design Center

The design center is user-owned schema, not bbox-owned classification.

Bbox owns a tiny interpreter vocabulary. The project owns everything above it.

The interpreter vocabulary exists only to answer these questions:

- Which graph vertices are type definitions?
- Which graph vertices are edge type definitions?
- What source and target vertex types does an edge type allow?
- Which type does a vertex instantiate?
- Is a fact structurally valid against the declared project schema?

It does not answer these questions:

- Which domain concepts should the project use?
- Which lifecycle states are correct?
- Which claims are durable operational rules?
- Which facts are semantically true?
- Which project ontology deserves promotion into bbox core?

## The Fixed Floor

The fixed floor is the smallest code-owned vocabulary needed to interpret the
rest of the graph as schema.

Conceptually:

```text
meta:VertexType
meta:EdgeType

meta:INSTANCE_OF
meta:FROM_TYPE
meta:TO_TYPE
```

That is enough to express both schema and facts.

Example schema:

```text
repo:Module         --meta:INSTANCE_OF--> meta:VertexType
repo:Invariant      --meta:INSTANCE_OF--> meta:VertexType
repo:CONSTRAINED_BY --meta:INSTANCE_OF--> meta:EdgeType
repo:CONSTRAINED_BY --meta:FROM_TYPE----> repo:Module
repo:CONSTRAINED_BY --meta:TO_TYPE------> repo:Invariant
```

Example facts:

```text
repo:src/tools/graph.rs         --meta:INSTANCE_OF--> repo:Module
repo:canonical-entity-refs      --meta:INSTANCE_OF--> repo:Invariant
repo:src/tools/graph.rs         --repo:CONSTRAINED_BY--> repo:canonical-entity-refs
```

The recursion stops at the fixed floor. `meta:VertexType` and `meta:EdgeType`
are built in. Project type definitions are ordinary graph nodes above that
floor.

## Why A Floor Exists

The floor is not a policy guard on the user's schema. It is the interpreter
contract that makes "schema as graph data" possible without infinite regress.

Without a floor, either:

- schemas live in a separate non-graph registry that cannot be inspected as
  graph data, or
- `VertexType` itself needs another type definition forever.

The floor should remain boring and hard to expand. Every addition to the floor
is a claim that bbox itself must permanently understand that concept. Most
project concepts should stay above the floor as user schema.

## Project Ownership

A project graph belongs to one registered project root.

Project-owned schema and facts should be reviewable, diffable, and deletable by
the project.

Each project graph declares one project namespace. The namespace is independent
from the graph id, though they may be the same string. The graph id names the
storage directory and ref segment; the namespace prefixes graph-defined vertex
types and edge types.

Example namespaces:

```text
repo:Module
repo:CONSTRAINED_BY
```

Bbox reserves the `meta:` namespace for the fixed floor and `bbox:` for
core-owned concepts. Project namespaces must not use those reserved prefixes.

## Structural Validation

Validation is structural only.

For each project graph, bbox should reject facts that are incoherent according
to the declared graph schema:

- vertex has no declared type,
- vertex type is not a `meta:VertexType`,
- edge type is not a `meta:EdgeType`,
- edge type has no source or target type declaration,
- edge source vertex does not instantiate the declared source type,
- edge target vertex does not instantiate the declared target type,
- fact uses a reserved namespace incorrectly,
- vertex type or edge type name does not use the graph namespace,
- vertex id is duplicated within a graph,
- edge type declaration is duplicated within a graph,
- JSON or JSONL input is malformed,
- referenced vertex is missing.

Validation does not mean the claim is true. It means the graph can be
interpreted consistently.

For example, bbox can validate this as well-formed:

```text
repo:src/tools/graph.rs --repo:CONSTRAINED_BY--> repo:canonical-entity-refs
```

It cannot prove that `src/tools/graph.rs` is actually constrained by that
invariant. Truth remains a provenance and review problem outside the fixed
floor.

## Storage Shape

V1 should use project-owned JSON files. The exact row shape can evolve, but the
ownership boundary should not.

Requirements:

- committed graphs live under the project, not hidden daemon state,
- local scratch graphs have a separate ignored location,
- schema and facts can be reviewed in git,
- schema and data are separate files,
- deletion of the files deletes the graph on the next refresh,
- generated materialization, if any, is rebuildable.

Proposed shape:

```text
<project>/.bbox/graphs/<graph-id>/
  schema.json
  vertices.jsonl
  edges.jsonl

<project>/.bbox/local/graphs/<graph-id>/
  schema.json
  vertices.jsonl
  edges.jsonl
```

`schema.json` declares graph-defined vertex types and edge types.
`vertices.jsonl` carries typed vertices plus their properties. `edges.jsonl`
carries facts between vertices.

This is still one graph model. The split is for review and validation hygiene,
not a claim that schema is a separate non-graph authority.

`schema.json` is the authoritative schema-graph document. Its entries are a JSON
encoding of graph facts about types:

- each `vertex_types` key projects to a vertex that instantiates
  `meta:VertexType`,
- each `edge_types` row projects to a vertex that instantiates `meta:EdgeType`,
- each `edge_types` row also projects to `meta:FROM_TYPE` and `meta:TO_TYPE`
  edges.

`vertices.jsonl` and `edges.jsonl` are the authoritative data facts that
instantiate that schema. If they reference a type not declared by `schema.json`,
the graph is invalid.

Meta vertices and meta edges are projection-only in v1. Users declare them
through `schema.json`, not by hand-authoring `meta:*` rows in `vertices.jsonl`
or `edges.jsonl`.

`vertices.jsonl` and `edges.jsonl` are normalized state files, not append-only
event logs. Duplicate vertex ids and duplicate edge keys are invalid. An edge
key is `(from, type, to)`. Multiple pieces of evidence for the same relation
belong in the edge properties, not in duplicate edge rows. If write tools are
added, they should rewrite the affected JSONL file atomically rather than append
conflicting rows.

Example `schema.json`:

```json
{
  "version": 1,
  "namespace": "repo",
  "vertex_types": {
    "repo:Module": {
      "required": ["path"],
      "properties": {
        "path": "string",
        "summary": "string"
      }
    },
    "repo:Invariant": {
      "required": ["claim"],
      "properties": {
        "claim": "string",
        "strength": "string"
      }
    }
  },
  "edge_types": [
    {
      "type": "repo:CONSTRAINED_BY",
      "from_type": "repo:Module",
      "to_type": "repo:Invariant",
      "required": ["source"],
      "properties": {
        "source": {
          "path": "string",
          "heading": "string"
        },
        "confidence": "string"
      }
    }
  ]
}
```

Vertex type names and edge type names must use the graph namespace as a prefix.
Bare names are rejected.

Project graph edges connect project graph vertices in v1. References to
existing bbox entities such as `project_file:*`, `knowledge:*`, or `commit:*`
can be stored as vertex or edge properties using canonical ref strings, but
they are not traversable project graph edge endpoints in v1. Traversable edges
to non-project-graph entities are a future extension.

## Properties

Vertices need properties. A graph of typed IDs is too thin to be useful for
repo knowledge.

Every vertex should have:

- a stable project-local id,
- a graph-defined type,
- a human label,
- a property bag.

`id`, `type`, `label`, and `properties` are reserved top-level keys. User
fields live inside `properties`.

Example vertex:

```json
{
  "id": "src/tools/graph.rs",
  "type": "repo:Module",
  "label": "graph tool adapter module",
  "properties": {
    "path": "src/tools/graph.rs",
    "summary": "MCP adapter layer for graph inspection and traversal tools"
  }
}
```

V1 should validate declared property requirements from the schema:

```json
{
  "vertex_types": {
    "repo:Module": {
      "required": ["path"],
      "properties": {
        "path": "string",
        "summary": "string"
      }
    }
  }
}
```

Property schemas should support JSON objects, not just flat scalar fields.
Repo knowledge often needs structured locations, owner lists, references,
measurements, or evidence summaries. A scalar-only bag would force useful data
back into strings too quickly.

Example nested property schema:

```json
{
  "vertex_types": {
    "repo:DesignClaim": {
      "required": ["claim", "source"],
      "properties": {
        "claim": "string",
        "source": {
          "path": "string",
          "heading": "string"
        },
        "tags": ["string"]
      }
    }
  }
}
```

This gives enough structure for authoring and review without making the fixed
floor a general property ontology. Property definitions belong to the project
schema. Bbox should enforce required property presence and JSON shape checks,
including nested object and array shape, but should not attach bbox-owned
semantic meaning to those properties.

V1 property schema terms:

- `"string"`
- `"number"`
- `"boolean"`
- nested objects, whose values are property schema terms,
- arrays with one element schema, such as `["string"]` or `[{"path":"string"}]`.

`null` is not a schema term in v1. Missing optional properties are allowed;
present properties must match their declared shape.

## Edge Facts

Edges are facts between vertices. They should also be able to carry properties,
because many useful project graph relationships need evidence, source location,
confidence, or a short note.

Example edge:

```json
{
  "from": "src/tools/graph.rs",
  "type": "repo:CONSTRAINED_BY",
  "to": "canonical-entity-refs",
  "properties": {
    "source": {
      "path": "PROJECT.md",
      "heading": "Workflow"
    },
    "confidence": "reviewed"
  }
}
```

The edge `type` must be declared in `schema.json`. In v1 each edge type has one
declared `(from_type, to_type)` pair. Edge properties use the same JSON shape
rules as vertex properties.

## Query Semantics

The graph should participate in existing graph traversal once loaded.

V1 must support:

- list project graphs,
- validate a project graph,
- inspect a project graph vertex by exact ref,
- traverse project graph edges,
- include project graph vertices and edges in evidence bundles.

`bbox_project_graph_validate` returns validation errors with file, line number
when applicable, stable error code, and message. An empty error list means the
graph is structurally valid.

Search comes later. V1 requires exact refs, graph listing, or traversal from
known graph vertices. Indexing project graph vertices into full-text or vector
search is deferred until the graph model proves useful.

The important semantic rule is:

```text
read paths may traverse project graph facts
read paths must not invent project graph facts
```

Agents can propose or write graph facts only through explicit mutation surfaces
or normal file edits.

## Ref Shape

Bbox needs one generic ref family for project graph vertices.

Conceptually:

```text
project_graph_vertex:<project-id>:<graph-id>:<vertex-id>
```

The `vertex-id` is project-owned. Bbox should not parse domain meaning out of
it beyond basic safety and round-tripping.

`graph-id` must not contain `:`. `vertex-id` may contain `:`. The parser splits
out the project id and graph id first; the remainder is the raw vertex id.

Example:

```text
project_graph_vertex:d723917f:repo:src/tools/graph.rs
project_graph_vertex:d723917f:repo:canonical-entity-refs
project_graph_vertex:d723917f:repo:CONSTRAINED_BY
```

`meta:INSTANCE_OF`, `meta:FROM_TYPE`, and `meta:TO_TYPE` are edge kinds. Domain
edge kinds such as `repo:CONSTRAINED_BY` are graph-defined edge kinds.

The fixed floor is self-describing by construction:

```text
meta:VertexType  --meta:INSTANCE_OF--> meta:VertexType
meta:EdgeType    --meta:INSTANCE_OF--> meta:VertexType
meta:INSTANCE_OF --meta:INSTANCE_OF--> meta:EdgeType
meta:FROM_TYPE   --meta:INSTANCE_OF--> meta:EdgeType
meta:TO_TYPE     --meta:INSTANCE_OF--> meta:EdgeType
```

The floor edge endpoint pairs are built in:

```text
meta:INSTANCE_OF: any vertex -> meta:VertexType
meta:FROM_TYPE:   meta:EdgeType -> meta:VertexType
meta:TO_TYPE:     meta:EdgeType -> meta:VertexType
```

These self-references are the recursion base case.

## Minimal Tool Surface

The tool surface should be small at first.

Read and validation:

```text
bbox_project_graph_list(project?)
bbox_project_graph_describe(project, graph_id)
bbox_project_graph_validate(project, graph_id)
```

Mutation can be file-first in V1. If tool writes are added, they should be
explicit and boring:

```text
bbox_project_graph_put_vertex(project, graph_id, vertex)
bbox_project_graph_put_edge(project, graph_id, edge)
```

No passive search, inspect, or agent reasoning path should create facts.

## Relationship To Bbox Knowledge

Project graph facts are not durable memory directives.

This graph fact:

```text
repo:src/tools/graph.rs --repo:CONSTRAINED_BY--> repo:canonical-entity-refs
```

means:

```text
the project graph asserts this relationship
```

It does not mean:

```text
future agents must obey this as rendered bbox knowledge
```

Promotion to `bbox_learn`, `bbox_remember`, or `bbox_decide` remains explicit
and operator-gated. The project graph can provide evidence and candidates, but
it should not become a weaker back door into durable memory.

## Non-goals

- No project-specific Rust enum variants.
- No attempt to make every local ontology part of bbox core.
- No lifecycle system in the fixed floor.
- No epistemic framework in the fixed floor.
- No automatic promotion to knowledge, decisions, pins, roadmap items, or
  system memories.
- No LLM in validation.
- No graph database query language in V1.
- No automatic writes from ambient agent reasoning.
- No cross-project ontology sharing in V1.
- No traversable edge endpoints outside project graph vertices in V1.
- No implementation commitment to a specific materialization backend.

## Implementation Boundary

This design intentionally does not specify daemon materialization internals.

The implementation will still need a loader, an entity provider for project
graph vertices, and a way to expose validated edges to graph traversal. Those
are implementation details. They should be designed after the model above is
accepted.

The model should survive different storage implementations:

- direct load from project files,
- generated sidecar edges,
- indexed documents for search,
- future import/export with external graph tools.

If the implementation design cannot preserve the small reflective model, the
implementation is probably too large.

## Resolved Choices

- Local scratch graphs do not participate in traversal by default. Callers must
  explicitly include `.bbox/local/graphs/*`.
- Each edge type declaration allows one `(from_type, to_type)` pair. Polymorphic
  edge types can be added later if real graphs need them.
- Property validation supports JSON objects and arrays, not only flat scalar
  fields.
- Project graph vertices are not indexed into full-text or vector search in V1.
  Use list, describe, exact inspect, and traversal first.
- `schema.json` is the schema-graph document. It is stored separately for
  review, but loaders project it into meta vertices and meta edges.
- A graph has one project namespace. The namespace is independent from graph id,
  but graph-defined edge and vertex type names must use it.
- Edges may carry properties, with the same JSON shape validation as vertices.

Deferred beyond v1:

- Cross-project graph references.
- Traversable edges to existing bbox entities such as project files,
  knowledge, commits, notes, or threads.
- Multiple endpoint pairs for one edge type.
- Full JSON Schema compatibility.
- Full-text or vector indexing of project graph vertices.

## Recommendation

Ship the reflective model first, not the ecosystem around it.

The first useful version needs only:

- a tiny fixed floor,
- project-owned schema as graph data,
- project-owned facts,
- structural validation,
- exact inspect and graph traversal.

Everything else is optional pressure discovered from use.
