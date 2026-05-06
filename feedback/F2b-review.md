# F2b + E2 fixes review

Commits `4c54fd7..bd2820e` (4 E2 fixes + F2b eval gates resolved).

## Issues (fix-forward)

1. **Eval check pass-criterion is "any expected ref present in
   collected".** That's a low bar — agentic answers that find ONE
   of N expected entities pass even if they miss the others.
   `expected_ref_check`:
   ```rust
   let matched = expected.iter().any(|expected| collected.contains(expected));
   ```
   Per design §16.3 the gate is "agentic must hit ≥27/30, no class
   <5/6". The shipped check makes it easy to game: as long as
   collected covers ANY expected entity, any agentic answer
   passes. Refine per query class:
   - `cross_modal_code_prose`: require ALL expected refs (since the
     point is "code AND design context")
   - `stale_decision_lookup`: require both old + new entities for
     supersession queries
   - `exact_symbol`: any-match is fine
   - `conceptual_design_doc`: any-match acceptable
   - `transcript_provenance`: require the specific transcript ref
   Defer the per-class refinement to H3 (eval harness binary) when
   the actual harness shape is being designed; for F2b the any-match
   baseline is acceptable as a gate for "entity found at all."

2. **`expected_refs_for_checker` calls `load_manifests()` from
   inside every checker invocation.** O(n) over all 30 manifests
   per pass-check call, and it re-parses all 30 JSONs. For a 30×3
   strategy eval that's 90 calls = 2700 manifest parses. Cache the
   manifests once at startup or at first checker invocation.

## Concerns

3. **`expected_entity_refs` are baked into JSON manifests** — they
   reference specific chunk_hashes / defn_hashes / commit SHAs in
   the current corpus. If the corpus changes (transcripts grow,
   docs are edited), the expected refs become stale. F2b's oracle
   baseline is a snapshot. Operator workflow when the corpus
   drifts:
   - Re-run the manifest resolver against the live corpus
   - Compare new expected_refs vs old; surface diffs for review
   - Update only entries where the drift is intentional (e.g. a
     decision was superseded)
   Add a script `eval/scripts/refresh_expected_refs.sh` that walks
   manifests, re-resolves locators against live tantivy, and emits
   a diff. Defer the script to H3 but flag here.

4. **`transcript_ref_matches_locator` uses `transcript_hint`** as
   a content phrase to find the matching transcript event. The
   resolver searches for the phrase — but tantivy has multiple
   matching events for common phrases. The resolver may pick a
   non-target event. Verify on at least one transcript-provenance
   query that the resolved ref points at the intended event, not
   just any event with the phrase.

## E2 fix observations

5. **E2 fix #1 (WAL-backed dedup)** — replaced in-memory
   `seen_hashes` HashSet with WAL lookup. `should_embed` consults
   the vector store's WAL records. Memory leak fixed. ✓

6. **E2 fix #2 (cap retry head-of-line)** — retry budget capped
   at N attempts; on exhaustion, batch is dropped, error surfaced
   in `last_error`, queue resumes processing. `retried_count`
   counter exposed in RouteStatus. ✓

7. **E2 fix #3 (token-bucket rate limiter)** — proper token bucket
   counting input items, not batches. Per-route bucket. ✓

8. **E2 fix #4 (tombstone via WAL)** — `EmbedQueueHandle::tombstone`
   now calls `vectors::delete_entity_all_routes(entity_id)`. Knowledge
   `bbox_forget` actually removes vector. Test verifies. ✓

## Nits

9. **`truncate_locator_description`** uses `.chars()` for
   utf8-aware truncation. ✓ Note: assumes single-codepoint chars
   are <= 80 grapheme-equivalent units; fine for ASCII corpus.

10. **`expected_ref_resolves`** at the boundary of locator+ref —
    walks the manifest's locators and asserts at least one matches
    the ref. Good gate for the resolver's correctness.

11. **`stub_checker!` macro reused unchanged from F2a** — all 30
    checkers funnel into `default_stub_check` → `expected_ref_check`.
    F2b doesn't introduce per-class checker logic; that's H3's job.
