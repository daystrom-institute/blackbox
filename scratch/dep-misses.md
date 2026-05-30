# Environment & dependency misses

Catalog of environment/toolchain gaps and test-architecture fragilities found
while stabilizing the test suite on a fresh macOS (darwin, aarch64) dev machine.
Date: 2026-05-29. Worktree: `.claude/worktrees/bugfixes`.

Status legend: ✅ resolved · 🟡 partial / workaround · 🔴 open · 📋 investigate

---

## 1. Toolchain inventory (this machine)

| Tool | State | Notes |
|---|---|---|
| `cargo`/`rustc` | ✅ present | stable aarch64-darwin |
| `rust-analyzer` | ✅ present (1.96.0) | Rust LSP-backed refactor kinds work |
| `node` | ✅ present (v26) | |
| `jq` | ✅ present (1.7.1) | RTK hook dependency |
| `jdtls` | ✅ installed this session | brew 1.58.0; see §2 |
| JDK (`java`) | 🟡 keg-only | openjdk 26 via brew; see §2 |
| `elixir`/`mix` | 🔴 missing | NOT the cause of elixir test failures — see §4 |
| `go` | 🔴 missing | tree-sitter-go is compiled in; only needed if a test shells out to `go` (none found so far) |

---

## 2. jdtls / JDK — ✅ resolved (with one optional follow-up)

**Symptom:** Java LSP-backed refactor kinds (`rust_*`/jdtls-gated paths) and any
test that spawns the Java language server had no server to talk to. `java` /
`javac` resolved only to the macOS `/usr/bin/java` stub ("Unable to locate a Java
Runtime").

**Resolution:**
- `brew install jdtls` → `/opt/homebrew/bin/jdtls` (1.58.0), which pulled
  `openjdk` 26.0.1 (keg-only at `/opt/homebrew/opt/openjdk`).
- The brew `jdtls` wrapper **self-locates the JDK**: it hardcodes
  `JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk/libexec/openjdk.jdk/Contents/Home}"`.
  So jdtls runs without `java` on PATH.
- blackbox launches it via `LspSessionManager`, which reads `BLACKBOX_JDTLS_BIN`
  and **defaults to `"jdtls"`** (`src/lsp/session_manager.rs:672`). Since jdtls is
  on PATH, blackbox auto-detects it — no config needed.

**Optional follow-up (🟡, needs sudo — not done):** bare `java`/`javac` still hit
the non-functional system stub because openjdk is keg-only and unsymlinked. Only
needed if something invokes `java` directly (jdtls does not). To enable system-wide:
```
sudo ln -sfn /opt/homebrew/opt/openjdk/libexec/openjdk.jdk \
  /Library/Java/JavaVirtualMachines/openjdk.jdk
```
Left for the operator (privileged op). Not required for jdtls or the daemon.

---

## 3. Canonical-path test bugs (macOS `/var` → `/private/var`) — 🔴 open (dominant)

**Symptom:** Tests build paths from `tempfile::tempdir().path()` (which on macOS is
under `/var/folders/...`) and assert them against values the code under test has
**canonicalized** (to `/private/var/folders/...`). `starts_with` / `contains` /
exact-equality then miss. Linux CI hides this because `/var` is already canonical
there.

**Confirmed instances:**
| Test | File | Failing assert |
|---|---|---|
| `watcher_installs_atomic_rename` | `src/watcher.rs` | ✅ **fixed** this session (canonicalize tempdir) |
| `watcher_deletion_marks_removed_not_deleted` | `src/watcher.rs` | ✅ **fixed** |
| `project_init_creates_bbox_skeleton` | `src/tools/projects.rs:425` | `result.created.contains(non-canonical cfg_path)` |
| `project_init_is_idempotent_without_force` | `src/tools/projects.rs:451` | `result.skipped.contains(...)` |
| `project_init_force_overwrites_skeleton_files` | `src/tools/projects.rs:476` | `result.created.contains(...)` |
| `containment_accepts_relative_existing_file` | `src/macros/probe.rs:1620` | `result.unwrap().starts_with(dir.path())` |
| `annotate_symbol_marks_local_vs_jdt` | `src/macros/probe.rs:1805` | local file not classified `project_local=true` |

**Root cause:** the *test* is wrong, not the code. The code correctly canonicalizes;
the test compares against the raw tempdir path.

**Fix pattern (applied to watcher, to roll out):** canonicalize the temp root at
the top of the test — `let root = dir.path().canonicalize().unwrap();` — and derive
all expected paths from `root`. A shared `tests` helper
(`fn canonical_tempdir() -> (TempDir, PathBuf)`) would prevent regressions.

**Likely more:** any test across the suite using `tempdir().path()` + a code path
that canonicalizes. The clean full-suite baseline (running) will surface the full
set.

---

## 4. `tree-sitter-language-pack` broken on this host — 🔴 open (DOMINANT remaining failure source)

**This is the single biggest remaining cause — ~44 of the ~50 non-env failures.**
It is NOT an installable env-binary miss; it is a broken Cargo dependency.

**Two failure modes, same root (the language pack):**
- **`get_parser("elixir")` returns Ok but the grammar mis-parses** valid source →
  `refactor::elixir::*` (40) + `chunker::code::…elixir…` (1).
  e.g. `parse_clean_accepts_valid_output`: `verify_parse_clean("defmodule Foo do …
  end").is_ok()` returns `Err` (valid Elixir produces parse-error nodes).
- **`get_parser("toml")` / `get_language("elixir")` returns `Err` outright** →
  "unsupported language toml" / "unsupported language elixir":
  - `refactor::tests::ensure_toml_table_*` (2) — `apply()` errors with
    `unsupported language toml`.
  - `code_nav::tests::test_code_query_uses_language_pack_for_mapped_languages` (1) —
    `unsupported language elixir`.
  - `code_nav::tests::code_refs_unsupported_language_typed_error_for_non_identifier_kind` (1, likely).

**Symptom (elixir example):** `verify_parse_clean("defmodule Foo do … end").is_ok()`
returns `Err`.

**Root cause (hypothesis):** Elixir is **not** parsed by shelling out to
`elixir`/`mix`; it uses a compiled tree-sitter grammar loaded from the language
pack via `tree_sitter_language_pack::get_parser`/`get_language("elixir")`
(`src/chunker/code.rs`, imports at `:7`). The grammar loads but mis-parses valid
source, which smells like an **ABI/version mismatch**. Resolved versions
(Cargo.lock): **`tree-sitter 0.26.8`** vs **`tree-sitter-language-pack 1.8.0-rc.26`**
(a **pre-release** RC). Installing `elixir`/`mix` will **not** fix it — the binary
is never invoked.

**Scope:** ~41 tests (`refactor::elixir::*` 40 + `chunker::code::…elixir…` 1). All
other languages parse fine because they use **pinned** grammar crates in
`parser_for_language` (`src/chunker/code.rs:142`: rust/python/java/go/ts/js/c/cpp/
csharp), NOT the language pack — elixir is the only language sourced from the pack
in the failing tests.

**Next steps (own, validated effort — NOT done here; changing a core grammar dep
affects every pack-loaded language):**
- (a) Confirm whether these pass on Linux CI (would isolate to an
  aarch64-darwin pack build vs a universal grammar/ABI break).
- (b) Add a dedicated `tree-sitter-elixir` crate pinned to a tree-sitter-0.26-ABI
  release and route `"elixir"` through `parser_for_language`'s explicit match
  (same pattern as the other pinned grammars), instead of the RC language pack.
- (c) Or bump `tree-sitter-language-pack` off the `-rc` to a stable line and
  re-run the full grammar matrix.

---

## 5. Parallel-execution fragility (the multi-agent ask) — 🔴 open

Two distinct failure modes when work runs in parallel:

### 5a. Within one `cargo test` process — global env races
- **30 source files** mutate **process-global** `std::env::set_var` in tests
  (HOME, XDG_*, BLACKBOX_*). Rust runs tests multi-threaded; one test's `set_var`
  races with another thread reading the same var. Manifests only at **full-suite
  scale** (modules pass in isolation — `util` 7/7 alone, but fail in the 3000-test
  run).
- `serial_test` is **not** a dependency.
- **✅ RESOLVED this session** (chose option 2 — the repo already had a
  `test_env_lock()` mutex convention, 12 files used it, 14 didn't):
  - Made `test_env_lock()` **poison-tolerant** (`unwrap_or_else(|p| p.into_inner())`)
    so a panicking env test no longer cascades a poisoned-mutex panic into every
    later env test.
  - Added a **`TestEnvGuard` RAII** helper (`src/util.rs`) that holds the lock AND
    restores every touched var on drop (can't leak even on panic).
  - Audited `set_var`/`remove_var` across the tree and added the lock to every
    env-mutating test that lacked it (~26 tests / 14 files). Note the audit must
    match BOTH `env::set_var` and bare `set_var` (via `use std::env::set_var`) —
    the first grep missed the bare form.
  - INVARIANT going forward: any test touching process env must hold
    `test_env_lock()` (directly, via `TestEnvGuard`, or via a helper that does).
    Watch for **reentrancy**: the mutex is non-reentrant, so a test must not take
    the lock if a helper it calls (e.g. `with_state_dir`) already does.

### 5b. Across agents — shared real filesystem / services
Multiple agents each run `cargo test` in their **own worktree** (separate process,
separate `target/`, separate env — so 5a does NOT cross agents). They still collide
on **shared host state**:
- Real `$HOME` / XDG dirs (`~/.config/blackbox`, `~/.local/state/blackbox`,
  `~/.claude*`, the tantivy index) — any test that reads/writes real paths instead
  of an isolated tempdir is a cross-agent landmine.
- The running prod daemon on `127.0.0.1:7264`.
- Fixed ports — **mostly already mitigated**: server/embed/system-event tests bind
  `127.0.0.1:0` (ephemeral). One env reference at
  `src/orchestration/providers/tests.rs:48` sets `BLACKBOX_MCP_URL=…:7264` (env, not
  a bind — falls under 5a).
- **Fix:** guarantee every test isolates to a per-test tempdir and never touches
  real shared paths or the prod daemon. `SharedState::for_test(tempdir)` already
  exists (`src/server/state.rs:277`) — the pattern to standardize on.

### Concurrency evidence
The first full-suite run reported **128 failures** while a second `cargo` process
(the fleet-tui branch) ran concurrently in the main worktree. Re-running modules in
isolation: `secrets` 13/13 and `util` 7/7 **passed** — confirming most of those 128
were 5a/5b contention, not real defects. A clean baseline (no concurrent agent) is
running to get the true within-process failure set.

---

## 6. Summary of actions

| Item | Status |
|---|---|
| Install jdtls + JDK | ✅ done (`brew install jdtls`, auto-detected) |
| System `java` symlink | 🟡 optional, needs operator sudo |
| **Global-env test races** | ✅ **fixed** — poison-safe `test_env_lock` + new `TestEnvGuard` RAII (`src/util.rs`) + guarded ~26 tests across 14 files. Removed **~75** full-suite failures. |
| Canonical-path test bugs | ✅ **fixed** — watcher (2), project_init (3), probe (2), plan_slot (1), find_usages (1) all canonicalize the tempdir root |
| macOS config-dir test (`malformed_config_file_errors`) | ✅ **fixed** — write to `default_config_path()` not hardcoded `~/.config` |
| Hardcoded `/home/invidious/...` (`rust_public_api`) | ✅ **fixed** — use `env!("CARGO_MANIFEST_DIR")` |
| **`tree-sitter-language-pack` broken** (elixir + toml + code_nav) | 🔴 **open, DOMINANT** (~44 failures) — see §4; needs dep surgery |
| `eval_check::all_30_manifests` | 🔴 data dep — needs a populated transcript index/corpus (not present in fresh env) |
| Cross-agent fs isolation | 🟡 mostly OK (`SharedState::for_test` + tempdirs; ephemeral ports) |
| `go` toolchain | 🔴 low priority (compiled grammar; no shell-out found) |

### Test-suite impact (clean baseline, no concurrent agent)

| | Before | After |
|---|---|---|
| Full `cargo test --lib` failures (clean, no concurrent agent) | **127** | **48** |
| of which: env races | ~75 | **0** |
| of which: canonical / platform / hardcoded | ~9 | **0** |
| of which: `tree-sitter-language-pack` (elixir/toml/code_nav) | ~45 | 45 (dep issue, untouched) |
| of which: data-dep (`eval_check::all_30_manifests`) | 1 | 1 |
| of which: residual non-env flaky | — | 2 |

**Residual non-env flaky (🟡, low priority):**
`orchestration::allocator::…probe_quota_capacity…` and
`packets::store_tests::load_resolves_domain_prefix_to_latest` each **pass in
isolation** but fail ~intermittently under full multi-threaded load. They are NOT
env races (allocator already holds `test_env_lock` via `with_provider_bins`), so
some *other* shared state races (a global static, an on-disk packets dir, or
timing). Not regressions from this session's work; needs a separate look. The
remaining real blocker to a green suite is §4 (the language pack).
