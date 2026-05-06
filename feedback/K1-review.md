# K1 + S3 fixes review

Commits `535f8da..a19a3a6` (4 S3 fixes + K1 knowledge entry indexing).

## Issues (fix-forward)

1. **`upsert_knowledge_entry` opens a fresh writer + commits per call.**
   `index.writer(50_000_000)?` then `writer.commit()` on every
   `bbox_learn` / `bbox_remember` / `bbox_decide` MCP call. Tantivy
   commits flush segments to disk; each commit is several ms even for
   tiny writes. A burst of 50 knowledge writes = 50 segment flushes +
   50 reader-reload cycles. Either:
   - Share a single long-lived writer in `SharedState` (with mutex);
     commit on a debounce timer.
   - Hand off knowledge writes to the existing reindex thread via a
     channel; that thread already has commit batching.
   Defer unless the daemon shows the load; flag in done note.

2. **`indexable_knowledge_entry` filters out `Status::Superseded` entries.**
   So a query like "what was the first decision about Postgres
   consolidation?" can't find the SUPERSEDED original — only the
   replacement. The supersession chain edges (S4 wired) point at
   entities that aren't searchable. Either:
   - Index ALL entries (including superseded) and rely on type-aware
     rerank to deprioritize them (per design §10.2 Approval multipliers
     don't currently address supersession; would need extension).
   - Index superseded entries with a flag that the rerank uses to
     downweight.
   - Document explicitly that `bbox_search` won't return superseded
     knowledge; hybrid_search (H1) will need to expose them differently.
   Pick one.

3. **Knowledge mutation hooks don't verify the index write succeeded
   before returning success to the MCP caller.** Looking at
   `bbox_learn` / `bbox_remember` / `bbox_decide` flow: the knowledge
   store write happens first, then `sync_knowledge_entry_to_index` is
   called. If the index write fails, the function returns an Err, but
   the JSON store ALREADY committed the entry. So the user sees an
   error but the knowledge IS persisted (just not indexed). Either:
   - Roll back the knowledge store write on index failure (transaction
     semantics — hard).
   - Surface the partial failure clearly: "knowledge committed; index
     update failed; will retry on next reindex cycle."
   - Wrap the index-write in a try and log warning rather than
     propagate (so MCP returns ok even on transient index errors).
   The current behavior is "all-or-nothing-MCP-error but actually-half".

## Concerns

4. **`embed_queue.rs` is two no-op functions hardcoded to the
   knowledge route.** Acceptable as a stub. When E2 lands the real
   queue, the API will need to grow per-route dispatch
   (`enqueue(route, entity_id, content)`). Flag the refactor in
   release notes.

5. **`reindex_knowledge_store_standalone` deletes ALL knowledge docs
   then re-adds.** Heavy-handed on bootstrap (~ms per entry) but
   correct. For incremental updates the per-entry path is used. OK
   pattern; flag if knowledge corpus grows past ~10k entries.

6. **The MCP tool descriptions for `bbox_learn`/`bbox_remember`/
   `bbox_decide` don't mention that writes now sync to the search
   index.** Behavioral change worth surfacing: agents calling these
   tools should know their entries become searchable immediately.
   Update tool descriptions in a follow-up commit (low priority; flag
   for the next leapfrog cycle).

## S3 fix observations

7. **S3 fix #1 (remove phantom symbol edges)** — landed cleanly.
   `derive_has_field_edges` and `derive_impl_trait_edges` now skip
   when target doesn't resolve through the symbol table. `IMPORTS`
   edges dropped entirely from this phase. ✓

8. **S3 fix #2 (code tokenizer)** — solid implementation.
   `CodeTokenizer` splits on `_`, `:`, `.`, `>` AND camelCase
   boundaries while keeping originals + lowercase variants. The test
   covers `KnowledgeStore` → `[KnowledgeStore, Knowledge, Store]` and
   `bbox_project_register` → `[bbox_project_register, bbox, project,
   register]`. Registered on `code_content` field via
   `with_tokenizer("code")`. ✓

9. **S3 fix #3 (call keywords audit)** — verify the keyword list now
   covers the full flow-control set. (Not yet inspected; flag if
   incomplete.)

10. **S3 fix #4 (warn on language-pack fallback)** — uses a per-language
    once-only warn pattern. Diagnostic visibility improved. ✓

## Nits

11. **`knowledge_entity_id(entry_id)` constructs an EntityRef, calls
    `.to_string()`.** Could be a one-liner that just emits `format!("knowledge:{entry_id}")`
    avoiding the EntityRef construction overhead. Subjective; the
    current form is more grammar-correct (uses the canonical
    constructor).

12. **`build_knowledge_doc` puts `account: "knowledge"` and
    `role: "knowledge"`.** Reusing fields meant for transcripts
    (account = which Claude account, role = user/assistant) for a
    different doc_type. Search filters by account/role would mistake
    knowledge entries for transcript events. Either:
    - Skip these fields entirely for knowledge docs (don't add).
    - Use distinct sentinel values like `account: ""` and `role: ""`.
    Document the filter contract: "queries scoped to account/role
    only apply to transcript docs."

13. **`indexable_knowledge_entry` is `pub(crate)`** but only used in
    `knowledge_docs.rs`. Could be private. Subjective.
