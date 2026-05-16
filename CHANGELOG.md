# Changelog

All notable changes to Blackbox are documented here.

This project uses Semantic Versioning. Until the public API and operator
workflows stabilize, `0.y.z` releases may include breaking changes; call those
out explicitly under `Changed` or `Removed`.

## Unreleased

### Added

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

### Removed

## 0.0.1 - 2026-05-14

### Added

- Initial versioned release baseline for `blackboxd`, `bro`, `bro-irc`, and
  `bro-slack`.
- Shared changelog and release process anchored on `Cargo.toml` package version,
  SemVer tags, and GitHub Releases.
