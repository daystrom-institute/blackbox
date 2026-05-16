# elixir_ast_helper

Daemon-managed escript helper for blackbox's Elixir refactor surface.

## Why

Per `design/refactor-tools/elixir/refactor-elixir-expansion.md` Open Question 1
(resolution: daemon-managed escript with project-root pinning), three plan
kinds depend on Elixir-native operations the Rust daemon cannot perform
directly:

- `elixir_compile_fix_round` — needs `Code.with_diagnostics/2`
- `elixir_credo_fix_round` — needs Credo's JSON output parsed in-process
- `elixir_dialyzer_attribution` — needs dialyzer warnings mapped to defs

Plus EX-V6 round-trip preservation needs
`Code.string_to_quoted_with_comments!/2`.

## Build

```bash
cd priv/elixir_ast_helper
mix deps.get
mix escript.build
# produces ./elixir_ast_helper executable
```

The daemon builds this once per registered project root and caches the
escript at `$BLACKBOX_STATE_DIR/elixir_helpers/<project_id>/elixir_ast_helper`.

## Protocol

One JSON request per line on stdin; one JSON response per line on stdout.

```jsonc
// → {"id": "abc", "cmd": "ping"}
// ← {"id": "abc", "ok": true, "result": "pong"}

// → {"id": "def", "cmd": "parse_with_comments", "args": {"source": "..."}}
// ← {"id": "def", "ok": true, "result": {"quoted": "...", "comments": [...]}}

// → {"id": "ghi", "cmd": "compile_diagnostics", "args": {"path": "lib/foo.ex"}}
// ← {"id": "ghi", "ok": true, "result": {"result": "...", "diagnostics": [...]}}
```

Errors come back as `{"id": id, "ok": false, "error": "..."}`.

## Commands (v1)

- `ping` — health check
- `parse_with_comments` — EX-V6 writable-lane parse
- `compile_diagnostics` — for EX-G11 `elixir_compile_fix_round`
- `format_check` — `mix format --check-formatted` analogue

v2 commands: `credo_diagnostics`, `dialyzer_diagnostics`.

## Lifecycle

The Rust daemon (`src/refactor/elixir/helper.rs`) launches one helper per
project root, reuses it across requests, and recycles on project version
change (detected via `mix.lock` hash). Cold start is ~1.5s; per-request
latency is ~10ms.
