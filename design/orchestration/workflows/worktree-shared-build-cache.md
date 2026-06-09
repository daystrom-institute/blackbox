---
title: "Shared build cache for workflow-created worktrees"
kind: design
lifecycle: superseded
corpus: blackbox-design
topic:
  - orchestration
  - workflows
date: 2026-05-31
status: "superseded 2026-06-08 by per-worktree isolation + project_dispatch (closeout Phase 5)"
superseded_by: design/fleet-tui/closeout-command.md
brief: "Give WorktreeCreate an optional shared-build-cache mode so sibling Cargo-workspace worktrees stop cold-compiling the full dependency tree independently — threaded via a new arc→dispatch env seam, gated on a benchmark."
---

> **SUPERSEDED (2026-06-08).** This doc's Option A/B/C framing (shared
> `CARGO_TARGET_DIR` vs sccache vs per-worktree, gated on a benchmark) was
> resolved by a different cut: the fleet dispatch path was **de-hardcoded to
> per-worktree target isolation** (no shared `CARGO_TARGET_DIR`), which removes
> the build-lock contention this doc set out to fix, and a project-agnostic
> **`project_dispatch`** env surface in `fleet.json` now carries optional
> per-project build env (e.g. `RUSTC_WRAPPER=sccache`) instead of the engine
> guessing a mode. The shared-target disk-bounding motivation evaporates because
> worktree removal auto-reclaims a per-worktree target dir. See
> `design/fleet-tui/closeout-command.md` (Phase 5, leading edge) and
> `docs/developing-blackbox.md` (sccache setup). The original gap note
> `note-555bf465` is resolved by that work.

# Shared build cache for workflow-created worktrees

Related: [Workflow Engine](../../../docs/workflows.md), [Workflow Orchestration](workflow-orchestration.md), [Supervision](../supervision/supervision.md)

Origin: gap note `note-555bf465` (`blackbox.gap_note.v1`, domain `workflow-orchestration/worktree`, 2026-05-30). Captures the cost observed during a window-0 wave-2 build: two parallel drones in sibling worktrees, each with a cold per-worktree `target/`, spent almost all of a 15–20+ min single-file task in compile-wait. Several supervision "stalls" were really cargo cold-compiles.

## 1. Problem (grounded in code)

When a workflow arc creates a worktree, `exec_worktree_create`
(`src/workflow/ops/worktree.rs:7`) runs a bare `git worktree add` and sets up
**no build-cache environment**:

```rust
cmd.arg("-C").arg(&repo_root).arg("worktree").arg("add");
// ... -b / branch selection ...
Ok(OpEffect::SetWorktree(Some(path)))
```

`grep CARGO_TARGET_DIR` returns **zero hits** anywhere in `src/` or `crates/`.
Git worktrees do **not** share build state: each sibling worktree has its own
`target/`, so a freshly created worktree cold-compiles the entire dependency
tree before the first useful `cargo check`. For this large workspace that is
both a large wall-clock tax and tens of GB of disk per worktree (43 GB + 16 GB
observed in the field).

This is a per-arc penalty: any multi-worktree workflow (fan-out drones,
`WorktreeCreate` in a phase-decompose arc, ensemble implementers each on a
branch) pays it once per worktree, and pays it *concurrently* — which is where
the supervision-stall symptom comes from.

### Why a convention alone is insufficient

Commit `2595a69` documented a project **convention** ("share `CARGO_TARGET_DIR`
across Cargo-workspace git worktrees") in `CLAUDE.md`. That convention binds a
human or agent who is hand-rolling `git worktree add` — it does **not** change
`exec_worktree_create`, which is the path the workflow *engine* takes
automatically. A workflow drone dispatched into an engine-created worktree never
sees that instruction as code; it inherits whatever env the dispatch seam gives
it. So the convention covers the ad-hoc case and leaves the orchestrated case
(the one that motivated the gap note) unaddressed. This proposal closes the
orchestrated case in code.

## 2. The missing seam

Today the arc threads exactly three things from a hook op into a dispatched
node:

- `OpEffect::SetWorktree(Option<String>)` → `ctx.meta.worktree`
  (`src/workflow/ops.rs:236-237`, `src/workflow/context.rs:60-62`).
- `OpEffect::SetProjectDir(Option<String>)` → runner/`meta.project_dir`.
- `OpEffect::SetVar { key, value }` → `ctx.vars`.

A dispatched actor's working directory is then derived as
`effective_project_dir() = meta.worktree.or(project_dir)`
(`src/workflow/engine.rs:711-717`), passed as the actor's `project_dir`
(`src/workflow/engine/actor_nodes.rs:222,242`). So **cwd** flows from the
worktree op to the drone automatically — but **environment** does not.

There *is* an env-injection point further down: a brofile carries
`env: Option<HashMap<String, String>>` (`src/orchestration/brofile.rs:144`) and
`resolve_provider_env` (`brofile.rs:557`) builds the provider env at dispatch
time. But nothing lets a value **computed at `WorktreeCreate` time** (the shared
target path, which depends on the chosen worktree) reach that env map. That is
the gap this design fills: an **arc → dispatch environment seam**, parallel to
the existing worktree/cwd seam.

## 3. Options (the gap note's verify caveat is load-bearing)

The gap note explicitly warns: *"a single shared `CARGO_TARGET_DIR` +
concurrent cargo invocations contend on cargo's build lock — benchmark
shared-dir-with-lock vs per-worktree-cold to confirm a net win; sccache, or
per-worktree target dirs with a shared registry/git cache, may beat a single
shared dir. Measure, do not assume."* This design therefore presents options and
**gates the choice on a benchmark**, rather than hard-coding one.

### Option A — single shared `CARGO_TARGET_DIR`

Point every sibling worktree at one `CARGO_TARGET_DIR` (e.g.
`<repo>/.bbox/local/worktree-target` — under `.bbox/local/` so it is gitignored
host-local cache, consistent with the index/embeddings-are-derived rule).

- **Pro:** maximal artifact reuse; the second worktree's `cargo check` is warm.
- **Con:** cargo takes an **exclusive build lock** on the target dir, so two
  worktrees building at once **serialize**. For a fan-out where drones build
  simultaneously, this can erase the saving (or invert it: serialized warm
  builds vs parallel cold builds). The CLAUDE.md convention already flags this
  and says "if you genuinely need parallel builds across worktrees, accept the
  duplication instead."
- **Best when:** worktrees build at *different* times (sequential phases), or
  the dependency tree dominates and per-crate rebuilds are small.

### Option B — `sccache` (shared compilation cache, no single build lock)

Set `RUSTC_WRAPPER=sccache` with a shared `SCCACHE_DIR`, each worktree keeping
its own `target/`.

- **Pro:** caches *compilation outputs* at the rustc level without a single
  build-dir lock, so concurrent worktrees still compile in parallel and share
  cached objects. Better fit for the fan-out case that motivated the gap note.
- **Con:** requires `sccache` on PATH (a new external dependency for the
  orchestrated path); cache hit rate depends on identical rustc flags; not all
  crates cache well (build scripts, proc-macros).
- **Best when:** drones build concurrently (the observed wave-2 scenario).

### Option C — per-worktree target + shared registry/git cache only

Keep per-worktree `target/` but share `CARGO_HOME`/registry/git caches (these
are already shared via the user's default `CARGO_HOME`).

- **Pro:** no build-lock contention, no new dependency.
- **Con:** does **not** address the dominant cost — the cold *compile* of the
  dependency tree, not the *download*. Likely the smallest win; included for
  completeness.

## 4. Recommended shape

1. **Build the seam first (option-independent).** Add an arc-level env channel
   so `WorktreeCreate` can hand the chosen cache env down to every actor
   dispatched into that worktree. Two viable forms:
   - a new `OpEffect::SetEnv { key, value }` applied into a `meta.env:
     BTreeMap<String,String>` (mirrors `SetWorktree`), or
   - `exec_worktree_create` returns the env alongside the path and the runner
     stores it on `meta`.
   The dispatch path then merges `meta.env` into the resolved provider env in
   `actor_nodes.rs` (precedence: explicit brofile `env` wins over arc-injected
   env, so a brofile can still override). This seam is reusable beyond build
   caches (e.g. per-arc `RUST_LOG`, feature-flag env).

2. **Make the cache mode opt-in and declared on the op**, not a silent default.
   Add `WorktreeCreate` args, e.g.
   `build_cache: "none" | "shared_target" | "sccache"` (default `"none"` to
   preserve current behavior). `exec_worktree_create` detects a Cargo workspace
   (a `Cargo.toml` with `[workspace]` at `repo_root`) and only then populates
   the env; on a non-Cargo repo the flag is a no-op.

3. **Default path under `.bbox/local/`** so the shared target/cache is treated
   as derived host-local state (never committed), consistent with the
   repo-owned-vs-host-local split.

4. **Gate the *default* choice on a benchmark** (see §5). Ship the seam +
   `"none"` default + explicit opt-in modes regardless; only flip a non-`none`
   default once the measurement says which mode wins for the common fan-out.

## 5. Benchmark gate (required before changing the default)

Measure, on this workspace, for the realistic fan-out (N=2 and N=4 concurrent
worktree builds, cold start):

- **Baseline:** per-worktree cold `target/` (today).
- **A:** single shared `CARGO_TARGET_DIR` (expect serialization under
  concurrency).
- **B:** `sccache` with shared `SCCACHE_DIR`, per-worktree `target/`.

Metrics: wall-clock to first green `cargo check` per worktree, total wall-clock
to all-green, peak disk. The hypothesis from the field report is that **B wins
under concurrency and A wins under sequential phases**; if so, the op flag lets
the arc author pick per workflow shape rather than the engine guessing.

## 6. Non-goals

- **Not committing any build cache.** Target dirs, sccache dirs, and registries
  stay host-local derived cache (the index/embeddings/edge-sidecar rule).
- **Not changing the ad-hoc convention.** The `CLAUDE.md` convention for
  hand-rolled `git worktree add` stays; this adds the orchestrated-path code
  the convention cannot reach.
- **Not a global RUSTC_WRAPPER mandate.** sccache, if adopted, is scoped to
  worktrees created with `build_cache: "sccache"`, not forced on every dispatch.

## 7. Open questions

- **Cleanup ownership.** A shared target dir under `.bbox/local/` grows
  unbounded; does `WorktreeRemove` (or a janitor) prune it, and on what policy?
  A shared dir cannot be removed with any single worktree.
- **Lock-wait visibility.** Under option A, a drone "stalled" on cargo's build
  lock looks identical to a hung drone to the supervisor. Should the build-lock
  wait be surfaced (e.g. distinct from a compile or a true stall) so supervision
  does not misattribute it? This is the inverse of the symptom that produced the
  gap note.
- **sccache availability.** Treat missing `sccache` as a hard error on
  `build_cache: "sccache"` (fail closed, like the RA-backed refactor kinds), or
  silently fall back to per-worktree cold (silent downgrade — disfavored by the
  repo's fail-closed posture)?
