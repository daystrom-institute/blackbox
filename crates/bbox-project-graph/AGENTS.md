# bbox-project-graph: reflective project graph kernel

- The fixed floor is exactly `meta:VertexType`, `meta:EdgeType`,
  `meta:INSTANCE_OF`, `meta:FROM_TYPE`, and `meta:TO_TYPE`. Project vocabulary
  stays in schema data and must not become Rust domain variants.
- Committed graphs live under `.bbox/graphs`; `.bbox/local/graphs` is excluded
  unless a caller explicitly opts in. A committed and local graph with the
  same id is ambiguous when local graphs are included and must fail closed.
- Source documents are read into one stable candidate, structurally validated,
  then published to the accepted-generation catalog in one lock-protected
  replacement. Invalid, rolled-back, or same-generation divergent candidates
  never replace the prior accepted generation.
- The descriptor owns authority, schema compatibility, custody, and generation
  metadata. Authority and storage source are ONE decision, checked in both
  directions: project authority never uses connector-managed storage, and
  connector authority never uses a checkout graph root. That pair of refusals
  is what keeps a checkout from authoring a connector graph and a connector
  refresh from reaching project-authored facts. The connector-managed source
  is deliberately rootless (`relative_root() == None`), so checkout discovery
  cannot reach it however it is asked.
- Connector projection mechanics (deltas, named checkpoints, retained
  observations, the atomic snapshot) live in `bbox-source-graph`, which
  consumes this crate's descriptor, schema, validation, and `build_generation`.
  Evidence endpoints, text/vector retrieval, and hosted auth remain outside
  both crates.
- Schema property terms may carry retrieval annotations
  (`{"type": <term>, "index": "none|word|text", "embed": <bool>}`) under a
  per-graph `index_policy`. They are structural here: validated, preserved,
  and read by nobody yet. Embedding is per-kind opt-in under the per-graph
  policy, and an opt-in without the policy is a schema error rather than a
  silent downgrade. The annotated form requires an `index` or `embed` sibling,
  so a bare `{"type": ...}` keeps its older nested-object meaning.
  metadata. M1 accepts project authority only. Connector projection authority,
  checkpoint coupling, text/vector retrieval, and hosted auth remain outside
  this crate.
- Evidence bindings (`evidence.rs`) are TENANT-owned and are not graph facts.
  They live in `.bbox/evidence/bindings.json`, outside both `.bbox/graphs` and
  any connector-managed snapshot, and a connector reprojecting or deleting an
  endpoint moves a binding's freshness without deleting the binding. One
  complete valid document replaces one project's accepted set; an invalid
  candidate replaces nothing.
- The committed binding document names project-scoped endpoints structurally
  (graph id, vertex id, path/chunk hashes), never as canonical refs, because a
  canonical project-scoped ref embeds a host-assigned `project_id` and the file
  is cloned across hosts. Canonical `EntityRef` endpoints are materialized at
  load with the project id supplied by the lane. The literal-ref escape hatch
  exists for project-independent types only and refuses the rest.
- This crate scores endpoint freshness but does not observe it. The read plane
  looks an endpoint up and hands back an `EvidenceEndpointObservation`; the
  status algebra and the aggregation live here so they stay testable without a
  daemon. Only `unauthorized` refuses traversal - stale and missing endpoints
  stay traversable and labeled.
- Tests use canonicalized per-test roots and never read or write real HOME or
  XDG state.
