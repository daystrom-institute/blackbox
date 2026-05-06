# H2 + F2b fixes review

Commits `3d7057e..ae8cfca` (2 F2b fixes + H2 discover_seed_entities).

## Issues (fix-forward)

1. **`hybrid_search::hybrid_search` is invoked as a JSON round-trip.**
   `discover_seed_entities` calls hybrid_search → gets back a JSON
   string → `serde_json::from_str` → walks `Value::as_array` /
   `Value::get("vector_status")`. Two unnecessary serde passes per
   query plus runtime field-name dependence (any rename in
   hybrid_search response breaks discover_seed silently). Refactor:
   expose hybrid_search's typed response struct (or split into
   `hybrid_search_typed` returning `HybridSearchResponse` + a thin
   JSON-rendering wrapper). Then discover_seed consumes the typed
   shape directly.

2. **`render_text` calls `seeds.iter().position(...)` for each seed**
   to compute its 1-indexed rank — O(n²) per render. With default
   limit=8 that's 64 lookups; with max_limit=30 it's 900. Use
   `.iter().enumerate()` on the outer loop:
   ```rust
   for (idx, seed) in seeds.iter().enumerate() {
       out.push_str(&format!("{}. {} — ...", idx + 1, seed.label, ...));
   }
   ```

## Concerns

3. **`notable_edges` calls `entity_loader::load(ctx, entity_ref)` for
   each seed.** Default 8 seeds = 8 entity loads per discover_seed
   call. After fix #1 (when hybrid_search exposes typed response),
   the entity properties could be threaded through so
   discover_seed reuses what hybrid_search already loaded. Same
   pattern as D2 fix #2: shared entity loading across the agentic
   surface.

4. **`select_notable_edges` has two passes** — first by priority
   order, second to fill up to limit with anything in priority_set.
   The second pass uses the SAME priority_set so it just re-picks
   from the same set. Unclear what it adds beyond the first pass.
   Probably dead code; flag.

5. **`match_source` classification is BM25 / vector / hybrid based
   on hybrid_search's `sources` map keys.** The keys are
   `"bm25"` and `"vector:<route_id>"`. If hybrid_search's source
   naming changes (e.g. adds new source types like `"reranker"`),
   this classifier needs updating. Couple it to a const list.

## F2b fix observations

6. **F2b fix #1 (pass strictness)** — `PassStrictness::{Any | All
   | First}` enum added; `expected_ref_check` honors the field.
   `cross_modal_*` and `stale_decision_*` queries now require ALL
   expected refs; transcript_provenance requires the first ref;
   others default to Any. ✓

7. **F2b fix #2 (cache checker manifests)** — `OnceLock<Vec<EvalQueryManifest>>`
   pattern; first call loads, subsequent calls return cached. 90
   checker invocations parse manifests once. ✓

## Nits

8. **`render_text` includes the full `entity_ref` on each seed
   line** alongside the label. For transcript/project_file refs
   (long, multi-component), this clutters the output. Consider
   `<label>` only on the first line, with `entity_ref` on a
   following indented line if useful. Subjective.

9. **`PER_DIRECTION_EDGE_LIMIT = 2`** is hardcoded. Daystrom spike
   used a similar small number. Make it a config constant if
   operators want richer previews.

10. **`empty_neighborhood_view` is called as a fallback when
    entity_loader::load fails.** This means a `bbox_discover_seed_entities`
    call against a corpus where some entity refs are unresolvable
    won't fail — the seed gets included with empty notable_edges.
    Good degradation; document the contract.
