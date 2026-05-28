# Changelog

All notable changes to Blackbox are documented here.

This project uses Semantic Versioning. Until the public API and operator
workflows stabilize, `0.y.z` releases may include breaking changes; call those
out explicitly under `Changed` or `Removed`.

## Unreleased

### Added

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

### Removed

## 0.0.1 - 2026-05-14

### Added

- Initial versioned release baseline for `blackboxd`, `bro`, `bro-irc`, and
  `bro-slack`.
- Shared changelog and release process anchored on `Cargo.toml` package version,
  SemVer tags, and GitHub Releases.
