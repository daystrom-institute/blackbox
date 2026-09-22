## Validation

Use the narrowest command that proves the change, then broaden when touching
shared behavior.

Baseline commands:

```bash
cargo check
cargo nextest run --workspace   # mid-cycle gate; quarantines slow tests (.config/nextest.toml)
cargo clippy
```

**These gates assume a WARM checkout** (the base repo or a lane). In a
cold checkout (any manual `git worktree add` or agent-created worktree) do
not run them locally: cold cargo fails loud by design
(`scripts/rustc-cold-guard.sh`, wired via `.cargo/config.toml`;
`BBOX_ALLOW_COLD_BUILD=1` is a deliberate operator override). Done-criteria
for dispatched edit-only work is committed changes with tests WRITTEN, not
locally-run gates; the orchestrator runs all gates lane-side against the
ref (see "Where Heavy Work Runs" and `prompts/agents/edit-only-worktree.md`).

**USE NEXTEST for all test runs, and ALWAYS pass `--workspace`**
(`brew install cargo-nextest`) — do not default to `cargo test`. The root
manifest is workspace+package, so a bare run silently covers the root package
only and drops the ~1,800 tests living in the peeled `bbox-*`/`bro-*` crates.
The fold/closeout gate is the FULL suite:
`cargo nextest run --workspace --profile full` (includes the quarantined slow
tests). Plain `cargo test --lib` is a no-install fallback only: it is
single-process, root-package-only, and ~25x slower wall-clock (~610s vs ~24s
mid-cycle), and the slow-test quarantine and per-test timeouts only apply
under nextest.

Concurrency enforcement (Phase 4, design/daemon-runtime/concurrency-model.md
§5) rides the baseline: `cargo clippy` enforces the `clippy.toml`
disallowed-methods gate (blocking fs/process calls deny in `src/tools/` and
the harness crates; sanctioned actor contexts carry reasoned `#[allow]`s),
and `scripts/lint-concurrency.sh` is the handler-shape backstop — no new
sync `#[tool]` handlers, no thread spawns in tool modules. Run it alongside
clippy when touching MCP handlers.

Targeted recipes:

- Tool docs or MCP adapters: run the relevant tool/module tests and ensure
  `tool_docs.rs` coverage still passes.
- Config, startup, service, or DI-like behavior: `cargo check` is not enough;
  start `blackboxd` or the relevant sidecar and confirm it initializes.
- Render or knowledge changes: run render/knowledge tests and inspect generated
  markdown diffs. Do not hand-edit generated provider memory regions.
- Refactor machinery (harness bindings and the `bbox-refactor` library):
  `cargo nextest run --workspace -E 'package(bbox-refactor) |
  package(bro-harness)'`; for LSP-backed paths also validate the language
  server availability/failure mode.
- Bro admission or event observation changes: run targeted tests and exercise
  the affected HTTP/tool path when behavior depends on runtime state.
- Provider dispatch changes: verify arg construction for the affected provider
  and confirm recursion guard/MCP injection semantics.
- Frontend/site/docs-only changes: run the docs/site build only when that surface
  is touched.

**Isolate binding validation (`isolate` binary).** When working on the harness
isolate bindings (`java.*`, `analysis.*`, `code.*`, `edits.*`, `lsp.*` under
`crates/bro-harness/src/bindings`), validate a tool's behavior locally against
a fixture or a real target root without running the full harness, dispatching a
probe, or writing a Rust test. Build it with `cargo build -p bro-harness --bin
isolate`, then:

```bash
isolate --list                                            # enumerate the surface
isolate --root <dir> --describe <tool>                    # a tool's input schema
isolate --root <dir> <tool> --args '<json>'               # run; pretty-print JSON
isolate --root <dir> <tool> --args-file args.json \
  --field /internal_helper_deps --strict
isolate --root <dir> --cell 'const r = await tools.file_read({file_path:"src/Foo.java"}); text(r);'
isolate --root <dir> --cell-file setup.js --cell-file verify.js
```

`--field <json-pointer>` extracts one result field (avoids a `jq`/`python` pipe);
`--strict` rejects a run whose `file` arg is missing under `--root` (guards the
silent empty-result footgun where a wrong path reads as empty instead of
erroring). It builds a `ToolCx` rooted at `--root` and calls the binding directly
— no agent loop, LLM, or daemon. `--cell` / `--cell-file` evaluates full
code-mode JavaScript cells through the same `exec` runtime a harness consumer
uses, including nested `tools.*` / namespace calls and session-scoped
`store()` / `load()` across repeated cells in one invocation.

A lane-built isolate is linux/amd64 and cannot run on the macOS host. Run
syntax-tier probes pod-side through the cargo shim (`lane-run.sh <lane> --ref
<ref> -- cargo run -p bro-harness --bin isolate -- --root <lane path> ...`);
`lsp.*` probes run host-side and want a lane root with a warm RA build-data
cache (cold-start on this workspace exceeds the default readiness budget).

Do not record exact test counts in this file. Counts stale quickly and do not
help an agent choose the right validation.

## Where Heavy Work Runs

On the operator's estate, the dev machine is the control plane, not the
compute tier: it runs agent sessions, the daemon, editing, and the tight
per-crate loop (`cargo check -p <crate>`, targeted nextest) - and not much
else. macOS code-signing assessment makes churning through freshly built
test binaries pathologically slow locally (every workspace test run mints
~50 never-seen binaries; each first exec waits on syspolicyd), while the
cluster runs them at rated speed. Reach for the cluster-backed tooling BY
DEFAULT; the operator-local overlay repo `~/repos/bbox-cage` owns it (its
`build/README.md` is the runbook):

- **Full verification of a pushed ref** (workspace nextest full profile,
  clippy, concurrency lint) runs on the cluster:
  `~/repos/bbox-cage/build/submit-bbox-verify.sh --ref <ref>`. Local
  full-suite runs are the fallback, not the default, and running someone
  else's ref locally is an anti-pattern.
  - Reading results: the `--watch` submit exits 0 regardless of outcome;
    the workflow phase in its printed step tree is the verdict. On any red,
    triage with `~/repos/bbox-cage/build/verify-triage.sh <workflow-name>`
    first - never hand-roll log pipelines per incident.
  - A red at a sha whose lane gates are green is not presumptively code:
    resubmit the same sha (red-green = flaky test; deterministic red-red =
    environmental), and check whether a base regen landed between the last
    green and first red - the cluster's warm-base rotation can mint bad
    artifacts that every subsequent clone inherits. The remedy table in
    `~/repos/bbox-cage/build/README.md` covers both cases.
- **linux/amd64 images** build on the cluster:
  `~/repos/bbox-cage/build/submit-bbox-build.sh --ref <ref>` (native amd64
  in a warm ZFS clone; QEMU emulation and controller-host docker builds are
  legacy fallbacks).
- **Deploying the built image to the cage is one command**:
  `~/repos/bbox-cage/scripts/converge.sh --image <tag>` (tag as printed by
  the build submit; the script resolves the immutable digest, assembles the
  operator-local stack env, and runs `pulumi up`; `--preview` dry-runs).
  There is NO local corpus daemon to restart: this host runs only the
  checkout-bound code collectors plus fleetd, and
  blackbox.daystrom.app serves everything. Native provider history is collected on its source host by
  `bbox-transcript-collector`, using a dedicated producer grant and the native
  transcript transport. See `docs/native-transcript-collector.md` for enrollment,
  backfill, and macOS signing/Local Network requirements. Indexed drill-down
  alone does not prove a current background scan; inspect source freshness.
- **Interactive heavy worktrees are lanes, not local disk**: claim a warm
  standby lane in seconds (`~/repos/bbox-cage/build/lanes/lane-pool.sh
  claim` prints the checkout path), or create a named one from the
  operator's estate root (`bin/estate lane create <name> --family bbox`,
  ~5 min to full warmth). Either way the checkout lives at
  `~/lanes/<name>/blackbox`; cargo, rustc, and sccache route into a builder
  pod automatically, keyed on cwd. Read the lane contract
  `~/repos/bbox-cage/build/lanes/BBOX_LANE_WORK.md` before heavy work.
  Worker loss is lane loss - push anything durable.
- **Formatting is project-pinned, not host-pinned**: use
  `scripts/fmt.sh` / `scripts/fmt.sh --check` in both local and lane
  checkouts. The wrapper selects the same exact rustfmt on macOS and Linux;
  do not substitute the moving `cargo +stable fmt` alias.
- **What stays local**: file edits, single-crate checks and tests in a WARM
  checkout, and the arm64 macOS daemon binary build/deploy (launchd) - the
  cluster produces Linux artifacts only. Warmth is the discriminator, not
  task size: a fresh `git worktree add` makes any build cold, and its first
  build/test run is a 20+ minute full-dependency compile plus syspolicyd
  assessment of every fresh binary - that is lane work, not local.
  `fleet.json` `project_dispatch.seed_dirs` covers only cockpit- and
  workflow-created worktrees, never manual `git worktree add`.

**Dispatch propagation**: when orchestrating subordinate agents into heavy
blackbox work, claim a pool lane (or create one), pass the printed path as
the dispatch cwd, and put ONE line in the prompt: read `BBOX_LANE_WORK.md`
(path above) before heavy work. Release or destroy the lane when the
dispatch concludes. Restate situational constraints in the dispatch prompt;
older worktrees carry older copies of this file.

Contributors without the operator's estate: everything above degrades to
the plain local commands in Validation; nothing in the repo depends on the
cluster.

