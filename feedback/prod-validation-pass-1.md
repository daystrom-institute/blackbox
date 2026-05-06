# Prod validation pass — agentic-corpus on prod blackboxd

Captured 2026-05-06T09:18Z, prod blackboxd PID 1971586.
Start state: fresh release build + Voyage key drop-in installed, agentic-corpus-impl branch deployed.

## Bugs found and fixed inline (uncommitted, will land in next commit cycle)

### 1. `bbox_blame` returns "not found" for valid file/line input
`src/git.rs::git_root_for_path` passes the file path (post-canonicalize) directly
to `git -C <path>`. `git -C` requires a directory; for a file path it errors
silently → returns None → handler reports "no git blame data found" even though
direct `git blame -L N,N -- file` works fine.

**Reproduce**:
```
bbox_blame(file="/.../examples/agentic-corpus/brofiles/describe-symbol-fit.json", line=3)
→ {"status": "error.not_found", "message": "No git blame data found ..."}
```

**Fix**: take parent if `path.is_file()` before calling git. 6-line diff to
`src/git.rs`. Verified `git blame -L 3,3 examples/.../describe-symbol-fit.json`
returns commit `3b208202` cleanly.

### 2. Voyage error wrapper swallows HTTP status + body
`src/embed/voyage.rs` chained `.error_for_status().context("voyage embedding
request failed")?` consumed the response on error → all production logs read
"voyage embedding request failed" with no actionable detail (rate-limit?
token-limit? auth? bad batch?).

**Reproduce**: any failed batch — `journalctl --user -u blackbox.service | grep
"voyage embedding request failed"` shows the opaque message.

**Fix**: capture `response.status()` first; if non-success, read body as text and
bail with HTTP code + batch_size + 512-char body snippet. Now we can actually
diagnose voyage rejections.

### 3. Embed queue drains entire pending queue in single batch (no cap)
`src/embed/queue.rs::collect_quiescent_batch` ends with
`Some(pending.drain(..).collect())` — when 2278 chunks accumulate (e.g. after a
restart re-indexes), the next call sends ALL 2278 to Voyage in one request.

**Voyage limits**: 128 docs/req, ~120k tokens/req. 2278 chunks at 4-byte avg
token size = ~9M tokens. Voyage rejects → 3 retries, all reject → batch
DROPPED (lost forever, no re-enqueue).

**Reproduce in prod logs**:
```
03:12:27 WARN ... embedding batch dropped after retry limit dropped=2185
03:16:22 WARN ... embedding batch dropped after retry limit dropped=2579
```
**~4764 code chunks lost** in this validation window before fix.

**Fix**: cap at MAX_BATCH_DOCS=64 docs and MAX_BATCH_BYTES=80KB; remaining stays
in `pending` for next iteration. Conservative under both Voyage limits.

## Confirmed deferred-thread items (no fix this pass)

### 4. `include_vectors=false` ignored
H1 review #5 in deferred-thread. Reproduces exactly: response still carries the
full `vector_status { queues, partitions, searched_partitions }` block when
caller asks `include_vectors=false`.

### 5. `bbox_project_register` does not bootstrap
Deferred-thread Engine §5. Confirmed: registering
`/home/invidious/repos/erlang-test` (Elixir, 264 .ex / 232 .exs / 139 .md):
- ✅ Adds to project list
- ✅ Triggers EdgeIndex full rebuild (5998ms, projects all 1M+ docs)
- ❌ Does NOT walk source files and chunk them (BM25 index has 0 hits for any
  elixir-keyword query against erlang-test paths)
- ❌ Does NOT enqueue project-file chunks for embedding
- ❌ Does NOT walk git history (no COMMIT_PARENT / COMMIT_TOUCHED_FILE edges)
The "auto-reindex" tick that runs every 120s indexes transcripts only — that's
a separate pipeline.

### 6. KnowledgeEntry session_id → KNOWLEDGE_FROM_SESSION edge not projected
`bbox_inspect_entity(knowledge:1f584756)` returns zero edges across every
knowledge edge family. Design §4.1 + S2 expectations say
KNOWLEDGE_FROM_SESSION should auto-project from `KnowledgeEntry.session_id`.
Same upstream-projection-gap shape as #5 — the writing pipeline records the
property but the EdgeIndex projection step doesn't materialize the edge.

Discover_seed's `notable_edges: []` is therefore HONEST not buggy — there are
no edges to surface. The fix is upstream in the projection step.

### 7. Tree-sitter language pack ships with zero languages (BEING FIXED BY CODEX)
`Cargo.toml: tree-sitter-language-pack = { default-features = false }` strips
every grammar. `process()` always errors → forces direct-grammar fallback for
the 9 languages with explicit deps; everything else (elixir, erlang, ruby, etc.)
gets no AST chunking. Codex Task 2 in flight.

## Confirmed working

- **Voyage embed pipeline end-to-end**: Knowledge route smoke (1 entry written
  → embedded → queryable) succeeded. 412 git_message + 14 docs + 1 knowledge
  embedded into HNSW partition (max_level=2 → 4 as it grew, 33,478 neighbor
  refs at 521 nodes).
- **BM25 over transcripts**: returns 5+ relevant elixir-code excerpts from
  prior conversation transcripts.
- **`bbox_provenance_export`** writes correct git note for HEAD with full
  tool-call anchor chain (Write + Read + Edit ops with byte ranges +
  source_refs to transcript turns).
- **Schema introspection** (`bbox_describe_schema`): 12 entity types + 7 edge
  families correctly post-migration g1.
- **EdgeIndex >100k docs warning**: fired at startup (1.08M docs); system
  remained functional. Marker for the deferred S4 #2 streaming refactor.
- **Bad-input contracts**: `bbox_inspect_entity` and `bbox_find_paths` return
  structured `error.bad_input` with `suggested_fix` for malformed entity refs
  (per §4.4).
- **`bbox_project_register` idempotent**: re-registering same path returns
  same project_id without modifying registered_at (per tool docs).

## Not yet exercised

- `bbox_bundle_evidence` end-to-end
- `bbox_find_paths` with real source/target both populated
- `bbox_audit` against the F4 catalog packets
- Full agentic loop: discover → inspect → find_paths → bundle (cold-start
  agent unfamiliar with the codebase)
- Code-route embedding completion (blocked on bug #3 fix landing)
- AST chunking for elixir / erlang / etc (blocked on codex Task 2)
- bbox_blame on a project_file ref (blocked on bug #1 fix landing)

## Re-test list after codex commits land + rebuild + prod restart

In order:

1. `bbox_blame(file=..., line=3)` → expect full anchor chain (validates fix #1)
2. Watch journal for `voyage embedding request failed: HTTP 4xx body=...` if
   any batch fails (validates fix #2)
3. Watch code-route `indexed_count` rise from 93 toward 4500+ (validates fix #3)
4. Reindex erlang-test elixir source — first verify whether codex's Task 2
   chunker fix triggers an erlang-test indexing pass; if not, that's the
   bootstrap-arc gap (deferred #5) blocking the smoke test
5. After erlang-test source DOES get chunked, query for elixir-specific
   symbols (e.g. `defmodule GenServer`) and confirm `project_file:7c3dfb23:*`
   refs surface in the top results
