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
  `file_read{into}` and `shell_run{stdout_to}` produce a register instead of
  returning output, and `file_write{from}` / `shell_run{stdin_from}` consume one
  instead of inlining bytes (the tool-chaining ref ABI, Stages 1–2; Stage 3
  pending-ref Tasks deferred until an async producer exists). `clip_*` are
  pinned/always-available; tune via `BRO_HARNESS_PIN_TOOLS`.
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

### Changed

### Fixed

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
