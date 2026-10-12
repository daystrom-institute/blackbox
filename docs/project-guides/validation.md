## Validation

Use the narrowest command that proves the change, then broaden when touching
shared behavior.

Baseline commands:

```bash
cargo check
cargo nextest run --workspace   # mid-cycle gate; quarantines slow tests (.config/nextest.toml)
cargo clippy
```

**These gates assume a WARM checkout** (a checkout with a populated build
cache, typically the base repo or a lane). In a cold checkout (any manual
`git worktree add`, agent-created worktree, or fresh clone) do not run them
locally. The repository enforces one case mechanically:
`scripts/rustc-cold-guard.sh`, wired via `.cargo/config.toml`, refuses to
compile in a linked worktree on macOS that has no cached `.rlib` files
(`BBOX_ALLOW_COLD_BUILD=1` is a deliberate operator override). The policy is
broader than the guard: a checkout the guard lets through is not thereby
cheap to build. Done-criteria
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
provider adapter crates; sanctioned actor contexts carry reasoned `#[allow]`s),
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
- Bro admission or event observation changes: run targeted tests and exercise
  the affected HTTP/tool path when behavior depends on runtime state.
- Provider dispatch changes: verify arg construction for the affected provider
  and confirm recursion guard/MCP injection semantics.
- Frontend/site/docs-only changes: run the docs/site build only when that surface
  is touched.

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
    resubmit the same sha to test reproducibility, read the failing test's
    evidence, and check whether a base regen landed between the last green
    and first red - the cluster's warm-base rotation can mint bad artifacts
    that every subsequent clone inherits. A repeat failure alone does not
    establish an environmental cause, and a pass on retry does not establish
    which test is flaky. The remedy table in
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
  checkout, and the arm64 macOS satellite binary builds and installs
  (collectors, fleetd, bro) - the cluster produces Linux
  artifacts only. Warmth is the discriminator, not
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
