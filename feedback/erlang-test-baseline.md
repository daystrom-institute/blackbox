# erlang-test smoke — pre-AST-fix baseline

Captured 2026-05-06T09:10Z, prod blackboxd PID 1971586, post-Voyage-key drop-in.
Repo: /home/invidious/repos/erlang-test (Elixir, despite the name).
Source inventory: 264 .ex / 232 .exs / 139 .md (zero .erl).

## Pipeline state

| Route        | indexed | queued | retried |
|--------------|---------|--------|---------|
| git_message  | 412     | 0      | 0       |
| docs         | 14      | 0      | 0       |
| knowledge    | 1       | 0      | 0       |
| code         | 0       | 2278   | 2       |
| notes        | 0       | 0      | 0       |
| transcripts  | 0       | 0      | 0       |

HNSW partition `voyage-voyage-code-3-1024-0103683e`: 427 active nodes, max_level=2,
27,400 neighbor refs, 2 rebuilds (one of those was the schema-migration restart).

Code-route queue depth = 2278 BUT `indexed_count = 0`. None of the 2278 chunks are
from erlang-test — they're transcript-search project_files queued at startup that
voyage hasn't drained yet (rate-limit on a single concurrent batch worker).

## What works on the live daemon

1. **Vector search over commit messages** — `bbox_hybrid_search("supervisor restart strategy")`
   returns 10 erlang-test commits ranked by voyage cosine, confirming the Voyage key
   drop-in landed and embeddings dispatch end-to-end.
2. **BM25 over transcripts** — `bbox_hybrid_search("defmodule GenServer handle_call",
   vector_weight=0)` returns 5 transcript hits showing prior-conversation excerpts of
   elixir code. Useful indirect signal but not the primary surface.
3. **Schema introspection** — `bbox_describe_schema` reports 12 entity types + 7 edge
   families correctly post-migration g1.

## What's broken or absent

1. **No commit edges.** `bbox_inspect_entity(commit:d7a484df:da8b2fd...)` returns
   `edges: { in: [], out: [] }`. Schema marks `COMMIT_PARENT` /
   `COMMIT_TOUCHED_FILE` / `COMMIT_PRODUCED_BY_ARC` as "optional" so the inspector is
   happy, but in practice every non-root commit should have at least one parent.
   Conclusion: **git_history indexing pipeline did not run for erlang-test on
   `bbox_project_register`**. Journal contains zero `git_history` log lines since
   the project was registered.

2. **`notable_edges: []` on every discover_seed result.** Design §4.1 says seeds
   surface 1-3 notable edges for orientation. Today the field is always empty for
   commit entities — direct consequence of #1.

3. **No `project_file:*` BM25 hits for elixir queries.** Erlang-test's 496 source
   files are not in the BM25 index either. The auto-reindex thread reports "indexed
   5 files (18646 docs)" at 03:09:45 but that was the TRANSCRIPT reindex pass — the
   project-file ingestion is a separate pipeline that registration does NOT trigger.

4. **`include_vectors=false` ignored.** The response still carries the full
   `vector_status { queues, partitions, searched_partitions }` block. Reproduces
   the H1 review #5 deferred item exactly.

5. **`bbox_project_register` doesn't bootstrap.** All it does:
   - Add to project list (`bbox_project_list`)
   - Trigger an EdgeIndex full rebuild (5998ms, projects all 1M+ docs from scratch)
   It does NOT:
   - Walk `.ex`/`.exs`/`.md` files and chunk them
   - Walk git history and emit COMMIT_PARENT / COMMIT_TOUCHED_FILE
   - Enqueue project-file chunks for embedding
   The deferred-thread already tracks this as "auto-trigger of bootstrap-arc" gap.

6. **AST coverage is zero for elixir.** `language_for_path()` in
   `src/chunker/code.rs:105` returns None for `.ex` and `.exs`. Even when the
   `tree-sitter-language-pack` features land (codex's task 2), `language_for_path`
   still needs to be extended for elixir to opt into AST chunking. Without that,
   all 496 source files fall through to the markdown/text chunker.

7. **`tree-sitter-language-pack` ships with zero languages.** Confirmed by journal:
   `Language 'rust' not found`, `Language 'python' not found`. The
   `default-features = false` flag in Cargo.toml strips every grammar; the direct-
   crate fallback table in `parser_for_language()` saves rust/python/etc but only
   for the 9 languages with explicit deps. Codex is fixing this now.

## What to re-test after codex lands

- After Cargo.toml feature fix: `language_for_path("foo.ex")` should still return
  None until extended — the code change has to extend BOTH the pack features AND
  the path→language map. Verify codex did both.
- After `bbox_reembed` E3 implementation: trigger
  `bbox_reembed(route="code")` then watch erlang-test elixir chunks appear in
  hybrid_search results (assuming chunking + indexing also fires; if reembed only
  re-fills the queue from already-indexed entities, this won't help erlang-test).
- The deferred bootstrap-arc gap is the more fundamental problem: until project
  register triggers code chunking + git history indexing, registering a new repo
  is mostly useless.
