# Search quality walk — autonomous deep dive

Captured 2026-05-06. Iterated end-to-end on search/graph quality against
two corpora: transcript-search itself (the local crate) and erlang-test
(an Elixir codebase, 264 .ex / 232 .exs / 139 .md). The user's framing:
"we're on a mission for SEARCH QUALITY" — not just defect-free, but
result quality at the AgenticTools donor's standard (97% recall vs 23%).

## Showcase queries

Three queries were used as the quality probe throughout:
- **"voyage error swallow body batch cap"** (transcript-search): conceptual
- **"recombination"** (erlang-test): single keyword, doc-heavy
- **"triad implementation"** (erlang-test): multi-term, both docs and code

Plus targeted symbol queries to validate the path-token boost:
- **"queue collect_quiescent_batch MAX_BATCH"** (transcript-search)
- **"triad_closure"** (erlang-test)

## Quality fixes shipped

Eight fixes landed across four commits during this walk.

### Retrieval & ranking (commit c53ba89, 53e9717)

| # | Fix | Effect |
|---|-----|--------|
| 1 | Per-file collapse — drop chunks past the first when (project_id, rel_path_hash) collide | Top-10 of "queue collect_quiescent_batch MAX_BATCH" went from 4× queue.rs + 1 doc to 1× queue.rs + 4 distinct files |
| 2 | Modal diversification — guarantee at least one each of {code_block, doc_section, git_message} when present in fetch | "triad implementation" top-10 now includes apps/substrate/lib/substrate/triad_closure.ex (was missing entirely; the file has "triad" in path but no "implementation" in body, so BM25 ranked it below all doc files) |
| 3 | Read-lock fast path — bbox_hybrid_search/bbox_search no longer take state.idx.write() just to check is_empty() | Searches don't block behind the auto-reindex writer (5-30s per cycle) |
| 4 | Tokenized path field — file_path STRING + new path_tokens TEXT field with code_tokenizer (splits on /, _, ., :, CamelCase) | "voyage" matches files literally named voyage.rs over arbitrary "voyage" mentions; symbol qualified-names also indexed there |
| 5 | Commit subjects mirrored into path_tokens — same boost as project_files | Commits compete on equal footing; commit b458289 ("voyage error swallow body batch cap") surfaces in top 10 |
| 6 | Path-tokens boost lowered to 1.5x (was 2.5x) and commit-subject parity added | Avoids over-promoting project_files at the cost of commits |

### Graph navigation (commit f170080)

| # | Fix | Effect |
|---|-----|--------|
| 7 | EdgeIndex.insert dedup keys on (source, kind, target, provenance, confidence) — metadata is logically same-edge but per-emission | Cut 21k duplicate edges from a 1.17M-edge corpus. find_paths from a transcript turn that wrote a file 7 times went from 7 identical paths to 1 path |
| 8 | Drop IN_FILE chunk[0]→chunk[0] self-loops + skip NEXT_CHUNK/PREV_CHUNK projection (NEXT_SECTION already covers that relationship via the chunker) | inspect_entity output is signal-only; no more "this chunk is in the file containing this chunk" noise |
| 9 | project_file.recommended_next_hops re-ordered — semantic edges (CONTAINS_SYMBOL, DEFINED_IN, CALLS, EDITED_FILE, READ_FILE, EDITED_BY_SESSION, EDITED_IN_COMMIT, COMMIT_TOUCHED_FILE) come ahead of structural fallback (IN_FILE, NEXT_SECTION) | discover_seed surfaces READ_FILE incoming on the test file (which transcript turns read it); the agent gets actionable provenance instead of structural noise |
| 10 | New families enumerated in expected_edge_families (READ_FILE, EDITED_FILE, NEXT_CHUNK, PREV_CHUNK, COMMIT_TOUCHED_FILE, DEFINED_IN) | edge_family_coverage now reports them as `present` when populated |
| 11 | select_notable_edges dedup on (kind, target) per direction | No more duplicate NEXT_SECTION entries pointing at the same chunk |

### find_paths quality (commit 07e6949)

| # | Fix | Effect |
|---|-----|--------|
| 12 | Per-file collapse on path-terminal project_file — fetch 8x then dedup so `limit` returns distinct files | Walk from a transcript turn that touched a 9-chunk markdown file went from 10 paths covering 3 files to 10 paths covering 8 files including STATUS.md (never surfaced in hybrid_search because individual chunks have low keyword density) |

## Validated end-to-end

Cold-start agentic walk on the question "How does triad_closure work and
what files document its implementation?":

1. **discover_seed_entities("triad closure convergence test")** returns
   correspondences-root-probe-c-implementation.md, the test exs file, and
   triad-implementation-sequence.md — all in top 3 with notable_edges
   showing READ_FILE provenance.
2. **inspect_entity** on the test file shows 2 READ_FILE incoming edges —
   transcript turns from session 0781c6a7 ("I need to locate the triad
   implementation sequence documentation"). Pre-fix this entity returned
   only IN_FILE self-loops.
3. **find_paths(from=transcript:claude:0781c6a7@3232721, to_type=project_file)**
   returns 10 paths covering 8 distinct files: triad_closure test exs,
   outward-observation-activation-probe-b-c-impl.md, activation-probe-b-c-arc.md,
   STATUS.md, design/ARCS.md, triad-implementation-sequence.md,
   correspondences-root-probe-c-implementation.md, correspondences-root-probe-c-arc.md.
4. **bundle_evidence(question, [entity_refs], [P1, P3, P4])** returns a
   structured bundle with content_previews for each — the agent has the
   actual elixir code (triad_closure.ex chunk 2 showing the
   `maybe_put_signal_and_triage` function), the design markdown
   (triad-implementation-sequence.md showing "Recombination Triad —
   implementation sequence"), the conceptual doc (recombination-triad.md
   showing "What recombination is, mechanism-first"), and the project
   STATUS log. All four needed to answer the question are present.

## Showcase query results (post-fix top-10)

### "recombination"
1. design/triad-implementation-sequence.md ✓ ("Recombination Triad — implementation sequence")
2. design/recombination-triad.md ✓
3. design/fleet-substrate.md ✓ (ground truth top 3)
4. design/correspondences-root-probe-c-implementation.md ✓ (ground truth #1 by mention count)
5. design/scouting/synthesis-r1.md
6. design/correspondences.md
7. design/research-direction-council.md (ground truth top 8)
8. design/scouting/claude-cross-pollination-r4.md
9. apps/substrate/test/substrate/decision_effect_test.exs ← diversified code_block
10. commit a9ad9f7 (git_message)

### "triad implementation"
1. design/triad-implementation-sequence.md ✓ (perfect title match)
2. design/recombination-triad.md ✓
3. design/primitive-synthesis-from-gaplog-implementation.md
4. design/correspondences-root-probe-c-implementation.md ✓ (ground truth #1)
5. design/dispatch-projection-probe-b-implementation.md
6. commit 400c55e
7. design/outward-observation-activation-implementation.md
8. commit 65235b6
9. commit b902156 (transcript-search agentic-corpus impl)
10. apps/substrate/lib/substrate/triad_closure.ex ✓ ← diversified code_block, the actual implementation

### "voyage error swallow body batch cap"
1. feedback/prod-validation-pass-1.md ✓ (documents the fix)
2. erlang-test/apps/substrate/lib/substrate/embedding/voyage.ex (cross-project keyword pollution — see open gap below)
3. src/embed/voyage.rs ✓ (the fixed wrapper)
4. erlang-test test/voyage.ex (pollution)
5. erlang-test/feedback/dreaming-arc.md (pollution)
6. knowledge:smoke-test (irrelevant)
7. erlang-test/design/vector-substrate-integration.md (pollution)
8. erlang-test/design/dispatch-projection.md (pollution)
9. commit b458289 ✓ (the fix commit, was missing pre-diversification)
10. erlang-test/feedback/calculus-arc.md (pollution)

## Quality gaps still open

1. **Cross-project keyword pollution.** erlang-test/voyage.ex outranks
   src/embed/voyage.rs for "voyage error swallow body batch cap" because
   both have "voyage" in path tokens AND erlang-test's adapter has more
   literal "voyage" mentions in body. No project filter is applied. The
   right fix: accept a `project_id` parameter on bbox_hybrid_search so the
   caller can scope (caller usually knows its project context). Soft-boost
   when query mentions repo-local file/path tokens that resolve only in one
   project. Harder: auto-detect from cwd of the MCP client.

2. **IN_FILE chunk[0] as file proxy is fundamentally awkward.** Currently
   chunk[0] of every file serves as the "file" entity, so navigating
   FROM a chunk to "the file" lands on chunk[0] (which then has its own
   IN_FILE self-loop, dropped at projection). Cleaner schema: introduce a
   `file:<project_id>:<rel_path_hash>` virtual entity that all chunks
   point to via IN_FILE. find_paths could then traverse file→chunk→symbol
   without going through chunk[0].

3. **STATUS.md still doesn't surface in hybrid_search top-10 for
   "recombination"** despite 21 mentions across the file. Each individual
   chunk has fewer mentions than top-ranked alternatives. Compensable by:
   (a) per-file BM25 score aggregation (not just per-chunk) before fusion,
   (b) the `bbox_describe_schema`-aware diversification could route a slot
   to "file with most query-term hits across all chunks."

4. **Single-token query "triad_closure" surfaces design doc above the
   .ex file.** Both have the symbol; the doc happens to mention it more
   in body. With a code-only filter the user could narrow, but the default
   should arguably prefer the symbol's defining file. Could weight
   `chunk_kind=code_block ∧ symbol contains query token` higher.

5. **bundle_evidence's intra_bundle_edges is empty** — it doesn't surface
   relationships BETWEEN bundled entities even when they exist in
   EdgeIndex (e.g., the triad_closure.ex code chunk and the design markdown
   both have COMMIT_TOUCHED_FILE edges from the same agentic-corpus impl
   commit). Filling this would close the agentic-loop nicely: the bundle
   would tell the agent "these four entities are connected via these N
   edges in your collected graph."

## Schema bumps in this walk

- `agentic-corpus-g2-path-tokens` — added the tokenized path_tokens field
- `agentic-corpus-g3-commit-subject-tokens` — populates path_tokens from
  the commit subject too

Each bump triggered an automatic reset + reindex on daemon restart
(~6 minutes for a 1.1M-doc corpus including the EdgeIndex full rebuild).
