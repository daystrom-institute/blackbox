# bbox-source-graph: connector-managed source projections

- Connector-managed generations live in THIS store's own root, under neither
  `.bbox/graphs` nor `.bbox/local/graphs`. The separation is the authority
  boundary made physical: no checkout path writes here, and the store accepts
  connector authority only, so a connector refresh has no path to
  project-authored facts and a checkout has no path to a connector projection.
- One `accept` takes the descriptor, schema, normalized facts, source
  observation references, and the named checkpoint transition together. There
  is no partial acceptance: the graph and its checkpoint set move as one value
  or not at all.
- Generations advance by exactly one. Only an EXACT replay of the most
  recently accepted batch is idempotent; the same batch id with different
  content is a conflict, and a batch computed against a stale prior generation
  is refused.
- Every rejection class (graph validation, checkpoint conflict, schema
  rollback, snapshot integrity) refuses before the commit point, so the
  accepted generation and the accepted checkpoint set are unchanged. A
  snapshot that fails its own integrity check refuses to OPEN, which is the
  same guarantee by another route.
- `snapshot.json` is the only authority. `snapshot.prior.json` and the
  retained observation blobs are derived, advisory, and independently
  self-verifying, so there is no manifest or index sidecar to reconcile
  against and no torn pair to classify. The write ordering and every crash
  window are documented in the crate doc; that doc is the contract, so change
  the ordering there first.
- Checkpoints are a named set with compare-and-set advances. A mismatched
  `before`, an empty value, or a no-op advance is a refusal, never a
  last-writer-wins overwrite.
- Removing a vertex does not cascade to its edges. A delta that strands an
  edge is refused by name so the connector author learns which edge they
  forgot, rather than reading a generic validation failure.
- A full reconciliation that observed nothing may not remove everything
  without `allow_empty_full_reconciliation`. "The remote returned nothing" and
  "the remote is empty" must not look the same.
- Retained observations are content addressed and immutable once written
  (decision 98d9f430f62ad8ca): current plus prior generation plus a retention
  window, per source class. A replay past the horizon reports itself
  incomplete so the caller degrades honestly to re-observation instead of
  projecting a partial history.
- Status carries generation, schema and projection versions, graph
  fingerprint, latest observation time, reconciliation mode, and named
  checkpoints. It carries no observation payload and no credential material,
  and nothing may be added to it that could.
- Observation is not projection. This crate owns durable acceptance only: the
  producer plane observes, a `GraphProjection` maps an accepted batch to a
  delta deterministically without remote access, and the transport endpoint
  that carries batches to the corpus is M4's, not this crate's.
- Tests use canonicalized per-test roots and never read or write real HOME or
  XDG state.
