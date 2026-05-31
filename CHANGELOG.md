# Changelog

All notable changes to Blackbox are documented here.

This project uses Semantic Versioning. Until the public API and operator
workflows stabilize, `0.y.z` releases may include breaking changes; call those
out explicitly under `Changed` or `Removed`.

## Unreleased

### Added

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
  enforcement.
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
  provider (claude, codex, copilot, gemini, opencode, and the glm/deepseek/
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

## 0.0.1 - 2026-05-14

### Added

- Initial versioned release baseline for `blackboxd`, `bro`, `bro-irc`, and
  `bro-slack`.
- Shared changelog and release process anchored on `Cargo.toml` package version,
  SemVer tags, and GitHub Releases.
