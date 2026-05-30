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
exact-equality then miss. Linux hides this because `/var` is already canonical
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
  - `ensure_toml_table*` (2, in `src/refactor/tests.rs:3608` — e.g.
    `ensure_toml_table_adds_lib_table`) — `apply()` errors with
    `unsupported language toml`. NOTE: these live at the crate-test root, **not**
    under a `refactor::tests::` module. Filtering by `cargo test --lib
    refactor::tests::ensure_toml_table` silently matches **zero** tests and looks
    like a pass; use the bare filter `cargo test --lib ensure_toml_table`.
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
- (a) ✅ **DONE — isolated to aarch64 (ARM64).** See the cross-env validation matrix
  below. FAILS on aarch64-darwin AND aarch64-linux (Docker on this Mac); PASSES only
  on x86_64-linux. Since aarch64-linux **also fails**, the failing axis is the
  **architecture (aarch64)**, not the OS — "macOS-local" was wrong. Next root-cause
  work is now scoped to why the pack fails to register grammars on aarch64 (§4b).
- (b) Add a dedicated `tree-sitter-elixir` crate pinned to a tree-sitter-0.26-ABI
  release and route `"elixir"` through `parser_for_language`'s explicit match
  (same pattern as the other pinned grammars), instead of the RC language pack.
  CAUTION: this was tried and reverted — it hung the whole-crate indexer test at
  100% CPU (see §7). Not a blind pin.
- (c) Or bump `tree-sitter-language-pack` off the `-rc` to a stable line and
  re-run the full grammar matrix.

### Cross-env validation matrix (2026-05-30)

The three rows below LOOK like an OS/arch signal but are actually a **parser-cache
warmth** confound — see the root cause:

| Environment | arch | OS | parser cache | `refactor::elixir` | `ensure_toml_table` | `code_nav` |
|---|---|---|---|---|---|---|
| macOS (this Mac)            | aarch64 | darwin | cold | **40 failed** / 13 pass | **2 failed** | **1 failed** |
| Linux x86_64 (Manjaro 6.12) | x86_64  | linux  | **warm** | **53 pass**         | **2 pass**   | **1 pass**   |
| Docker `rust:bookworm`      | aarch64 | linux  | cold | **40 failed** / 13 pass | **2 failed** | **1 failed** |

**ROOT CAUSE (2026-05-30, confirmed from crate source + a live `curl` 404): a stale
prerelease pin whose runtime parser-download manifest 404s. It is NEITHER OS- nor
arch-specific.**

- We pin `tree-sitter-language-pack = "1.8.0-rc.26"` (`Cargo.toml:57`), default
  features → `download` + `dynamic-loading`. With `TSLP_LANGUAGES` unset, the pack's
  build compiles **no grammars in** (generated `STATIC_LANGUAGES` and
  `DYNAMIC_LANGUAGE_NAMES` are both empty). rust/python/java/go/ts/js/c/cpp/csharp
  still work only because blackbox pins **separate** grammar crates for them
  (`Cargo.toml:50-60`); **elixir/toml/erlang are sourced ONLY from the pack** and have
  nothing built in.
- At runtime `get_language("elixir")` misses the in-process registry and falls to the
  download slow-path (`registry.rs:122-135`), which GETs
  `…/releases/download/v1.8.0-rc.26/parsers.json` → **HTTP 404** (verified by curl).
  No GitHub release with parser assets exists for this crates.io RC. The 404 happens
  **before** the platform-key lookup (`download.rs` `platform_key()` correctly emits
  `linux-aarch64` / `macos-arm64`), so the failure is **architecture-independent** —
  aarch64 IS supported in code.
- The matrix is confounded by cache state: the only PASSING row (x86_64 Manjaro) had a
  **warm** cache at `~/.cache/tree-sitter-language-pack/v1.8.0-rc.26/libs/*.so` from a
  prior provisioning. The Mac and Docker rows were **cold**. A *cold x86_64* box would
  fail identically — the dead manifest is arch-blind.

⚠️ **Two earlier verdicts in this section's history — "Darwin-specific", then
"aarch64-specific" — were premature and are RETRACTED.** Each was written before the
decisive evidence (the Docker run, then the source trace) came back. The source path +
404 is ground truth: cold cache + dead `parsers.json` manifest for a stale RC.

**FIX — CONFIRMED end-to-end on a COLD aarch64-linux cache (2026-05-30).** The
download URL is `…/v{PACK_VERSION}/parsers.json`. Manifest probe (curl, after the
GitHub signed-asset redirect):

| pack version | `/v{ver}/parsers.json` |
|---|---|
| `1.8.0-rc.26` (what we pin) | **404** ← the bug |
| `1.8.0` | **200** |
| `1.8.1` | **200** |
| `1.9.0-rc.1 / rc.10 / rc.17` | **200** |

(My earlier note that 1.8.0/1.8.1 "also 404" was WRONG — they 200. Only the pinned
rc.26 is dead.) Every live manifest covers aarch64: platform keys are
`linux-aarch64, linux-x86_64, macos-arm64, macos-x86_64, windows-aarch64, windows-x86_64`.

**Proven fix:** pin **`tree-sitter-language-pack = "=1.8.0"`** (exact — plain `"1.8.0"`
resolves to 1.8.1 via caret, which breaks compile; see API note). On a fresh
aarch64-linux Docker container (cold cache, runtime download active) the three groups
**PASS in ~5s total**: elixir 53/0, toml 2/0, code_nav 1/0. No hang.

⚠️ The earlier "bump unmasks the §7 hang" hypothesis is **RETRACTED** — the 75-min
first attempt was `cp -a /src` copying the huge darwin `target/` into the container +
slow colima, NOT a parse hang. The clean run (excluding `target/`) compiles and parses
fine; `1.8.0` does not hang.

**API constraint — why not just "latest":** `1.8.1` and `1.9.0-rc.17` have live
manifests but are **NOT source-compatible**: `get_parser` now returns
`tree_sitter_language_pack::Parser` instead of `tree_sitter::Parser`, so the current
code fails to compile at `src/chunker/code.rs:139`
(`expected tree_sitter::Parser, found tree_sitter_language_pack::Parser`). So:
- **Zero-code-change fix (recommended now):** `= "1.8.0"` (stable, live manifest,
  API-compatible, passes cold on aarch64).
- **Forward/idiomatic fix:** bump to latest stable `1.8.1` (or the `1.9.0-rc` line) AND
  adapt the `get_parser` call site(s) to the new `tree_sitter_language_pack::Parser`
  type — small, localized change at `chunker/code.rs:139` (+ any sibling call sites).

Provisioning note: default features (`download` + `dynamic-loading`) ARE enabled — we
DO use runtime download + `~/.cache/tree-sitter-language-pack/v{ver}/libs/` caching.
For a daemon, the more robust idiomatic alternative is a build-time static compile via
`TSLP_LANGUAGES=elixir,toml,erlang` (no runtime network) — UNVERIFIED here (need to
confirm the crates.io package ships parser C sources; build.rs compiles from a
`parsers/` dir that may not be in the published package). The dedicated-grammar-crate
pin (§4b/§7) stays off the table (it hung the indexer).

Validation was run by a `codex` sidekick bro (the Claude Code CLI session had no LAN
route, and Docker/brew install is heavy work better offloaded to a native env).

⚠️ Process note: an earlier revision of this section asserted "Darwin-specific" — that
was written *before* the Docker result came back and was wrong. The experiment
existed precisely to settle this; trust the matrix, not the prior prose.

### §4a. Validation options for the aarch64 theory  — ⚠️ SUPERSEDED

> This section is kept for history but its premise is **obsolete**. The root cause is
> NOT an arch issue (see the ROOT CAUSE block above): it's the dead `v1.8.0-rc.26`
> download manifest, proven by pinning `=1.8.0` and passing cold on aarch64-linux. The
> "is it Darwin or aarch64" framing below was answered by finding the real mechanism,
> not by the OS/arch matrix. Disregard the experiment design here.

Goal: get a **single-variable** comparison by holding everything constant except
one axis. Two clean experiments would settle it:

- **aarch64-Linux** (the decisive one): if the pack works on aarch64-Linux →
  failure is **Darwin-specific** ("macOS-local" is the right label). If it fails
  there too → it's an **aarch64 pack-build** problem and "macOS-local" is wrong.
  Cheapest route: Docker. `docker run --rm -v "$PWD:/w" -w /w rust:1-bookworm
  cargo test --lib refactor::elixir` on this Apple-silicon Mac runs an
  **aarch64-linux** container natively (no emulation) — that is exactly the
  missing cell. (Mount a clean checkout; expect a cold `cargo` compile.)
- **x86_64-Darwin** (the other axis, harder): would isolate the arch on macOS, but
  Apple dropped Intel Macs and Rosetta only translates userland x86 binaries, not
  a different-arch toolchain target cleanly — not worth it. The aarch64-Linux
  experiment alone is sufficient to disambiguate, because combined with the two
  rows above it pins the responsible axis.

Docker viability notes:
- Docker Desktop on Apple silicon runs **arm64 (aarch64) Linux** containers by
  default — so `rust:1` without `--platform` IS the aarch64-Linux test.
- To ALSO get the x86_64-Linux confirmation locally (redundant with the bro run
  but fully controlled at the *same commit*), add `--platform linux/amd64`; that
  path uses qemu emulation (slow, but fine for a one-off).
- Caveat: the indexer-hang failure mode in §7 is tree-sitter-cursor CPU spin — a
  container inherits the same grammars, so if it's an aarch64 ABI break the
  container reproduces it; if it's Darwin-only the container is green.

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
timing). Not regressions from this session's work; needs a separate look.

---

## 7. Session postmortem — what shipped, what was abandoned

**Shipped (merged):** the env-race fix, canonical/platform/portability test fixes,
the watcher scoping fix, the `.bbox` project-file indexing fix, and clippy-clean.
All cross-platform-safe and verified (127 → 48 local failures, the remaining 48
being env-specific — see below).

**The remaining local failures are environment-specific, NOT code bugs:**

- **§4 language-pack failures are aarch64 (ARM64)-SPECIFIC (confirmed 2026-05-30).**
  `tree-sitter-language-pack` returns `Err` for every non-pinned grammar **on this
  host** (elixir/toml/erlang), but the same code PASSES on x86_64-linux — not a
  defect in the shared code. Pinned down across three environments (see §4 matrix):
  fails on aarch64-darwin AND aarch64-linux (Docker), passes only on x86_64-linux.
  Because the aarch64-linux container **also fails**, the responsible axis is the
  **architecture (aarch64)**, NOT the OS. The original "macOS-LOCAL" wording was
  wrong — the Mac is just one instance of an ARM64 host. Root cause is in the pack's
  aarch64 grammar-registration path — see §4b for the next investigation step.

- **Grammar-pin attempt was made and REVERTED.** I tried pinning dedicated
  `tree-sitter-elixir` / `tree-sitter-toml-ng` / `tree-sitter-erlang` crates and
  resolving them before the pack. It fixed the ~45 tests locally **but**:
  1. It changes behavior on Linux (where the pack already works) — wrong fix for a
     host-local env problem.
  2. It made the whole-crate indexer test (`registered_project_markdown_and_rust_source_are_searchable`)
     **hang at 100% CPU**, because pinning the grammars made `CodeChunker` AST-walk
     real `.toml`/`.ex` files in the crate, and the new grammars' cursor walk
     spins (a tree-sitter-0.26 ABI mismatch a Rust-level node-budget can't
     interrupt). So the pins are unsafe for the indexer. → reverted.
  If a real fix is wanted later: it belongs at the env/toolchain layer (why is the
  pack broken on aarch64-darwin?), or with grammar crates verified ABI-compatible
  with tree-sitter 0.26 **and** proven not to hang the indexer — not a blind pin.

- **The index-test hang is environmental (this worktree), not code.** At commit
  `45bcb1a` (no grammar crates — identical code to an earlier run that PASSED it),
  the test now hangs in **tantivy indexing threads** (`sample` shows
  `IndexWriter::add_indexing_worker` + merge threads all busy). Same code, different
  result ⇒ accumulated worktree/daemon/`target` state, not a regression. It passes
  in a clean checkout.

- **`eval_check::all_30_manifests`** needs a populated transcript corpus (a
  `transcript:<account>:<session>:…` ref doesn't resolve in a fresh checkout). Run
  with `--ignored`/with-corpus, or gate it. (Quarantine + the allocator one were
  prepared but live in the dropped grammar commit; re-apply if desired.)

**Net:** merged the solid, portable stabilization; abandoned the grammar pins as
the wrong layer for a host-local env problem.
