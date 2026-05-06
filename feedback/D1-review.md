# D1 + K1 fixes review

Commits `36462c4..6a2a9b6` (4 K1 fixes + D1 InspectableEntityProvider).

## Issues (fix-forward — URGENT, blocks D2 gates)

1. **Providers are scaffolding stubs; they don't load data from
   backing stores.** Concrete examples:
   - `KnowledgeProvider::get_entity(r)` returns a view with `id` as
     the ONLY property. It doesn't call `Knowledge::entry(id)` to load
     title, content, status, supersedes, or any other field.
   - `KnowledgeProvider::compact_label(r)` returns the truncated `id`,
     not the entry's title. The design said "knowledge → entry.title
     (≤80 chars)".
   - `ProjectFileProvider::get_entity` returns the ref components
     (project_id, rel_path_hash, chunk_hash, occurrence_idx) — no
     content, no chunk_kind, no language, no symbol.
   - `ProjectFileProvider::forward_edges` returns `Vec::new()` — never
     queries the EdgeIndex.
   - Same shape across all 12 providers (knowledge, project_file,
     transcript, session, thread, note, symbol, brofile, whiteboard,
     commit, virtual_task, virtual_bash_call).
   
   D2 will fail its gates ("Inspecting a knowledge entry returns
   properties + supersedes chain", etc.) against these stubs. Either:
   - **Recommend:** extend D1 in-place (or as a fast-follow `phase D1
     fix: load entities from backing stores` commit) so each provider
     actually reads from its store. This is the meat of D1; the trait
     scaffolding is only the contract.
   - Have D2 backfill — probably what'll happen organically when
     codex hits the failing gates. Acceptable but bundles two phases'
     work into D2.
   
   The trait + registry + dispatch IS correct and tested. The data
   loading is the missing half.

2. **Providers' `forward_edges(&self, r)` is unused** — even when
   data loading is added, this method conflicts with the EdgeIndex
   pattern. The EdgeIndex (S4) already projects all edges; providers
   shouldn't re-derive them. Either:
   - Drop `forward_edges` from the trait — D2's facade calls
     `EdgeIndex::forward_edges(r)` directly.
   - Repurpose `forward_edges` to expose provider-specific edges that
     aren't in EdgeIndex (none today; defer).
   The current trait has the method but every implementation returns
   empty. Dead surface.

3. **`recommended_next_hops` operates on the `Neighborhood` argument**
   passed in — which is fine if D2 populates it from EdgeIndex before
   calling. But the provider implementations as written ALSO never
   populate the entity's `neighborhood` field in `get_entity` (it's
   always `Neighborhood::default()` via `base_view`). So if a caller
   uses `entity.neighborhood`, they get empty. The contract is unclear:
   - Does `get_entity` populate neighborhood, or does the caller fill
     it before calling `recommended_next_hops`?
   - Looking at `next_hops()` helper, it operates on the
     `full_neighborhood` arg, not entity's. So caller-fills-from-EdgeIndex
     pattern. Document this contract on the trait.

## Concerns

4. **Virtual providers (`virtual_task`, `virtual_bash_call`) need a
   resolution path**. The design says virtual entities resolve to
   their materialized backing (transcript event for bash_call, session
   for task). Today they probably look like the others — return ref
   components as properties without resolution. Verify after the
   data-loading fix lands.

5. **`schema()` returns hardcoded vectors of property/edge/filter
   names per provider.** Today these are static; eventually they
   should reflect what's actually in tantivy (a knowledge entry that
   has no `rationale` shouldn't list it as a property). Defer; flag.

6. **`#![allow(dead_code)]` at the module top.** Same pattern as
   F3/S4 — the surface is built ahead of consumers (D2). Track for
   removal once D2 lands.

## K1 fix observations

7. **K1 fix #1 (writer-per-call doc)** — comment landed. Acceptable
   deferred-improvement marker.

8. **K1 fix #2 (index superseded knowledge)** — `indexable_knowledge_entry`
   now includes `Status::Active` AND `Status::Superseded`. History
   queries can find old entries. ✓

9. **K1 fix #3 (best-effort index sync)** — `sync_knowledge_entry_to_index`
   wrapped with warning log on error; MCP returns ok regardless. Half-success
   problem resolved. ✓

10. **K1 fix #4 (keep knowledge out of account filters)** — `account`
    and `role` fields no longer populated for knowledge docs.
    Documentation comment in `build_knowledge_doc` explains the
    contract. ✓

## Nits

11. **`provider_for` returns `&'static dyn` via `OnceLock` registry.**
    Clean. But `expect("provider registry must cover every EntityType")`
    panics if a new entity type is added without updating the registry.
    Add a compile-time check via `EntityType::ALL.iter().for_each(provider_for)`
    in a unit test. Already done in `registry_covers_entity_type_enum`. ✓

12. **`base_view` returns a view with `Neighborhood::default()`.**
    Subjective: name it `empty_neighborhood_view` or document that
    it's for the trait scaffold only and doesn't populate
    neighborhood (caller's responsibility).

13. **`truncate_label` truncates by char-len ≤80** but may produce a
    label that's <80 chars depending on UTF-8 boundaries. Document
    "max 80 bytes, may end mid-word." Subjective.
