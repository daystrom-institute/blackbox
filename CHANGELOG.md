# Changelog

All notable changes to Blackbox are documented here.

This project uses Semantic Versioning. Until the public API and operator
workflows stabilize, `0.y.z` releases may include breaking changes; call those
out explicitly under `Changed` or `Removed`.

## Unreleased

### Added

- Strict catalog Git transport cutover: the offline
  `project-catalog git-transport-cutover` workflow now supports checksummed
  preflight, apply, verify, and row-preserving re-cutover artifacts. Covered
  Published repositories fail closed on stale membership or producer
  authority, suppress checkout-backed history and provenance fallback, retain
  last-good data, expose named health states, and preserve unrelated marker
  rows. Bridge and never-covered/LegacyLocal adapter paths remain available.
- Durable project catalog, phases 1-6 (code-complete): corpus project
  identity is path-free and catalog-owned, checkout access is gated behind
  observable capability leases, and collected code survives without a
  local checkout. The offline cut machinery ships as
  `blackbox project-catalog` subcommands: `migrate` gains
  `--apply --configured` for the real-store cut, `verify` gains
  `--require-exclusive-availability` (the bridge-down proof), and two new
  verbs land with a preflight/apply/verify mode triple:
  `durable-backfill` (stamps the stable project id across the 14
  path-keyed durable-store owners from the migration's ledger, publisher
  dispositions sourced from the installed migration marker) and
  `path-free-rebuild` (drives the operator-triggered same-schema index
  replacement to a committed, fully verified rebuild manifest). Startup
  gates refuse unverified rebuilt history on migrated stores;
  marker-driven GC exclusion protects every rollback asset. The
  configured store stays version-1 bridge mode until the operational cut
  runs (design/daemon-runtime/durable-project-catalog-impl.md).

### Removed

- The daemon's refactor, slice, code-navigation, and macro MCP surface is
  retired (29 tools): `bbox_refactor_status`, `bbox_refactor_project_refs`,
  `bbox_refactor_plan_kinds`, `bbox_refactor_plan`, `bbox_refactor_apply`,
  `bbox_refactor_run`, the six `bbox_slice_*` tools, the eight
  `bbox_code_*` / `bbox_workspace_symbols` code-nav tools, and the eight
  `macro_*` tools. Refactor tooling is harness-native: the bro-harness
  `isolate` and its in-box bindings (`code.*`, `java.*`, `edits.*`,
  `analysis.*`, `lsp.*`) link the same engine crates with no daemon
  reach-back (design/bro-harness/refactor-tools-v2.md §6-§7). MCP-only
  consumers direct refactoring via `bro_exec` / `bro_resume` orchestration
  or canned atoms. The `bbox-macros` and `bbox-code-nav` crates and the
  `bbox-indexing` slice/code-nav modules are deleted along with the
  adapters; `bbox-refactor` and `bbox-lsp` survive as libraries for the
  harness bindings.
- The daemon no longer injects the `DaemonRefactor` capability into
  in-process harness sessions: the flat `refactor_plan` /
  `refactor_plan_get` tools and the `RefactorCapability` contract-bottom
  trait are gone. Harness sessions use the isolate bindings only.
- The daemon's warm LSP session pool (`lsp_sessions`) is removed with its
  last consumers; the `BLACKBOX_LSP_IDLE_SECS`, `BLACKBOX_JDTLS_*`, and
  `BLACKBOX_RUST_ANALYZER_*` env vars are inert for the daemon (the config
  schema still parses them for compatibility). Harness-side LSP lives in
  `bro-lsp`.
- Dispatch-time `project_dir` argument defaulting for the retired code-nav
  tools (`CODE_NAV_PROJECT_DIR_DEFAULT_TOOLS`) is removed; the
  `BRO_HARNESS_PIN_TOOLS` default pin pattern (`bbox_slice_*`) is now empty.
- The `readonly` MCP surface packet no longer allows the retired code-nav
  tools, and brofile allowlists across `system-defaults/brofiles/` and
  `.bbox/brofiles/` no longer name the retired tools. The four refactor
  persona brofiles (`{rust,java,elixir,csharp}-refactor-persona`) are
  re-pointed at the harness-native path: their lenses describe the cell
  bindings, and their allowlists open `exec`/`wait` plus the binding
  namespaces each language supports (java: `java.*`/`analysis.*`/`lsp.*`;
  rust: `lsp.*`; elixir/csharp: `code.*` facts + `edits.*` only). All
  mutation goes through the `edits.*` choke point with hash + parse
  validation at `edits.apply`; Write/Edit/Bash stay denied, and
  `build.gate` is deliberately off the persona surface because it accepts
  arbitrary shell commands (compile-level validation is the
  orchestrator's step). Review and pathology panel lenses ground in the
  retained evidence tools (hybrid search, knowledge, blame, Read/Grep/Glob)
  instead of the retired code-nav tools.
- `deploy/blackbox-java-worker` (the OpenRewrite sidecar whose only client
  was the deleted `bbox-macros` sidecar protocol) is removed; it is
  recoverable from git history if the sidecar is ported harness-side.

### Fixed

- Isolated daemons can no longer publish a partial knowledge store into the
  production global guidance files. Every daemon now claims its four resolved
  global render targets against other daemon instances, and `scope=global`
  refuses any implicit host-default target when the knowledge source is
  non-default. An intentional non-default source-to-host binding must name the
  target explicitly.
- Global guidance renders now fail closed when an incomplete source view would
  replace a substantial managed region with less than half its content. The
  guard catches nonempty bootstrap stubs as well as empty renders before any
  backup or atomic replacement can touch `~/.blackbox/BLACKBOX.md` or a
  provider memory file (gap-a44c80b2).
- `blackboxd --help` / `--version` are side-effect-free: they print and
  exit before any store open, background worker, port bind, or tokio
  runtime; unknown flags exit 2 instead of silently starting a daemon
  (gap-663baff0).
- Resolving a workflow-origin thread no longer writes a repo-owned
  `.bbox/record/` snapshot — discarding stale wf-* arc scaffolding leaves
  no reviewable exhaust; promotion still records deliberately
  (gap-8500c221).
- Pathology correction-plan sections (diagnosis, evidence, remediation,
  deferred, atom mapping) render object items as key-value prose instead
  of backtick JSON blobs (gap-d062e178).
- Foreach/matrix collected item results carry the child arc's
  `actor_sessions` (terminal actor→session map) alongside the existing
  outputs/exports/arc ids — full per-child dispatch provenance
  (gap-513594d8).
- `docs/perf-pathology-dispatch.md` repointed at the split
  `perf-pathology-rust` / `perf-pathology-java` lanes (gap-96936c27).

- Workflow dispatch waits no longer report a task that already completed as
  timed out (gap-0301dc75): `wait_for_task_with_timeout` re-checks the task's
  terminal status when the timer fires, closing the race where an ensemble
  member's finished output landed in the same tick as the timeout and the
  whole node was failed anyway.

- Visual embedding no longer poison-drops images that violate
  voyage-multimodal-3.5's pixel cap or aspect-ratio range (gap-48ae5495):
  `embed_visual_requests` now normalizes each payload before it reaches the
  provider. Images over 16M pixels (e.g. full-page scans) are downscaled
  under a conservative target; images whose long:short ratio exceeds a
  local 19:1 cap (thin OCR scan strips) are padded on the short side with
  white background, then re-checked against the pixel cap in case padding
  grew the canvas over it. `preflight_part` now also enforces the
  aspect-ratio cap locally so a violation that survives normalization
  (an undecodable payload, a video mislabeled as an image) still fails
  closed as poison instead of reaching the network. Adds the `image` crate
  (default-features off, jpeg/png/gif/webp decode plus jpeg/png encode
  only) to `bbox-embed`.

### Changed

- Harness-backed providers now execute in one standalone `bro-harness` child
  per dispatch instead of linking provider transports, code-mode, and V8 into
  `blackboxd`. User/control input uses stdin NDJSON, the existing event envelope
  uses stdout, session logs remain under the task `BRO_HOME`, and daemon tools
  arrive through the complete server-filtered MCP catalog. The compatibility
  flat tools `corpus_search` and `atom_invoke` alias daemon MCP methods without
  hiding their qualified forms. The standalone `isolate` binary continues to
  run V8 cells without the daemon.

- The whiteboard example (`examples/whiteboard/`) and the docs ADR example
  (`docs/whiteboards.md`) now demonstrate genuine multi-round deliberation
  instead of a single "annotate + vote" pass: an evidence round in the
  validate phase, a challenge round separated from voting, gated response
  rounds where each specialist answers the challenges against its own posts
  (concede / rebut with new evidence / withdraw / let stand), a deliberative
  loop gate on `whiteboard_summarize`'s `unresolved_challenges` with an
  agree-to-disagree round ceiling (new packet
  `whiteboard-demo/debate-settled`), and votes cast only after the exchange.
  The example also demonstrates the new per-actor dispatch `timeout` knob.

### Added

- `kimi` joins the dispatch provider stable: Kimi (Moonshot AI) rides
  bro-harness on the Anthropic Messages transport with credentials/base URL
  lifted from `~/.claude-k/settings.json` (the Kimi-for-Coding endpoint,
  `https://api.kimi.com/coding`). Model catalog carries the live-probed
  upstream ids (`k3` default; the Claude Code slot ids `k3[1m]` /
  `kimi-k3[1m]` are endpoint-rejected and normalize to `k3` at arg build;
  plus `kimi-k2.7-code`, `kimi-k2.7-code-highspeed`, `kimi-k2.6`,
  `kimi-k2.5`); effort catalog mirrors the other Anthropic lanes but
  defaults to `max` per vendor guidance. Fleet cockpit picks it up as a
  provider row (tag `k3`), classifier alias `kimi`/`k3`, and the transcript
  adapters attribute `k3*`/`kimi-*` sessions to the new lane. Harness
  compaction windows: `k3*`/`kimi-k3*` are 1M-class, `kimi-k2*` 256K-class.
  Wire-verified (2026-07-19 probe): beta header + `?beta=true`,
  `cache_control` breakpoints, and `output_config.effort` accepted;
  server-side prompt caching engages and reports `cache_read_input_tokens`.
  Allocator built-ins map the lane into every tier (`k3` at medium/high/max
  for standard/premium/deepthink, `kimi-k2.7-code-highspeed` at low for
  economy and drones) and both pools (`coding` weight 0.85, `any` weight
  0.6); all tier effort pins were probe-verified against the endpoint.
- Ensemble workflow nodes accept a `board` binding (template → whiteboard
  id) for engine-driven board auto-apply (gap-7fbefe13): each member's
  STRICT-JSON output — one object or an array of
  `{action: post|annotate|vote|none, …}` items, code fences tolerated —
  is parsed into typed `BoardAction`s and applied through the same
  registry checks the `whiteboard_*` tools enforce, with attribution from
  the item's `agent_name` (falling back to the member name). A salvage
  pass recovers schema-valid answers from drifted output (prose
  preambles, provider tool-call echoes) without letting unrelated JSON
  false-positive into board mutations. Closes the
  forgotten-tool-call failure mode where an agent writes the deliberation
  but never lands it on the board; failures log `board_autoapply_*` arc
  events without failing the node. The whiteboard example's Vote node now
  uses it. Live-proven: probe arcs posted 3/3 member claims engine-side
  with zero whiteboard tool calls.

- Ensemble dispatch substitutes `${member.name}` per team member in node
  prompts (the ArcContext templater deliberately leaves unknown
  `${member.*}` heads verbatim), and the `/admin/team/upsert` route
  accepts named members (`{name, brofile}` objects alongside bare brofile
  strings). One prompt template now drives N differently-named board
  agents without per-brofile lens duplication; the whiteboard example's
  team members are named `security` / `performance` / `design` so
  `${member.name}`, board registration, and auto-apply fallback
  attribution all agree.

- Workflow actor and node specs accept a `timeout` field (duration string
  like `"30m"` or number of seconds; node overrides actor; default stays
  900s) governing how long the engine waits on dispatched actor tasks -
  executor nodes, each ensemble member, and fire-and-forget join waits
  (gap-0301dc75). Zero/negative timeouts are rejected at compile/parse
  time, and `wait.timeout` now also accepts bare numbers of seconds.
- `whiteboard_archive` accepts `force=true` (facilitator/operator role) to
  archive a board from any phase - the abandon path for boards stranded
  mid-phase by a failed arc, e.g. from `on_arc_exit` cleanup hooks
  (gap-0301dc75). The phase history records the phase the board was
  abandoned from.

- `extract_java_class` grows a `constructor_injection` wiring strategy
  (gap-3f582e0a): the delegate becomes a `private final` field plus a
  parameter appended to the source's first constructor (inserted before a
  trailing varargs parameter) with `this.<delegate> = <delegate>;` wiring,
  so zero-field-injection architectures no longer receive generated
  `@Inject` fields. A source with no constructor gets a synthesized one
  carrying the caller-supplied annotations; a delegate name colliding with
  an existing constructor parameter is refused with a dedicated error code.
  The `java.extractClass` binding auto-selects the strategy from the
  source's own injection idiom (`@Inject` constructor selects
  `constructor_injection`, field-level `@Inject` selects
  `external_injection`, non-DI stays `own_construction`), and
  `java.extractClassPreviewPlan` recommends it the same way.

### Fixed

- `extract_java_class` no longer silently strands constructor assignments to
  moved fields whose initializer fails the bare-parameter threading test
  (e.g. a computed expression or object creation): each orphaned assignment
  now gets a FIXME comment above it and a `leftovers` entry on the plan, so
  previews and applies surface the manual relocation instead of producing
  non-compiling output discovered at build time.

- Git subprocesses spawned by the corpus resolvers (`git_output` /
  `git_output_strings`, e.g. session-cwd base-project resolution during
  reindex) now run under a 10s kill-on-timeout deadline. A transcript
  session whose cwd points into a dead autofs/NFS automount previously
  hung `git rev-parse` forever inside the IndexWriterActor, wedging all
  indexing until the child was killed by hand (observed live against a
  torn-down NFS lane). Timed-out children are killed with a bounded reap
  so an unkillable D-state child can never re-wedge the caller.
- `bbox_hybrid_search` doc_type filtering no longer collapses to zero
  results at small limits: the type filter ran only after RRF fusion,
  whose candidate pool (capped at `fetch`) filled with off-type vector
  hits, so a filtered query could return nothing while dozens of on-type
  BM25 matches existed (observed with `doc_type=transcript`, limit 3).
  Ranked lanes are now scoped to the requested type before fusion, with
  the post-fusion retain kept as backstop.
- Hybrid search transcript hits now carry canonical, parseable
  `transcript:<provider>:<session>:<offset>:<idx>` entity refs. Transcript
  docs store a legacy unprefixed entity_id shape that EntityRef cannot
  parse, so inspect/find_paths/bundle handoffs and eval expected refs
  could never match a hybrid transcript hit; the id is now canonicalized
  at read time from the doc fields (no reindex required).

### Changed

- File tools no longer pretend to confine to the session worktree
  (gap-e0ae3e7d, friction/2026-June-13-0840pm-worktree-containment-issues.md):
  `file_read` / `file_edit` / `file_write` / `code.*` bindings / `apply_patch`
  accept absolute paths; relative paths still join against the effective
  worktree root. The `path escapes worktree root` denial is gone. The
  harness has no other sandboxing machinery — shell already escaped any
  `cx.root` boundary with `git -C`, `find`, `tee`, `sed -i`, etc. — so the
  structured-tool containment was a speed bump, not a boundary. Schema
  descriptions updated to say "Relative paths resolve against the worktree
  root; absolute paths are accepted as-is." `bro_tools::workspace::
  resolve_in_root` and `bro_apply_patch::apply::resolve_within` are now pure
  path normalizers. **LSP refactor plan integrity** at `lsp_facts.rs`
  (`WorkspaceEdit` outside the worktree) and `java_transforms.rs`
  (`relativize`) intentionally retained — those are refactor plan
  safety checks, not file-tool confinement. `analysis.*` and `java.*` input
  schemas updated to reflect the new path contract; their plan OUTPUTS
  are still required to land inside the worktree. Gap-e0ae3e7d addressed.

### Added

- PDF OCR fallback (X-PDF, `agentic-corpus-multimodal-chunkers.md`): pages
  with no extractable text now rasterize via `pdftoppm` and recognize via
  `tesseract` when both are on PATH (availability-gated shell-outs; hosts
  without them keep text-first behavior unchanged). Kill-on-timeout per
  child, 300s per-document budget, 100-page cap; wholesale extraction
  failures probe from page 1 and stop at the first empty batch. OCR-sourced
  chunks stay `pdf_page` in the Docs bucket and carry `symbol = "ocr"` as a
  provenance marker.
- Embedding routing substrate (Layer 0 of
  `design/corpus/agentic-corpus/multimodal-embedding-routing.md`):
  `[embed.providers]` is now a typed alias map (`type = "voyage_text" |
  "ollama"`), so multiple Voyage-backed routes with different models can
  coexist; legacy `voyage`/`ollama` tables without `type` still parse.
  Every embed call is role-marked (`input_type=document` for stored
  content, `input_type=query` for live search), Voyage requests pin
  `output_dimension`, and routes carry `endpoint_kind`, `output_dtype`,
  and a code-derived `compatibility_family`. Asymmetric
  `document_model`/`query_model` pairs (voyage-4 family) are supported and
  family-checked at config load. Partition ids are unchanged for existing
  float routes; a non-float dtype forces a new partition. `bbox_embed_status`
  reports the new route identity fields. The query-embedding cache key now
  includes query model, dimension, and dtype.
- Model rerank stage (Layer 3): `bbox_hybrid_search` accepts
  `rerank="model"|"heuristic"|"none"`. Model mode sends the fused top-k to
  the `[embed.rerank]` cross-encoder (default `rerank-2.5-lite`, top_k 64),
  orders the top-k by relevance above the unsent tail, applies the
  heuristic type/temporal multipliers after under the existing cap, and
  degrades to the heuristic path with `degraded.rerank_unavailable` on API
  failure. After the measured eval win (MRR +56%, recall@1 2.5x) and
  operator latency acceptance, model rerank is the DEFAULT; heuristic and
  none remain per-call opt-outs.
- Contextualized chunk embeddings (Layer 2): `type = "voyage_context"`
  provider aliases (voyage-context-4, its own compatibility family), with
  document-grouped queue batching: chunks of one document embed together
  (`inputs: [[chunk, ...]]`), batches never split a document's consecutive
  run, and oversized documents fall back to windowed sub-documents.
  Queries encode as single-chunk documents against the same partition.
  No bucket routes there by default; the code/docs migration stays
  eval-gated.
- Multimodal route family (Layer 4, opt-in): `type = "voyage_multimodal"`
  provider aliases (voyage-multimodal-3.5, own family), interleaved
  text/image/video content with `truncation=false` always and local
  payload preflight (image byte/pixel caps, video byte cap) rejected as
  typed poison. `[embed.routes.visual]` maps visual chunk kinds to
  multimodal aliases only; text buckets never fall back there.
- Chunker registry grew five formats beyond PDF: `.ipynb` (per-cell
  `notebook_cell` chunks with truncated text outputs, code cells routed to
  the Code bucket via kernel language), `.xlsx`/`.xlsm`/`.xlsb`/`.xls`/
  `.ods` (per-sheet `spreadsheet_sheet` chunks with inline formulas,
  bounded row/char projection), `.html`/`.htm`/`.xhtml` (`web_section`
  chunks split on h1-h3 with script/nav/style stripped; previously .html
  was misrouted through the tree-sitter code chunker), and `.vtt`/`.srt`
  transcript files (`transcript_segment` chunks with start-second
  metadata and speaker prefixes), plus `.docx`/`.pptx` (`office_section`
  chunks split on heading styles, one `slide` chunk per non-empty slide;
  OOXML parsed directly via zip + quick-xml).
- Eval instrumentation for the eval-gated route decisions:
  `eval/scripts/rerank_mode_ab.py` (model vs heuristic vs none rerank A/B
  over the 30-query suite) and the `context_model_ab` example in
  bbox-embed (voyage-code-3 vs voyage-4 vs voyage-context-4 with
  production chunking). First runs: model rerank MRR 0.167 vs heuristic
  0.107 (win measured; default flip awaits latency acceptance);
  contextualized embeddings showed no retrieval win, so code/docs stay on
  their current models.
- Contextualized document packing enforces the model's 32K-token
  per-document context window (verified live): document runs window at
  64 KiB and two windows of one document never share a batch.
- X-PDF text-first chunker: `.pdf` files now index as per-page `pdf_page`
  chunks (pdf-extract) into the docs bucket; encrypted/scanned/corrupt
  PDFs degrade to zero chunks without failing the reindex pass. Tables
  and OCR remain deferred; figures shipped separately below.
- Visual sidecar path (Layer 4 completion): a new content-hash-addressed
  payload store (`bbox-visual-store`, outside tantivy, deduped by hash,
  under the daemon state dir) backs two new visual chunk kinds. X-IMG
  claims standalone `.png`/`.jpg`/`.jpeg`/`.gif`/`.webp` files (extension
  plus magic-byte scan) and emits one `image` chunk per file with the file
  name stem as its only text content. `pdf_figure` extracts embedded
  raster XObjects from PDFs via `lopdf` (DCTDecode/JPEG passthrough and
  unpredicted FlateDecode DeviceGray/RGB re-encoded into a minimal
  hand-built PNG; other encodings are skipped rather than mis-decoded).
  Both route through a new visual embed queue lane
  (`[embed.routes.visual]`, opt-in per chunk kind) that loads payload
  bytes from the sidecar at request-build time and batches by payload
  byte size rather than caption text length; index-time enqueue and
  backfill/reembed coverage both recognize visual chunk kinds and report
  status under `visual:<kind>` route keys. Sidecar anchoring uses the
  interim chunk[0]-as-file proxy (gap-ab3ef97f tracks the eventual
  `file:` entity migration).
- Query-side visual retrieval (Layer 4 retrieval leg): `bbox_hybrid_search`
  now searches configured `[embed.routes.visual]` partitions instead of
  silently skipping them - the vector lane enumeration was bucket-keyed
  while visual routes are chunk-kind-keyed, so a partition with indexed
  image/pdf_figure vectors never surfaced in `searched_partitions`. The
  query embeds once via the multimodal alias through the existing
  process-wide query cache (no re-bill on repeat queries), searches each
  distinct visual partition at the same `vector_weight` as text lanes
  (image and pdf_figure sharing one alias dedupe to one search), and
  degrades only that lane on embed failure (`degraded.vector_errors`) -
  never the whole search. Hosts with no `[embed.routes.visual]` entry see
  no behavior change. `bbox_embed_status` rows for `visual:<kind>` lanes
  now populate `provider`/`model`/`dim`/`compatibility_family` from
  `VisualRouteMeta` instead of showing them `null` next to
  `available=true`.
- `bbox_embed_partitions`: vector partition lifecycle tool (Layer 5 of the
  same design). `action="list"` inventories every on-disk partition with
  dims, active count, last write, disk bytes, and whether any configured
  bucket currently maps to it (orphans show `mapped=false`).
  `action="prune"` reclaims orphaned partitions: requires
  `older_than_days`, only deletes partitions both unmapped by current
  route config and idle beyond that age, and is dry-run unless
  `apply=true`. `bbox_reembed` never prunes.
- `isolate` now has code-mode cell execution via repeated `--cell` or
  `--cell-file` flags, using the same harness `exec` runtime so nested
  `tools.*` / namespace calls and session `store()` / `load()` work during
  command-line probe validation.
- Java isolate refactor probes now have first-class long-method extraction
  gates: `analysis.methodRegions({ file, method, className?, ranges? })`
  reports statement/candidate-region captures, live-outs, field touches,
  lambda/listener counts, non-local control flow, and extractability stop
  reasons; `java.extractMethodCodeBlock({ file, oldText, methodName, ... })`
  exposes the existing code-block extractor as a code-mode transform returning
  `edits.merge` changes with explicit hints for multi-live-out and control-flow
  refusals.
- `java.extractMethodCodeBlock` can now opt into `resultRecord: true` for
  multiple safe Java live-outs: it synthesizes a private nested record, returns
  the bundle from the helper, and unpacks the components at the call site while
  refusing inferred types and nested-scope live-outs that would not be visible at
  the helper return site.
- `analysis.methodRegions` now supports compact long-method inventories via
  `includeStatementRegions`, `statementContains`, line-window filters, and
  `statementLimit`, plus a `statement_region_summary` that reports total,
  matched, returned, and omitted statement counts. Live-out variables now include
  `after_use_kinds` and `component_tree_consumptions` so Java UI extraction
  probes can distinguish return values and in-region component-tree wiring from
  undifferentiated multi-live-out blockers.
- `analysis.methodRegions` now exposes syntax-only `resolved_type` hints for
  Java `var` captures/live-outs when the type is locally derivable, and can opt
  into `includeNestedStatementRegions` for marker searches inside giant
  enclosing loops/blocks. `java.extractMethodCodeBlock` consumes resolved
  simple `var` types while still refusing unresolved inferred helper boundaries.
- Java extraction probes now cover the next tranche of isolate ergonomics:
  `resolved_type` also handles conservative same-file method-call returns and
  known static-factory receivers, `code.readLines({ file, startLine, endLine })`
  reads exact hash-anchored source text from line ranges, and generated helper
  bodies strip common deep-nesting indentation before insertion.
- Embedding coverage now converges and says so when it can't
  (gap-b9d39c10): `bbox_embed_status` reports health=`stalled` (with a
  health_reason naming the fix) when an available route's coverage sits
  under threshold with an idle queue — previously that state read as
  health=ok forever (git_message sat at 0% coverage invisibly). The
  transcripts route reports an explicit `coverage_state` ("guarded: …")
  instead of a null indistinguishable from broken. `bbox_reembed` gains
  `route="backfill"` — an idempotent residue sweep of every route except
  guarded transcripts — and `embed-compaction-arc` v3 runs it nightly so
  items that predate a route or were dropped after retries eventually
  embed.
- Provider-rejected embed payloads no longer poison their batch
  (gap-e3e033ce): a payload-level HTTP 4xx (e.g. Voyage's empty-string
  rejection) is classified non-retryable, and the queue bisects the batch
  to the offending item(s) instead of retrying three times and dropping
  all 52 batch-mates with it. Empty/whitespace text is skipped at enqueue
  and excluded from coverage (it was the actual root cause — providers
  reject empty input). Permanently-dropped items are counted in a durable
  `dropped_count` on `bbox_embed_status` (with a `last_dropped`
  diagnostic) that a later success does not clear, and a `stalled` route
  whose shortfall is poison says so rather than pointing at a backfill
  that cannot close the gap.
- Inactive workspace-snapshot retention is now budget-bounded
  (gap-efd270dd): `bbox_storage_gc` gains `max_snapshots_per_workspace`
  (default 32) and `max_snapshot_total_bytes_per_workspace` (default
  16 GiB), bounding the age-based keep so per-commit snapshot churn can no
  longer reach ~100 GB steady state inside the age window. Floors (recent
  per workspace/repo, branch-switch grace) always retain and consume the
  budget. The in-daemon 6h maintenance pass inherits the budgets via the
  policy default, and `daily-compaction-arc` v2 passes explicit
  `max_snapshot_age_days=7` plus both budgets.
- `bbox_inbox` now surfaces "Cron scheduling gaps": cron-routing packets
  installed with no live cron scheduling them (maintenance that exists but
  silently never runs — the class behind storage GC never firing,
  gap-f268badd), and the inverse, live crons whose routing packet domain is
  missing. `system-defaults/maintenance/scripts/install-maintenance.sh` is
  the one-command, idempotent deploy step that installs and schedules both
  maintenance arcs (daily-compaction, embed-compaction-nightly) including
  their cron specs.

### Fixed

- `java.extractMethodCodeBlock` now preserves call-site indentation, inserts
  helpers with normalized method spacing, and re-indents moved blocks relative
  to the helper body instead of collapsing nested indentation.
  `analysis.methodRegions` no longer treats `return` statements inside a fully
  selected Java lambda as method-level non-local control flow. `code.read` now
  reports exact `byte_length`, `char_length`, and `truncated=false` metadata so
  display truncation is not confused with source truncation.
- The tool-docs coverage tests (`every_registered_tool_has_a_doc`,
  `description_summary_parity`) now resolve their `src/` scan root at
  runtime (walking up from the test cwd to the `[workspace]` manifest)
  instead of compile-time `CARGO_MANIFEST_DIR` — a test binary carried into
  a worktree by the CoW-seeded `target/` clone previously scanned the
  checkout it was compiled in and silently passed on handlers the worktree
  added.
- Worktree-redirected repo-owned records (knowledge entries and gap notes)
  now survive a daemon restart that happens before their worktree branch
  merges: the central store retains the record (with its redirect marker)
  until the committed file is observed under a registered base root, then
  drops it. A redirect whose worktree disappeared stays central-only — the
  daemon never falls back to writing the base checkout on a branch's
  behalf. Committed repo files never carry the redirect marker.
- Corpus ops from inside a worktree now key durable state to the registered
  base project. `bbox_learn` / `bbox_remember` / `bbox_decide` resolve a
  worktree `project` to the base scope and redirect the repo-owned
  `.bbox/knowledge/` file into the worktree checkout (so an immediate
  `bbox_render` from the same worktree includes the entry — the
  gap-de82a74d asymmetry); pins key to the base and dispatch injection
  aliases the literal worktree cwd; notes, roadmap items, and whiteboards
  rescope their `project` on write; notes/roadmap/inbox filters map
  worktree paths to the base scope. `bbox_thread` lookup misses and empty
  listings now carry a store-identity breadcrumb (lookup is global by id;
  `project` never narrows it) so cross-daemon list/resolve divergence
  (gap-518d7215) is visible instead of mystifying.

### Removed

- Councils feature removed (`bro_council_list`, `bro_council_open`,
  `bro_council_posts` MCP tools; `/council/*` HTTP routes; `bro council` CLI;
  `CouncilPosted` / `CouncilMention` system event kinds; `src/council/` store
  and drain machinery). Orphaned state at `$STORE_DIR/councils/` is left on
  disk and can be deleted manually.
- `bro-irc` sidecar binary (`src/irc_bridge.rs`) removed along with the
  `/irc/*` legacy HTTP route aliases, the `irc 1.1.0` crate dependency, and
  the `deploy/irc/` ngircd stack (`deploy/bro-irc.service`,
  `deploy/irc/docker-compose.yml`, `deploy/irc/config/ngircd.conf`). The
  `/control/*` HTTP control plane is unaffected. Dependency tree shed 4
  unique nodes (626 → 622); the remaining 49 irc subtree crates were
  shared with other workspace dependencies.
- Unused direct dependencies dropped from 5 workspace crates (cargo-machete
  cleanup, 623 → 609 lock entries): `blackbox` root loses `bincode`, `crossterm`,
  `ignore`, `lsp-types`, `ordered-float`, `ratatui`, `ratatui-core`, `rusqlite`,
  `serde_yaml`, `tera`, `toml_edit`, `tree-sitter` + 10 grammars, `tui-markdown`,
  `unicode-width`, `url`, `wide` (TUI surface lives in `crates/bro-cli`; grammars
  compile via `bbox-chunker`; `rusqlite`/`libsqlite3-sys` leaves the build
  entirely); `bro-cli` loses `bro-transcript` (local module, not the crate) and
  `futures` (only `futures-util` is used); `bbox-code-nav` loses `tracing`;
  `bro-tools` loses `walkdir`; `bbox-refactor` loses `chrono`, `serde_yaml`,
  `strum`, `tokio`, and 9 tree-sitter grammars (accessed through `bbox-chunker`).
- rustls crypto provider unified on `ring` across the workspace (609 → 604 lock
  entries): `bro-harness` switched from `aws_lc_rs` to `ring` feature; rmcp's
  `reqwest` feature replaced with `reqwest-tls-no-provider` in both the root and
  `bro-harness` to prevent reqwest 0.13's default `rustls` feature from
  re-activating aws-lc-rs. `aws-lc-sys` (43.6s build script) and `aws-lc-rs`
  leave the build; `bindgen`/`clang-sys` remain via `v8` (bro-code-mode). The WS
  Responses transport (`openai_responses_ws.rs`) now installs the ring provider.

### Added

- `bro_arc_result` MCP tool (gap-55be3518): read a completed workflow arc's
  structured result — `structuredExit` (vars._structured_exit), final `vars`
  (filterable via `keys`), `arcThreadId`, `actorSessions`, optional
  `nodeOutputs` — by arcId or task id, without parsing bro_wait's escaped
  envelope. Alongside it, the workflow task result envelope no longer
  duplicates the arc's event log (events stay in the task event log,
  readable via `bro_status` tail), which shrinks bro_wait's workflow result
  from ~81k chars to the structured fields; and `bro_wait` / awaited
  `bro_orchestrate_run` now lift `structuredExit` first-class on workflow
  tasks.
- MCP response cap is now lossless: over-cap tool responses (>80KB) are
  spilled to `<state_dir>/response-dumps/` before the inline reply is capped.
  The JSON `response_too_large` envelope gains a `spilled_to` path (full
  payload, readable with any file tool); oversized text responses name the
  dump path in their truncation suffix. Previously the overflow was simply
  destroyed (1KB preview for JSON, hard truncation for text), which broke
  programmatic consumers (code-mode isolates) and forced interactive agents
  to re-query. Spill failure falls back to the old inline truncation; dumps
  older than 7 days are pruned on the next spill.
- Worktree-scope cluster: dispatched agents working in daemon-managed
  worktrees now get coherent project scoping end to end. Dispatch emits a
  `pin:*.project_dir` worktree-confinement pin from one mechanical choke
  point (`AmbientContext::tool_arg_defaults`) covering bro_exec/resume,
  agent dispatch, workflow executor turns, and the fleet cockpit
  (gap-8144b4b5); retrieval surfaces (`bbox_hybrid_search` project filter,
  knowledge scoping, render) resolve worktree and descendant paths to the
  registered base project via `resolve_base_project_for_scope`
  (gap-72edd4f2); and the operator-approved ambient tool-arg default
  expansion (gap-ae22a6b2 item 2) fills retrieval-read scope
  (`bbox_hybrid_search.project`, `bbox_discover_seed_entities.project`, and
  `project_dir` on the read-only `bbox_code_*` / `bbox_workspace_symbols`
  family) plus coordination ids (`bro_report.task_id`) from the dispatch's
  ambient context. Defaults fill only when the model elides the param;
  passing `project=""` still requests an unscoped search; knowledge/note/
  learn `project` params stay permanently excluded (absence means global
  write scope), and `bbox_thread` ids are not defaulted because the
  per-(tool,param) table cannot action-gate. The dispatch tools (`bro_exec`,
  `bro_resume`, `bro_broadcast`, `bro_agent_dispatch`) now advertise `cwd`
  as the schema name for the dispatch working directory — the contract-bottom
  canonical name — with `project_dir` retained as a deprecated serde alias
  (gap-6366c92d; inverts the interim 830c2c0 aliasing). Because the
  default/pin table applies in the harness before the daemon's alias
  normalization, the worktree-confinement pin is emitted under both
  spellings (`pin:*.cwd` + `pin:*.project_dir`), and session-start schema
  validation no longer warns when a glob rule matches tools that simply
  lack the param (glob rules are "wherever the param exists" by design;
  exact-name rules still warn on unknown params). Project-dir-semantic
  params on code-nav/refactor/team/brofile/badgey/allocator tools keep
  their names. Gap mutation paths now honor worktree write-targeting too
  (gap-b94129ba): `bbox_gap_resolve` / `bbox_gap_update` accept an optional
  `project` (session cwd / worktree path) that redirects the rewritten
  repo-owned `.bbox/gaps/<id>.json` into the session's own worktree —
  write redirection only, knowledge-lane style (the session commits it,
  the branch carries it, the gap's durable project scope never changes,
  global gaps ignore it, and the base checkout's committed copy survives
  the save-side generation purge untouched). In-tree linked worktrees of
  a registered repo (e.g. `<root>/.claude/worktrees/<name>`) are now a
  recognized worktree class across the write-side surfaces
  (`fleet_worktree_scope_and_dir`: gap filing/mutation, thread records,
  slices, code-nav) via a structural gate — the nearest `.git` marker is
  a FILE pointing into the registered root's `.git/worktrees/` — so a
  plain subdirectory is never worktree-classed. Dispatches also gain
  ambient `project` defaults for `bbox_gap` / `bbox_gap_resolve` /
  `bbox_gap_update` from the canonicalized dispatch cwd (`bbox_gaps`
  stays undefaulted: its `project` is a result filter), with
  `scope="global"` winning over a defaulted `project` on filing.
- `extract_rust_crate` refactor compound (gap-fe4dd97f): peel leaf root
  modules of a monolithic crate into a new workspace-member crate via
  `bbox_refactor_run`. New primitives: `extract_rust_crate_scaffold` (atomic
  crate scaffold + dependency inference + module file moves + visibility-
  preserving `mod`→`use <crate>::<mod>;` alias swap + `[workspace].members`
  merge + consumer dep wiring; fails closed on non-leaf modules with
  file:line offenders), `rewrite_rust_crate_paths` (crate-path rewriting
  with mixed-group use-tree splitting), and `rust_workspace_dag_check`
  (path-dependency acyclicity guard, dev-deps excluded). Analysis-only plan
  kinds are now valid `bbox_refactor_run` steps (the runner and apply
  previously rejected every edit-less plan).
- Root-crate split (continued): runtime system memories moved to a new
  `bbox-system-memory` leaf crate, and the dependency-free `query`,
  `template`, and `search` (rrf/rerank) modules moved into `bbox-corpus-core`
  — all re-exported under their original `crate::` paths. System-memory test
  initialization is now `init_for_tests_from(&dir)` in the leaf with the
  repo-root defaults path owned by the daemon's `util::init_system_memory_for_tests`.
- Root-crate split: the rule-packet engine moved to a new `bbox-packets` leaf
  crate (compile/apply/audit AST, coercion, scanner, event log), re-exported as
  `crate::packets` so call sites are unchanged. Shared `json_store` and
  `now_iso` moved into `bbox-corpus-core`; companion gap-note ingestion
  inverted root-side (`gaps::emit_companion_packet_gap_note`) so the leaf has
  no gap-store coupling.
- cargo-nextest test gates (`.config/nextest.toml`): the test suite now runs
  process-per-test under nextest, workspace-wide —
  `cargo nextest run --workspace` is the mid-cycle gate (~24s, 3,700 tests;
  the two >45s tests are quarantined), and `--workspace --profile full` is
  the fold/closeout gate running the entire suite (~85s). This also closes a
  coverage gap: the previous `cargo test --lib` gate covered the root package
  only, leaving ~1,800 tests in the peeled crates (854 in bbox-refactor
  alone) outside the documented gate. Per-test slow-timeouts name newly slow
  tests instead of silently stretching the suite. `cargo test --lib` remains
  the no-install fallback.
- Phase-4 concurrency enforcement (concurrency-model §5): a `clippy.toml`
  disallowed-methods gate denies blocking fs/process/tantivy-writer calls in
  MCP handler modules and the harness crates (sanctioned actor contexts carry
  reasoned `#[allow]`s), `scripts/lint-concurrency.sh` blocks new sync
  `#[tool]` handlers and thread spawns in tool modules, and a debug-build
  `BlockingScope` marker panics if a sanctioned actor body ever runs on a
  runtime thread. Landing the gate also converted the last two disk-writing
  sync handlers (`bbox_packet_gap`, `bro_slack_bind`) to the blocking pool,
  moved `/control/closeout`'s git phases off the runtime workers, and wrapped
  `apply_patch`'s pre-image reads in the harness blocking helper.

### Fixed

- Stream-delta ingest no longer does O(message) work per token chunk on the
  daemon's async runtime. Per accepted event the ingest path: seeds the
  parse sink by taking (not cloning) the accumulated assistant message,
  computes the live-tail snippet in O(tail) instead of O(message), stores
  the event by move instead of deep clone — and stream_event envelopes are
  no longer stored in the per-task event ring at all (every consumer
  already filtered or structurally skipped them). Roster summaries throttle
  to ~1/s on the delta path, and `task.progress` system events are emitted
  only at step boundaries — previously every text delta appended a line to
  the system-events journal (99.9% of the production journal was
  task.progress).
- The harness sidecar event log no longer writes on the daemon's async
  runtime: appends enqueue on a bounded channel drained by a dedicated
  writer thread (serialization + ordered `write_all` happen there), with a
  turn-boundary flush bounding the crash-durability gap to the current
  turn. Previously every protocol event paid a full-envelope serialize +
  sync write inline in the agent loop (40–65 events/sec while a bro
  streams, events up to ~100KB).
- `bro agent` now renders its agent's transcript. The standalone cockpit read
  the task handle's local event buffer, which daemon-backed dispatch never
  fills — the prompt appeared sent but no response, thinking, or result ever
  rendered, and the activity throbber counted forever. Standalone now rides
  the same focused-transcript SSE stream as the fleet zoom view, swaps in the
  daemon roster handle so lifecycle updates flow, and both cockpits refetch
  the focused snapshot when the focused task reaches a terminal status so the
  closing assistant turn always appears. Terminal agents now render
  "✓ took Ns" instead of a perpetual "working" spinner (the state previously
  derived from a stream heuristic that defaults to turn-active on an empty
  event buffer).
- In-process harness tool execution no longer blocks the daemon's async
  runtime: the sync-bodied builtins (`content_search`, `glob`,
  `sandbox_status`, `sandbox_grounding` — tree walks, capped reads, sync git
  captures) now run on the blocking pool. Under compile-free streaming-agent
  load these inline bodies degraded mean worker poll time from ~130µs to
  6–11ms with ~40% of polls over 900µs.
- A model turn that ends with no text and no tool calls (e.g. an output-token
  cap hit mid-thinking) no longer silently terminates the session as a clean
  success carrying stale narration as its result. The harness nudges the model
  once to produce its final answer; if the retry also returns nothing, the
  turn ends with the empty-output stop flagged in turn-end diagnostics and a
  `suspicious_turn_end` block on the result event. The detector previously
  tested session-accumulated text, so any earlier narration masked the
  condition entirely.
- Remaining MCP handlers doing disk I/O inline on tokio workers moved to the
  blocking pool: the five packet tools (`bbox_compile`/`bbox_apply`/
  `bbox_audit` append fsync'd events; `bbox_packet_list`/`bbox_packet_events`
  re-read the store/event log), `bbox_inbox` (gap-spool import rewrites the
  gap store under its write lock), and `bbox_artifact_supersede`/
  `bbox_artifact_remove` (flock'd catalog rewrites).
- bro-harness Anthropic transport input-token accounting for GLM (and any
  other Anthropic-compatible endpoint that doesn't place `input_tokens`
  exactly where the canonical shape expects it). The SSE fold now reads
  `input_tokens` from the four candidate paths observed in production
  (`message.usage.input_tokens`, flat `usage.input_tokens`,
  `message.usage.prompt_tokens`, flat `usage.prompt_tokens`), treats a
  zero value in `message_start` as a placeholder rather than a real
  count, and also captures `input_tokens` / `cache_read_input_tokens` /
  `cache_creation_input_tokens` from `message_delta.usage` (the field the
  previous parser ignored) so the end-of-stream snapshot overrides the
  start-of-stream placeholder. DeepSeek on the same transport is
  unaffected. Also: a cancelled or interrupted turn now reports the
  usage accumulated up to the drop point instead of zeros — the running
  segment usage is mirrored onto the transport after every fold and
  recovered via a new `Transport::take_interrupted_usage` trait method
  (default returns zeros for transports that don't track segment
  state).

- Edge-index rebuilds no longer stall the daemon under load: the store read
  guards now cover only the fast in-memory projections, while the multi-GB
  sidecar parse (measured 13–109s per rebuild in production) runs with no
  guards held. `bbox_thread` link actions and project unregister now wake the
  rebuild watcher instead of rebuilding inline on the async runtime — they
  return immediately, and the changed edges appear in the graph within
  seconds (previously these calls blocked for the full rebuild duration).
- Gap-note mutations (`bbox_gap`, `bbox_gap_resolve`, `bbox_gap_update`) run
  their disk-authoritative reload/rewrite on the blocking pool instead of a
  tokio worker.
- Inactive edge-snapshot growth: the 6-hourly storage GC maintenance pass now
  applies a 2-day snapshot age floor (override with
  `BLACKBOX_STORAGE_GC_SNAPSHOT_MAX_AGE_DAYS`; keep-recent retention of
  3/workspace + 10/repo is unchanged). Previously the 14-day default let
  per-commit snapshot churn accumulate ~24GB in one heavy week. GC also
  removes snapshot directories once all their files are pruned.
- Fleet cockpit assistant-text rendering preserves the author's line
  structure. A `TranscriptItem::AssistantText` whose raw text is one number
  per line (`1\n2\n3\n…`) used to be rendered space-separated and
  soft-wrapped, because the markdown path collapsed single newlines into
  soft breaks per CommonMark. The new
  `render_markdown_preserving_breaks_with_width` rewrites every newline
  (outside fenced code blocks) into a markdown hard break (`  \n`), so
  the source layout survives while inline markdown (bold, code, links,
  …) still renders.

### Added

- `bro-harness` sidecar session event log: every harness session now appends a
  timestamped `<session-id>.events.jsonl` next to its resume snapshot in
  `$BRO_HOME/harness-sessions` — one `{"ts", "event"}` line per protocol
  envelope event (user turns, assistant turns, tool results, terminal results)
  plus harness milestones (session start/resume, compaction triggers).
  Append-only and flushed per line, so crashed or hung sessions keep a durable
  record up to the last completed event; compaction appends to it but never
  rewrites it (the snapshot stays the resume artifact).
- Harness-sessions transcript adapter: in-process harness sessions
  (glm/deepseek/minimax/brodex/vibebh) are now indexed into the transcript
  corpus from their sidecar event logs and surface in `bbox_search`,
  `bbox_messages`, `bbox_session`, and time-based queries like any other
  provider transcript. Harness task records also resolve a
  `transcript_location` pointing at the session event log (populated on status
  reads and at task finish).

- `bro-harness` code-mode (`exec` / `wait`): the authorial/metatool surface,
  adopted from openai/codex's `code-mode` (vendored as the `bro-code-mode`
  crate, Apache-2.0). `exec` runs a JS/TS cell that composes the whole filtered
  tool surface as a typed `tools.*` namespace (a nested `tools.X(...)` dispatches
  the same deny-filtered tool the flat surface exposes — no in-box bypass), emits
  output via `text()`/`image()`, and persists across cells in a session via
  `store()`/`load()`; `wait` resumes a still-running cell by `cell_id`. Replaces
  the NARF authorial surface.
- `minimax` provider: MiniMax M3 ridden through `bro-harness` on the Anthropic
  transport. Credentials/base URL are lifted from `~/.claude-mm/settings.json`
  (the same config selected by the `yolom` alias); default model is
  `MiniMax-M3`.
- `vibebh` provider: Mistral (vibe) ridden through `bro-harness` on the OpenAI
  chat-completions transport, parallel to the existing `vibe` CLI provider
  (which is unchanged). Aliased `vibe-bh`; dispatchable via `bro_exec`,
  workflows, `bro_resume`, and selectable in the Fleet TUI. Capabilities are
  tool-use + resume (the `--model` flag is forwarded verbatim; default
  `mistral-medium-3.5`). Credentials come from `MISTRAL_API_KEY` (process env or
  `~/.vibe/.env`); base URL and reasoning profile are fixed. The exemplar
  first-class completions-transport harness provider — the wiring template for
  future OpenAI-compatible endpoints.
- `bro-harness` OpenAI chat-completions reasoning support: the chat transport
  now folds Mistral's array-form `content` (typed `thinking` chunks) into a
  streamed thinking block and the turn's display thinking, and sends a mapped
  `reasoning_effort` (Mistral accepts `{none, high}`) gated by
  `BRO_HARNESS_CHAT_REASONING`. Reasoning-output parsing is provider-agnostic
  and additive; the plain-string (non-reasoning) path is unchanged.
  providers (Claude, vanilla Codex) run their real interactive TUI inside a tmux
  pane instead of as a headless child, and the turn's output is resolved from
  the provider transcript read plane — never from pane scraping. It is a brofile
  attribute, so every dispatch path picks it up uniformly with no per-call flags:
  workflow executor actors, `bro_exec`, and `bro_resume`. Harness-backed
  providers (brodex/glm/deepseek) and fork/fire-and-forget nodes fail closed.
  Durable actors and `bro_resume` continue the same provider session across
  turns (`codex resume` / `claude --resume`), so a durable terminal node keeps
  context instead of cold-starting. `bro_arc_cancel` (workflow) and `bro_cancel`
  (`bro_exec`) interrupt an in-flight turn and reap its pane. Requires `tmux` on
  `PATH`; headless dispatch is unchanged and remains the default. See
- `bro agent` standalone single-agent cockpit: a one-agent shell that reuses the
  Fleet TUI transcript/composer component without roster chrome, with provider /
  model / effort / cwd launch flags plus standalone `/clear` and `/resume`.
- `bro-harness` custom provider harness (`crates/bro-harness`,
  `crates/bro-tools`): a headless coding agent that speaks provider APIs
  directly behind one `Transport` interface (Anthropic Messages, OpenAI
  Responses, OpenAI Chat), runs its own tool-calling loop, and emits the Claude
  stream-json envelope so it slots into the existing dispatch seam. GLM and
  DeepSeek now route through it on the Anthropic transport, and a new `brodex`
  provider rides the OpenAI Responses (Codex/ChatGPT) backend; the existing
  `codex` CLI path is unchanged. Includes ChatGPT-OAuth token refresh,
  HTTP retry/backoff/timeout, client-side deferred tooling with a pinned-tier
  carve-out (`tool_search`), and client-side allow/deny recursion-guard
  enforcement. The OpenAI Responses transport tracks the modern codex CLI wire
  contract (verified live against the ChatGPT backend): a stable `session-id` +
  per-turn `thread-id` (no random-per-request id), the defunct
  `OpenAI-Beta: responses=experimental` dropped, a stable `prompt_cache_key`,
  `service_tier` as the `/fast`→`priority` latency lever (`--service-tier` /
  `BRO_HARNESS_SERVICE_TIER`), reasoning continuity via
  `include:["reasoning.encrypted_content"]` with encrypted reasoning items
  replayed across turns, model-gated reasoning effort (`minimal`…`xhigh`) plus
  `reasoning.summary`, an SSE per-event idle timeout
  (`BRO_HARNESS_STREAM_IDLE_SECS`), stream/HTTP error-code classification, and a
  one-shot `401`→token-refresh→retry. See
  `design/bro-harness/brodex-responses-deep-dive.md`.
- bro-harness built-in tool surface (`crates/bro-tools`): file read (line-range
  + token cap + optional line numbers), content search (content/files/count
  modes, context lines, case-insensitive), glob (mtime/name sort + result cap),
  edit/write/list, a shell lifecycle quartet (`shell_run`/`shell_poll`/
  `shell_kill`/`shell_list` with cooperative yield-poll, timeouts, stdin/EOF,
  signals, env, and bounded output), git read tools + guarded commit, web fetch,
  and `smart_read`.
- bro-harness durable session side-state: a transport-agnostic `side` cell in
  the session store that survives `exec → resume`, backing a durable
  `todo_write` and the hook nudge ledger.
- bro-harness clipboard + ref ABI (`crates/bro-tools`): session-durable `clip_*`
  registers (`clip_yank`/`clip_paste`/`clip_set`/`clip_list`/`clip_peek`/
  `clip_clear`) that move file slices between locations without the content ever
  transiting the model context — yank/paste/list/set return hashes + counts + a
  short preview, and only `clip_peek` egresses bounded content. Registers ride
  the `side` cell (durable across `exec → resume`) and are byte/count-capped
  with surfaced LRU eviction. The same register store is the chaining substrate:
  `file_read{into}`, `shell_run{stdout_to}`, `web_fetch{into}`, and
  `content_search{into}` produce a register instead of returning output, and
  `file_write{from}` / `shell_run{stdin_from}` consume one instead of inlining
  bytes (the tool-chaining ref ABI, Stages 1–2; Stage 3 pending-ref Tasks
  deferred until an async producer exists). Composable register→register
  transforms narrow/reshape a source server-side without the content entering
  context, propagating kind so they chain (`transform → slice → paste`):
  `clip_transform{from|file,jq}` runs a `jaq` (pure-Rust jq) program over JSON
  (`.body` plucks a field, `map(.title)` reshapes), `clip_slice` takes a
  sub-range (the register analog of `clip_yank`), and `clip_grep` filters lines
  by regex. Each reads its source from **either** a register (`from`) or a
  worktree file (`file`) — the `file` source makes file→transform one call;
  the result lands in a register (`into`, default `@` for a file source).
  Register handles tolerate an optional `clip:` prefix (`clip:a` ≡ `a`);
  `task:` is reserved for pending refs. The clipboard action verbs
  (yank/paste/transform/slice/grep) + `bbox_slice_*` are pinned/always-available
  and the utilities (set/list/peek/clear) stay callable but off the callout;
  tune with `BRO_HARNESS_PIN_TOOLS`.
- bro-harness hook subsystem and Nudger: an internal interception seam
  (user/assistant/tool-result hooks) that contributes ambient guidance steering
  the agent toward the richer blackbox toolbox, with a cache-stable/volatile
  system-prompt split and adopt-or-explain gap-note instrumentation.
- bro-harness design corpus under `design/orchestration/`: a cluster map plus
  tool-surface, clipboard, tool-chaining, hooks, and neuralyze (rewind +
  carry-a-message) designs.
- Vaadin Java refactor toolsuite. Adds read-only view structure, static
  UI/session audit, and route inventory analysis; conservative component,
  grid, dialog, navigation-helper, view-synthesis, and route-access plan
  kinds; plus Vaadin wrapper/workflow atom manifests and refactor eval catalog
  coverage.
- Elixir refactor toolsuite (EX-G1..EX-G19, EX-V1..EX-V6). 19 plan kinds
  dispatchable through `bbox_refactor_plan(kind=...)` covering the BEAM-
  specific shapes that the existing Rust/Java surfaces don't translate:
  multi-clause atom-tag dispatch decomposition (`split_elixir_clauses_by_tag`
  ★ keystone), GenServer concern extraction (single_dispatch_fn and
  per_message_handle_call shapes), defdelegate facade regeneration,
  behaviour adoption, pipe-chain and with-clause extraction, umbrella
  module moves, test fixture extraction, codegen audit, and mix
  compile/credo/dialyzer diagnostic ingestion.
- `elixir-refactor-persona` brofile + 19 atom manifests under
  `system-defaults/atoms/refactor/elixir-*.json` for atom_search
  discoverability and atomic-agent dispatch.
- `sm-refactor-elixir` system memory under `system-defaults/memories/`
  documenting the v1 plan-kind catalog, operator-authority
  acknowledgments, compose-run protocol, and v1-vs-v2 substrate
  decisions.
- Daemon-managed escript helper at `priv/elixir_ast_helper/`
  (`mix escript.build`) exposing `parse_with_comments`,
  `compile_diagnostics`, `format_check`, `ping` over a JSON-RPC stdio
  protocol; targets Elixir 1.15+ for `Code.with_diagnostics/2` support.
- EX-V6 round-trip preservation skeleton (`src/refactor/elixir/roundtrip.rs`)
  wired into every writable Elixir plan kind to enforce parse-clean
  output before the plan returns.
- Repo-owned project knowledge. Project-scoped durable knowledge
  (`bbox_learn` / `bbox_decide` / `bbox_remember` with `scope=project`) now
  persists one file per entry under the owning repo's
  `.bbox/knowledge/<id>.json` and travels with the checkout, instead of living
  only in the host's central store keyed on an absolute path that does not
  survive a different machine, checkout location, or `$HOME`. The committed file
  omits the `project` field — location encodes scope — so it reproduces
  identically on any clone. The daemon loads each registered repo's
  `.bbox/knowledge/` into the query surface at startup and on
  register/rename/unregister, indexes those entries into search, and enqueues
  their embeddings. `bbox_render scope=project` derives deterministically from
  the committed `.bbox/`, which closes the second-machine trap where rendering
  from an empty-for-this-host store would overwrite committed instruction files
  with a near-empty stub. A project becomes repo-owned only once its
  `.bbox/knowledge/` directory exists (created by a clone that carries it, by
  `bbox_project_init`, or by `bbox_project_eject`), so deploying the daemon
  never bulk-migrates every registered repo at boot.
- `bbox_project_eject`: migrate a registered project's existing central-store
  knowledge entries into its committed `.bbox/knowledge/` (one file per entry,
  absolute path scrubbed), with a `dry_run` preview. Opts the project into
  repo-ownership.
- Thread activity→record seam. Promoting or resolving a thread now snapshots a
  scrubbed durable summary into the owning repo's committed
  `.bbox/record/<id>.json` (absolute host paths reduced to `~`; session/bro/task
  identity and live-state fields omitted), and the reindex makes those records
  searchable on a clone where the host-local thread store does not carry them.
  Live threads, side-channel notes, and pins remain host-local operational
  exhaust by design.
- Live refresh of repo-owned knowledge. External changes to a repo's committed
  `.bbox/knowledge/` (a `git pull`, a branch switch, a manual edit) are now
  picked up without a daemon restart. The existing `.bbox/` watcher detects
  committed knowledge create/modify/remove and reloads the in-memory store, so
  `bbox_knowledge` and `bbox_render scope=project` reflect the change
  immediately; a shared dirty flag drives the background reindex thread to
  refresh search on its next pass (within one reindex interval), and the flag is
  set once at startup so changes made while the daemon was down are also indexed.
  The watcher never opens its own search-index writer — the reindex thread stays
  the single writer, so this adds no write contention and leaves `bbox_learn`
  latency unchanged. Knowledge loading is now tolerant of an unreadable/partial
  entry file (skip-and-continue) so an atomic-rename mid-pull cannot leave the
  store partial, and a reload with an absent central `kb.json` resets cleanly so
  deleted repo entries do not linger.
- Recall telemetry no longer churns committed knowledge. `recall_count` /
  `last_recalled` are bumped on every search hit; for repo-owned entries that
  was rewriting the committed `.bbox/knowledge/<id>.json` on each query — git
  churn, and (with live refresh) a self-triggered reload/reindex every search.
  Recall stats now live in a gitignored host-local sidecar
  (`.bbox/local/knowledge-stats.json`, one map per repo) and are merged back
  onto entries at load; the committed file holds durable content only, and a
  recall-only bump produces a byte-identical file that is skipped (no rewrite).
  Ranking (`search/rerank`) still sees recall stats; they survive restart via
  the sidecar. One-time migration: on first save after upgrade, repo-owned
  entries that previously had recall telemetry baked into their committed files
  are rewritten once to strip it (the stats move to the sidecar) — expected, and
  the only churn; steady state is zero.

### Changed

- The `bro` CLI (fleet / tail / council) no longer depends on the `blackbox`
  daemon crate. It links the extracted fleet engine (`bro-fleet-client`), the
  shared transcript parser (`bro-transcript`), and the contract bottom
  (`bro-protocol` + `bro-core`), reaching the daemon only over HTTP. The
  harness–daemon thin-client boundary is now structural and compiler-enforced
  (`design/bro-harness/harness-daemon-boundary.md` §7/§11).
- `bro fleet` is now **daemon-only**: the in-process dispatch fallback is gone,
  so the cockpit always drives the daemon singleton over `/control/*`. With no
  `--daemon-url` (or `BLACKBOX_FLEET_DAEMON_URL`) it defaults to the local daemon
  (`BBOX_PORT`, else 7264). Steer/interrupt now ride the daemon control plane;
  live `set_model` on a fleet session is temporarily unsupported pending the
  control-plane extension.
- Project provider files (`<repo>/{CLAUDE,AGENTS,GEMINI}.md`) and project-scoped
  knowledge are now a one-way projection of the committed `.bbox/`, not a
  bidirectional sync. The system of record for project durable knowledge is the
  repo; the daemon is a derived index over it. `bbox_absorb` remains a
  compatibility no-op — recover hand-authored instruction content with
  `bbox_bootstrap`, then render unidirectionally from the store.

### Fixed

- Bro token-usage reporting now accounts for prompt-cache tokens and is
  consistent across providers. Previously `Usage` carried only
  `{input_tokens, output_tokens}` with per-provider semantics that disagreed
  under one field name: codex reported cumulative, cache-INCLUSIVE input (so a
  cache-heavy session overstated real input load by orders of magnitude — one
  review run reported 7.7M input tokens of which 97% were cache reads), claude
  dropped its cache-read counter entirely, and copilot hardcoded input to 0.
  `Usage` now carries `cached_input_tokens` and `cache_creation_input_tokens`,
  `input_tokens` is normalized to **fresh** (cache-exclusive) input across every
  brodex harness path), and rollups surface the cache breakdown plus the
  cache-inclusive grand total. Token-burn supervision now keys off fresh input
  so a long cached session no longer trips false alerts. `bro-harness` emits the
  Anthropic-native cache counters so harness providers report identically to a
  real Claude CLI run.
- Raw `bro_exec { provider }` (no tier/pin) against a `bro-harness`-backed
  provider (`glm`, `deepseek`, `brodex`) no longer dies silently with exit 1
  and zero events. The harness has no built-in default model (unlike the
  `claude`/`codex` CLIs) and bails when none is passed; the allocator path
  pre-filled a default but the raw path did not. `build_exec_args` now defaults
  these providers to their catalog `.default` model at the single arg-building
  chokepoint, so every dispatch path is covered.
- Harness failure reasons are no longer lost. The dispatch process-waiter now
  joins the stderr reader before snapshotting `inner.stderr`, so a fast
  pre-stream bail no longer races the snapshot and reports an empty `error`. And
  `bro_status` now surfaces a bounded `stderrTail` when a task failed or emitted
  no events, so the diagnostic the operator needs is on the tool they already
  call before declaring a bro dead.
- Edge-index rebuild watcher no longer spins on a dirty worktree. The per-pass
  reindex re-materialized each project's dirty overlay unconditionally (atomic
  rename → fresh mtimes), so the watcher saw byte-identical sidecar "changes"
  and rebuilt the full EdgeIndex (~20s over a multi-GB corpus) every pass,
  pegging CPU and inflating RSS. Materialization is now skipped when a pass
  changed nothing for the project and the on-disk snapshot/overlay already
  matches the current HEAD, indexer/chunker version, and worktree dirty state.
  Fixes #2; incorporates the `*.write-tmp` temp-dir skip from #3 (thanks
  @benstpierre for the report and original fix).
- Watcher signature now folds in the manifest-index, so a branch switch that
  flips the active snapshot pointer between already-materialized snapshots
  (changing no `.jsonl` mtime) is detected instead of silently serving a stale
  graph.
- Tracked-file deletions now purge the deleted file's derived edges from the
  materialized graph (previously only the Tantivy docs were removed).
- A chunker/indexer/parser version bump now forces affected project files to
  re-chunk even when their mtime/size are unchanged, so snapshots are never
  keyed off stale-version edges. Introducing the per-file version stamp adopts
  unknown (pre-existing) versions without a full re-chunk.
- Edge-index rebuild no longer holds the store read-locks (`idx`/`kb`/`threads`/
  `notes`/`task_store`/`roadmap`) while acquiring `edge_index.write()`. Holding
  them across the write created a three-party deadlock (rebuild holds `idx.read`
  wanting `edge_index.write`; the auto-reindex commit queues `idx.write`;
  `bbox_blame` holds `edge_index.read` wanting `idx.read`), which could wedge
  the daemon — every tool taking `kb.write` (e.g. `bbox_knowledge`) blocked
  indefinitely. The rebuilt index is now computed under the read-locks, which
  are dropped before the `edge_index.write()` swap.

### Removed

- NARF and its substrate, superseded by code-mode: the `narf_exec` /
  `narf_prepare` / `narf_run` / `narf_define` / `narf_register` /
  `narf_registerWorkflow` / `narf_scheduleWorkflow` tools, the `bro-script`
  crate (the NARF raw-V8 runtime), and the model-facing `narf_kv_*` KV surface
  with its `KvCapability` trait. (`bro-code-mode` is now the only V8 embedder in
  the process.)
- Durable/scheduled cells (half-baked): server-side cell execution
  (`src/cells.rs`), the `CellRegistryCapability` / `DurableCellCapability`
  capabilities, and the `cell` artifact kind.

### Fixed

- The transcript modality is back in the agentic graph (gap-edc84378). A prior
  commit (ffd9027e) made the Tantivy stored-doc edge projection that produced
  transcript -> session `IN_SESSION` edges opt-in
  (`include_tantivy_projection`), and every caller ended up passing `false`
  (avoiding the deliberate multi-GB-materialization cost), so no caller ever
  ran it: `bbox_describe_schema` reported `transcript: 0 entities`, session
  vertices had zero `IN_SESSION` edges, and `bbox_find_paths`/
  `bbox_inspect_entity` could not hop transcript -> session. Fixed with
  query-time synthesis instead of re-enabling bulk materialization:
  `EdgeIndex::forward_edges_with_synthesis` derives the `IN_SESSION` edge
  straight from a `transcript:` ref's own provider/session_id (a pure
  function, no index lookup) and dedupes against any materialized edge;
  `bbox_inspect_entity` and `bbox_find_paths` both ride it. This is forward
  only: enumerating every transcript chunk in a session from a `session:`
  ref is not a pure function of the ref and stays unsupported.
  `bbox_describe_schema`'s transcript population count now comes from a
  cheap Tantivy `doc_type` count instead of the EdgeIndex (which deliberately
  excludes transcript from its active-graph counts).
- `bbox_inspect_entity` on a `transcript:` ref could 404 even when the doc
  demonstrably existed (hybrid search had just returned it): the property
  lookup (`TranscriptIndex::transcript_properties`) scanned a session's docs
  through a scored, capped collector (`TopDocs::with_limit(500)`) over a
  same-score term query, so any session with more chunks than the cap could
  silently drop the target doc depending on tie-break order. Switched to an
  unscored, unbounded collector over the session's postings so every doc in
  the session is checked. A ref whose (session, byte_offset) matches no doc
  still 404s as before.
- `eval/scripts/refresh_expected_refs.py` no longer just liveness-checks a
  dead `transcript:` expected ref and gives up: it now searches
  `bbox_hybrid_search` (`doc_type=transcript`) for the locator's
  `transcript_hint` and adopts a hit landing back in the SAME session as
  drift repair. A hit only in a different session is reported as a candidate
  re-target for manual review, not auto-adopted (that would silently change
  what the query is provenance for).

## 0.0.1 - 2026-05-14

### Added

- Initial versioned release baseline for `blackboxd`, `bro`, `bro-irc`, and
  `bro-slack`.
- Shared changelog and release process anchored on `Cargo.toml` package version,
  SemVer tags, and GitHub Releases.
