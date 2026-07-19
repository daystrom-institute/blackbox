# bbox-providers — entity providers for graph inspection

- **Symbols are edge-projected vertices: no entity doc exists.** The
  indexer derives DEFINED_IN/CONTAINS_SYMBOL/CALLS edges for `symbol:` /
  `symbol_v2:` refs but never writes a doc the entity index can resolve
  (gap-496fe07f). Existence = edge participation: the symbol providers
  treat `entity_properties` as enrichment and fall back to
  `ctx.edge_index()` when it misses, stamping `source=edge_projection`.
  Requiring an indexed doc here silently 404s every symbol vertex the
  graph itself emits — that ran undetected in prod until 2026-06.
- `ProviderContext::with_edge_index` is optional by design: call sites
  that hold an edge-index read guard (the graph tool adapters) wire it;
  without it the symbol providers keep the strict indexed-doc requirement.
  A provider needing edges for existence must degrade closed, not guess.
- A symbol's `defn_hash` IS the defining chunk's `chunk_hash`
  (`symbol_ref` in bbox-corpus-index project_files.rs) — current symbol
  refs were derivable from the retired `bbox_refactor_project_refs` MCP
  output without any search; the harness-side equivalent lives in the
  isolate `code.*` bindings. The eval refresh tooling depends on this
  equality.
