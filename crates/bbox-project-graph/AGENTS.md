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
  metadata. Project authority is file-backed and tenant-owned. Connector
  authority is accepted only through `SourceProjectionStore`, uses
  connector-managed retention, and couples the validated graph delta and named
  checkpoint transition in one atomic snapshot write.
- Connector projections advance by exactly one generation, are idempotent only
  for an exact replay of the most recently accepted batch, and fail closed on
  checkpoint conflicts, schema rollback, connector identity changes, or
  conflicting delta operations. The status view exposes accepted generation,
  versions, observation freshness, and named checkpoints without credential
  material.
- Connector-managed storage is not a file-backed project graph root.
  `ProjectGraphCatalog` can hold its accepted generations, but discovery under
  `.bbox/graphs` and `.bbox/local/graphs` never synthesizes connector state.
  Evidence endpoints, text/vector retrieval, and hosted auth remain outside
  this crate.
- Tests use canonicalized per-test roots and never read or write real HOME or
  XDG state.
