# erlang-test smoke — post-AST-fix daemon

Captured 2026-05-06T09:28Z, prod blackboxd PID 2039143.
Daemon: codex Tasks 1+2 + my 3 inline fixes (commit b458289).

## Pipeline state

| Route        | indexed | queued | retried |
|--------------|---------|--------|---------|
| code         | 516     | 7423   | 0       |
| docs         | 0       | 0      | 0       |
| git_message  | 2       | 0      | 0       |
| knowledge    | 0       | 0      | 0       |
| notes        | 0       | 0      | 0       |
| transcripts  | 0       | 0      | 0       |

Code route healthy: **0 retries, no batch drops** since restart. The
`MAX_BATCH_DOCS=64` cap (commit b458289) keeps voyage requests well under the
128-doc / ~120k-token limits — code chunks now flow through cleanly.

HNSW partition `voyage-voyage-code-3-1024-0103683e`: 1326 active nodes (up
from 521), max_level=4, 85,506 neighbor refs, 1 rebuild post-restart.

## What works

### 1. AST chunking for elixir via tree-sitter-language-pack ✅

Codex's Task 2 (Cargo.toml `default-features` enabled + `language_for_path`
extended to .ex/.exs) produces `chunk_kind: code_block` chunks for elixir
source. Auto-reindex tick at 03:23:03 indexed 514 files (19,933 docs) — avg
**38 chunks per file**, consistent with proper AST symbol-level chunking
(not raw-text fallback).

### 2. erlang-test BM25 surfaces real .ex source ✅

`bbox_hybrid_search("defmodule GenServer handle_call", vector_weight=0,
doc_type=project_file)` returns:

| Rank | File | Excerpt |
|------|------|---------|
| 2 | apps/witness/lib/witness/authority.ex | `defmodule Witness.Authority do ... use GenServer` |
| 3 | apps/substrate/lib/substrate/projected_topology.ex | `GenServer.call(server(), {:delete, ...})` |
| 4 | apps/witness/lib/witness/registry.ex | `defmodule Witness.Registry ... @table :witness_known_good` |
| 5 | design/recovery-floor-implementation.md | `GenServer.call(...) ... :rpc.call` (doc_section) |

Both `code_block` and `doc_section` chunk_kinds are populated. Doc-rich repo
(264 .ex / 232 .exs / 139 .md) covered across both source and design docs.

### 3. bbox_reembed (Task 1) actually works ✅

`bbox_reembed(route="code")` returned `{status: "ok", message: "rebuild
started: 7828 entities enqueued"}`. Queue depth jumped from ~575 to 7423
post-call (consistent with re-enqueue of all known code entities). Drained
516 in ~5 minutes with zero errors.

### 4. bbox_blame git -C fix (commit b458289) ✅

`bbox_blame(file=examples/.../describe-symbol-fit.json, line=3)` now returns:
```json
{
  "git_blame": {"author": "Mathieu Roy", "commit_sha": "3b20820...", ...},
  "bbox_anchor": null,
  "text": "... was last edited by: commit 3b20820 by Mathieu Roy [no bbox-tracked tool call matches this commit]"
}
```
vs the pre-fix `error.not_found`. The anchor is null because that file was
authored before any P1 anchors were tracked — expected.

## What's broken or absent

### 5. Edge projection systemically missing across entity types ❌

`bbox_inspect_entity(project_file:7c3dfb23:c0f9147c:...)` for the elixir file
returns ZERO edges. Schema lists 10 edge families for project_file:
`CALLS`, `CALLED_BY`, `CONTAINS_SYMBOL`, `IN_FILE`, `EDITED_BY_SESSION`,
`EDITED_IN_COMMIT`, `NEXT_SECTION`, `LINKS_TO_FILE`, `LINKS_TO_SECTION`,
`DESCRIBES`. All present at count=0.

`IN_FILE` is marked `expected: required`. **Required edge family is at
count=0 but the status string says "0 (expected)" instead of flagging a
violation.** This is a formatter bug in `bbox_inspect_entity` — it should
distinguish required-but-zero from optional-and-zero.

The systemic root: project-file chunks are written into tantivy (BM25 query
finds them) and routed to vectors (HNSW partition contains 1326 vectors),
but the EdgeIndex projection step that should derive structural edges from
these tantivy docs into EdgeIndex is missing or under-running. Same
gap-shape we see for:

- KNOWLEDGE_FROM_SESSION on knowledge entries (validation-pass-1 finding #6)
- COMMIT_PARENT / COMMIT_TOUCHED_FILE on commit entries (#5)
- IN_FILE / CONTAINS_SYMBOL / CALLS on project_file chunks (THIS finding)

Once the projection step lands, `bbox_discover_seed_entities` populates
`notable_edges` automatically, `bbox_find_paths` actually finds paths,
`bbox_inspect_entity` shows graph structure — the agentic surface becomes
useful instead of returning empty graphs.

### 6. project-bootstrap-arc workflow is a stub ❌ (deferred-thread Engine §5 root cause)

Auto-trigger plumbing exists (main.rs:6712), workflow file exists at
`examples/agentic-corpus/workflows/project-bootstrap-arc.json`, but the
workflow is a 7-node skeleton where each node just does `set_var(...=true)`
and goes to next. ZERO real ops. No `index_project_files`, no
`walk_repo_history`, no `enqueue_chunks_for_embed`.

Practical effect: `bbox_project_register` triggers a workflow that succeeds
without doing anything. The reason erlang-test got indexed at all was the
auto-reindex thread's 120s tick, NOT the bootstrap arc.

The chunking pipeline IS correctly wired into the auto-reindex path
(`scan_registered_project_files` → `index_registered_projects_standalone`),
so things eventually work. But registering then immediately querying yields
nothing for ~2 minutes until the next reindex tick.

Two fixes possible:
- (a) Wire actual ops into the bootstrap arc workflow (`op:
  index_project_files`) and have it run synchronously on register.
- (b) Drop the workflow and have `bbox_project_register` fire the reindex
  thread directly (single-call rather than scheduled tick).

(b) is simpler; (a) is more bbox-idiomatic.

### 7. include_vectors=false still ignored (deferred-thread H1 #5) ❌

Reproduces. Response carries full vector_status block when caller asked
include_vectors=false. Unchanged from validation-pass-1.

## Re-test list (next pass)

- Implement EdgeIndex projection from project_file chunks → IN_FILE +
  CONTAINS_SYMBOL edges. Then re-query and confirm
  `bbox_inspect_entity(elixir_file)` shows real edges.
- Fix `bbox_inspect_entity` formatter to flag `required` + `count=0` as
  violation (status should not say "(expected)").
- Fix `include_vectors=false` to actually omit vector_status.
- Wire the bootstrap arc to real ops OR drop it for direct reindex trigger.
- Run a full agentic loop end-to-end: discover → inspect → find_paths →
  bundle (against a query that requires graph traversal). Currently impossible
  because all entity inspection returns no edges.
