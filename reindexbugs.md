# Reindex Bugs

Observed incident: prod `blackbox.service` accepted TCP connections on
`127.0.0.1:7264`, but `/mcp` and `/roster` hung until the background full
rebuild completed. The May 8, 2026 rebuild ran from 09:03:17 to 09:17:44 MDT
and indexed 14,126 files / 1,206,715 docs.

Initial fixes in this pass:

- Automatic background full rebuilds are disabled by default. Set
  `BLACKBOX_BACKGROUND_FULL_REINDEX_TICKS=N` to opt back in.
- Full rebuilds no longer perform progress commits every 500 transcript files.
- Future full-rebuild project/git derived sidecars are written under managed
  `edges/derived/{project,git}/` paths and atomically replaced instead of
  appended.
- `bbox_edge_compact` dry-runs or applies one-project legacy sidecar compaction,
  removing parsed derived edges while retaining explicit/provenance/malformed
  lines and writing a backup before replacement.
- Full rebuilds no longer replay historical transcript tool-call edges into
  append-only sidecars.
- Background reindex now logs per-phase timings for transcript, project,
  knowledge/thread store-doc, purge, and commit phases.
- Startup EdgeIndex rebuild is deferred by default so HTTP binds before parsing
  legacy graph sidecars. Set `BLACKBOX_EDGE_INDEX_BOOT_REBUILD=1` to restore
  eager boot rebuild.

## 1. Automatic Full Rebuilds Run Too Often

`src/index/reindex.rs` forces `full=true` every 12 background ticks. Prod uses
the default `BLACKBOX_REINDEX_INTERVAL_SECS=120`, so a full rebuild is attempted
roughly every 24 minutes.

Impact:

- Predictable prod MCP brownouts.
- Large repeated rebuild cost even when only a few files changed.
- Repeated replay of expensive side effects: edge emission, git history scans,
  vector enqueues, Tantivy merges.

Fix:

- Disable automatic full rebuilds by default.
- Keep manual `bbox_reindex(full=true)` available for explicit maintenance.
- If automatic fulls are reintroduced, make cadence opt-in and measured in
  hours/days, not minutes.

## 2. Full Rebuild Is Not Just Transcript Reindex

The background full path deletes all Tantivy docs, then reindexes every domain:

- Claude/Codex transcript JSONL.
- Registered project files.
- Knowledge docs.
- Thread docs.
- Git commit docs.
- Graph sidecar edges.

Impact:

- The work volume is much larger than "transcript search index rebuild" implies.
- Project and git indexing run under the same maintenance pass as MCP serving.
- The rebuild has multiple slow subsystems and no per-domain timing, making
  bottlenecks hard to isolate from logs.

Fix:

- Split transcript, project-file, git, knowledge/thread, vector, and graph-edge
  maintenance into separately schedulable jobs.
- Add per-phase timing and counters.

## 3. Full Rebuild Commits Partial Indexes Every 500 Files

`index_directory_standalone` and `index_codex_directory_standalone` call
`writer.commit()` every 500 indexed files. During `full=true`, this commits the
initial `delete_all_documents()` plus a partial subset of rebuilt docs.

Impact:

- Search clients can observe an incomplete corpus during a full rebuild.
- Each intermediate commit increases segment churn and can trigger downstream
  watcher work.
- The nearby comment says full rebuild should atomically commit delete+adds at
  the end, but the shared indexing functions violate that during full rebuilds.

Fix:

- Suppress periodic commits when `full=true`, or pass an explicit commit policy
  into the standalone indexing functions.
- Only publish a full rebuild when all domains have completed successfully.

## 4. Derived Edge Sidecars Are Append-Only

`append_project_edges` appends derived project/git edges into
`~/.local/state/blackbox/edges/<project_id>.jsonl`. Full rebuilds emit the same
logical derived edges again, but do not replace or compact the prior copy.

Current prod sidecar size:

- `edges/*.jsonl`: about 13 GB.
- Total lines: about 26 million.
- Largest files: `146e1161.jsonl` and `ea0e29dd.jsonl`, each over 4 GB.

Impact:

- Every rebuild increases sidecar size.
- EdgeIndex rebuild has to parse an ever-growing JSONL corpus.
- Memory use stays high because the in-memory graph dedupes after reading, not
  before storing.
- Startup and graph refresh cost grow without bound.

Fix:

- Store generated derived edges separately from explicit/provenance edges.
- For full rebuilds, write derived sidecars to temp files and atomically replace
  per-project derived sidecars.
- Add a one-time compaction/migration for existing mixed legacy sidecars.

## 5. Explicit Tool-Call Edges Are Replayed During Reindex

Transcript indexing calls `ToolEdgeContext::emit_event_edges` for historical tool
calls. That appends `READ_FILE`, `EDITED_FILE`, and `RAN_BASH` sidecar edges
while replaying old transcript files.

Impact:

- Full transcript rebuild replays historical tool-call edges.
- Current code uses append-only writes for these edges.
- The target resolution reads and chunks the current target file for each file
  tool call, which is expensive and historically inaccurate when the file has
  changed since the transcript event.

Fix:

- Make transcript-derived tool edges idempotent.
- Avoid per-event current-file re-chunking in the hot rebuild path.
- Preserve historical anchor metadata separately from current chunk resolution.

## 6. Git Full Ingestion Shells Out Per Commit

Full git ingestion loads all commits, then calls `git diff-tree` once per commit
to derive `COMMIT_TOUCHED_FILE` edges.

Current registered repositories include about 17,301 commits, with `planglobal`
at about 14,359 commits.

Impact:

- Thousands of subprocesses during a full rebuild.
- Rebuild time is dominated by git process overhead for large histories.
- The work runs inside the prod daemon process that also serves MCP.

Fix:

- Use batched git plumbing where possible.
- Keep per-repo git indexing incremental by default.
- Do not force full git ingestion as a side effect of transcript full rebuild.

## 7. Rebuild Work Runs In-Process With MCP Serving

The background reindex thread, Tantivy writer/merge threads, vector enqueueing,
edge emission, edge-index refresh, and HTTP/MCP server all live in one
`blackboxd` process.

Impact:

- Heavy maintenance can starve unrelated routes like `/roster`.
- TCP can accept while request handling stalls, causing MCP clients to time out.
- There is no QoS boundary, no job cancellation, and no health gate for
  operator-facing routes.

Fix:

- Move full/offline maintenance out of the serving process, or run it with
  strict resource limits and explicit backpressure.
- Keep lightweight incremental indexing in-process only if it has bounded work.
- Add a cheap health endpoint that does not touch locks or heavy stores.

## 8. EdgeIndex Rebuild Competes With Reindex

The edge-index watcher polls Tantivy doc count and rebuilds the in-memory
EdgeIndex when the corpus grows. The rebuild reads sidecar edge files and
constructs forward/reverse maps.

Impact:

- EdgeIndex rebuild can overlap with reindex work.
- With 13 GB sidecars, graph refresh is itself a major maintenance job.
- Routes that inspect graph state can block on `edge_index` locks or suffer
  from memory pressure.

Fix:

- Trigger graph rebuild only after a committed maintenance epoch completes.
- Load from compacted/generated sidecars, not runaway append logs.
- Add graph rebuild timing and sidecar byte/line counters to logs.

## 9. Vector Queue Side Effects Are Coupled To Reindex

Project-file and git indexing enqueue embedding work while rebuilding.

Impact:

- Full rebuild can flood embedding queues.
- Missing provider keys cause repeated retry/drop noise.
- Vector queue pressure is unrelated to core MCP availability but shares the
  same daemon.

Fix:

- Make reindex enqueue behavior explicit and configurable.
- Avoid enqueueing unchanged entities during full rebuild if the content hash is
  already known.
- Separate vector maintenance from search-index publication.

## 10. Observability Is Too Coarse

Current logs show file/doc progress and final totals, but not enough phase
breakdown to explain a 14 minute run without external inspection.

Impact:

- Operators cannot tell whether time was spent in transcript parsing, project
  chunking, git diffing, edge sidecar writes, commits, merges, or graph refresh.
- The daemon looks "up" from systemd while operator-facing routes time out.

Fix:

- Emit per-phase timing.
- Emit sidecar size/line counts before graph rebuild.
- Emit request queue/latency warnings during maintenance.
- Surface a `maintenance_state` snapshot in a cheap route/tool.

## 11. Startup Blocks Before HTTP Bind

`main` rebuilt the in-memory `EdgeIndex` before constructing the HTTP listener.
With the legacy 13 GB edge sidecars, the process could be active under systemd
but not listening on `127.0.0.1:7264`.

Impact:

- `systemctl status` reports the daemon as running while MCP clients cannot
  connect.
- Deploy/restart can look successful but leave `/mcp` unavailable for the whole
  sidecar parse.
- Compaction endpoints cannot be reached until the bloated graph rebuild
  finishes, which blocks the obvious repair path.

Fix:

- Default boot to an empty `EdgeIndex` and bind HTTP first.
- Keep eager boot rebuild available only behind
  `BLACKBOX_EDGE_INDEX_BOOT_REBUILD=1`.
- Rebuild graph state explicitly after compaction or from a bounded background
  maintenance path.

## 12. Manual Full Reindex Still Blocks Serving

After disabling automatic background full rebuilds, an explicit
`bbox_reindex(full=true)` still ran synchronously inside the daemon. The May 8,
2026 manual run took about 202 seconds after the writer lock was acquired, and
`/roster` timed out during that window.

Impact:

- Operators can still self-inflict an MCP brownout with the manual tool.
- The HTTP listener is bound, but request handling can stall while the tool
  call performs CPU-heavy synchronous work.
- Manual maintenance has no progress handle, cancellation, or QoS boundary.

Fix:

- Run full reindex as an out-of-process or blocking job with a job id.
- Keep the MCP tool as a scheduler/status facade, not the worker itself.
- Leave cheap routes responsive during manual maintenance.

## 13. Startup Incremental Project Phase Can Stall For Minutes

On restart after compaction, the startup incremental reindex logged transcript
phase completion quickly, then project phase did not complete until vector
warmup finished about 259 seconds later.

Impact:

- The Tantivy writer lock stays held, so manual `bbox_reindex` gets
  `LockBusy`.
- Startup looks healthy from `/roster`, but maintenance is serialized behind a
  long project/vector path.
- Project phase timing is now visible, but the shared resource causing the
  delay still needs isolation.

Fix:

- Decouple vector warmup from project-file indexing.
- Avoid doing startup incremental indexing immediately when an operator is
  about to run explicit maintenance. Initial mitigation: the background
  reindex startup delay now defaults to the normal reindex interval instead of
  5 seconds, and can be overridden with
  `BLACKBOX_REINDEX_STARTUP_DELAY_SECS`.
- Add a maintenance-state route/tool that reports active phase and writer-lock
  holder.

## 14. Service Shutdown Does Not Stop Cleanly

Multiple `systemctl --user restart blackbox.service` attempts remained in
`deactivating (stop-sigterm)` until the exact unit was killed with SIGKILL.

Impact:

- Deploys hang while worker threads continue after SIGTERM.
- Operators need force-kill even for ordinary restarts.
- Long-running maintenance lacks cancellation points.

Fix:

- Install a shutdown token shared by background reindex, vector warmup, edge
  watcher, and orchestration workers.
- Make loops check the token and exit promptly.
- Join or detach known background workers deliberately during shutdown.
