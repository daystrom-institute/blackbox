# bbox-providers — entity providers for graph inspection

- **Providers are property readers.** Each loads its entity from its own
  store. Only project graph vertices carry a neighborhood (their graph
  edges and evidence bindings); every other provider returns an empty one
  and no `recommended_next_hops`. A project file or knowledge entry named by
  an evidence binding gets that binding from the context's evidence
  resolver, so the edge is visible from both ends.
- **Symbol refs have no existence proof.** The indexer writes no entity doc
  a `symbol:` / `symbol_v2:` ref can resolve and the daemon keeps no edge
  graph, so the symbol providers resolve a ref only where an indexed entity
  doc backs it; otherwise they answer not found. A provider must degrade
  closed, never infer existence from a well-formed ref.
- A symbol's `defn_hash` IS the defining chunk's `chunk_hash`
  (`symbol_ref` in bbox-corpus-index project_files.rs) — current symbol
  refs were derivable from the retired `bbox_refactor_project_refs` MCP
  output without any search; the harness-side equivalent lives in the
  isolate `code.*` bindings. The eval refresh tooling depends on this
  equality.
