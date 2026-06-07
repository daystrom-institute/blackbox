---
title: "Fleet TUI — /closeout command (phased closeout driver + escalation + hooks + target param)"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - fleet-tui
  - surfaces
brief: "A /closeout <disposition> cockpit command for folding a worktree back into a target branch. Grounded review (brodex/gpt-5.5) established that the existing exit_worktree tool (crates/bro-tools/src/fleet_worktree.rs:323) is MONOLITHIC (publish/merge push and remove in one call) and PRIVATE, and that bro-cli does not depend on bro-tools — so /closeout cannot externally wrap the tool. The real work is: decompose the closeout sequence into a phased, pub driver in bro-tools that runs lifecycle hooks at internal phase boundaries and returns a structured per-phase result; expose it via a daemon /control/closeout endpoint (the daemon links bro-tools); make the cockpit /closeout a thin client over /control/*; and handle worktree-local rebase conflicts by steering the worktree's own agent. Plus: target/branch parameterization (exit_worktree is hardwired to main across ~7 surfaces, not 3) and project-scoped shell hooks in fleet.json (strict-loaded, name-disambiguated from the harness nudge-hook subsystem)."
---

# Fleet TUI — `/closeout` command

> **Scope, corrected after review.** This is **not** a thin command wrapping a
> ready-made tool. An adversarial code-grounded review (brodex/gpt-5.5, recorded
> below) established two structural blockers:
>
> 1. **`exit_worktree` is monolithic** — `publish`/`merge` *push* (`fleet_worktree.rs:408`)
>    and *remove the worktree* (`:410`) inside a single tool call. Hooks or
>    escalation cannot be interleaved by an external wrapper: there is no
>    pre-push or pre-remove seam, so `on_fail=block` and `pre_remove` are
>    impossible from outside.
> 2. **No invocation path** — `bro-cli` does **not** depend on `bro-tools`
>    (verified: `crates/bro-cli/Cargo.toml` has no `bro-tools` entry), and
>    `exit_worktree` is a **private** fn (`:323`) behind an agent tool call.
>    Cockpit slash-commands only mutate TUI state or steer/resume an agent
>    (`run_local_slash` `fleet_tui.rs:1976`); none can call the tool directly.
>
> The actual work, therefore: **(1)** decompose the closeout sequence into a
> phased, `pub` driver in `bro-tools` that runs lifecycle hooks at internal
> phase boundaries and returns a structured per-phase result; **(2)** expose it
> via a daemon **`/control/closeout`** endpoint — the daemon root crate *does*
> link `bro-tools` (`Cargo.toml:82`), and the cockpit already speaks the
> `/control/*` plane; **(3)** make the cockpit `/closeout` a thin client; **(4)**
> handle worktree-local rebase conflicts by steering the worktree's own agent.
> `exit_worktree`'s existing dispositions stay as a back-compat surface; the
> phased driver factors out their shared git steps.

## 1. Motivation

The worktree closeout dance is mechanizable and we keep hand-running it. The
existing `exit_worktree` `publish`/`merge` dispositions already automate the
happy path. Three things keep it from being a single, robust command:

1. **The git dance fails closed at points an agent or operator must resolve** —
   a rebase conflict, a push rejected because the remote moved, a base branch
   that has diverged. `exit_worktree` correctly `bail!`s rather than forcing, but
   a one-shot tool call then dead-ends with no recovery path.
2. **There is no lifecycle hook for project hygiene** — most concretely the
   build-cache reclaim (`cargo sweep` / `cargo clean`) that bounds the never-GC'd
   shared `target/` (see the `Share CARGO_TARGET_DIR` convention). Closeout is
   the natural reclaim cadence, but nothing runs there — and per blocker 1, it
   *can't* without a phased driver.
3. **`exit_worktree` is hardwired to `main`** and to `bro-fleet/*` branches, so
   it cannot close out against `beta/blackbox-v2` (now primary focus) or any
   non-fleet branch.

Design philosophy, unchanged: *the daemon owns the state machine; the LLM is a
turn.* The phased driver is the deterministic state machine; an agent turn is
escalated **only** for worktree-local reconciliation.

## 2. What already exists

### 2.1 The disposition logic — `exit_worktree` (`crates/bro-tools/src/fleet_worktree.rs`)

| disposition | behavior | code |
|---|---|---|
| `keep` | report status only | `:339-345` |
| `preflight` | **non-mutating** readiness report | `:346` → `publish_preflight` `:471-552` |
| `discard` | remove clean/confirmed worktree, `branch -D` | `:347-357` |
| `publish` | commit selected paths → fetch/ff `origin/main` → rebase → ff-merge → push → **remove** | `:358-419` |
| `merge`/`adopt` | fold already-committed clean branch → push → **remove** | `:420-422` → `merge_committed_worktree` `:431-468` |

Reused safety rails (the driver factors these out, not reinvents): `confirm=true`
gate (`:348`,`:359`,`:438`); managed-worktree guard (`ensure_managed_worktree`
`:577-594`); unsafe-pathspec refusal (`:391-395`); post-commit dirty-tree check
(`:400-405`); the rich `preflight` report — `publish_ready`, `merge_ready`,
`base_ready`, `changed_paths`, `unsafe_paths`, `branch_commits_ahead_main`,
`main_vs_origin`, plus explicit `publish_plan`/`merge_plan` (`:509-551`).

### 2.2 The procedure doc

`prompts/CLOSEOUT.md` — operator-pointed 5-step procedure. `/closeout`
mechanizes it.

### 2.3 Transcript/state plumbing (reused for the cockpit client only)

- `user_event_has_successful_tool_result(events, _, "exit_worktree")`
  (`crates/bro-fleet-client/src/fleet.rs:758-785`, used at `:726`) derives the
  `worktree_finished` snapshot flag. **It is a success-only predicate** — it
  returns `false` for no-result, wrong-tool, parse failure, non-`ok` content, and
  genuine tool error alike. Its inverse is therefore **not** a failure detector
  (see §4.3).
- `resume_agent` (`fleet_tui.rs:2501-2548`) / `ResumeSpec`
  (`crates/bro-protocol/src/dispatch.rs:52`) is the resume **mechanism** used for
  agent-steered reconciliation. (`install_ctrl` `fleet_tui.rs:2655` is cited for
  resume mechanics **only**; it handles `/control/steer|interrupt` transport
  errors, not transcript tool failures, so it is not a failure-detection
  precedent.)
- MiniMax/GLM/DeepSeek/Brodex/VibeBh are bidi-steerable
  (`provider_supports_bidi` `fleet.rs:1396-1401`) — so the agent-reconcile leg is
  available. (Code includes MiniMax; the fn's doc-comment list omits it — fix in
  passing.)

## 3. The three gaps

### Gap A — Escalation, split by *which repo* fails

The closeout sequence can fail in two different working trees, which require
different recovery — the original draft conflated them:

| step | code | repo | recovery |
|---|---|---|---|
| base on target + clean | `ensure_base_ready_for_publish` `:596-606` | **base/target checkout** | operator-surfaced or a base-reconcile step; **not** the worktree agent |
| `merge --ff-only origin/<target>` | `:385` (`:450`) | **base/target checkout** | refetch/reclassify; operator if still blocked |
| `rebase <target>` | `:406` (`:451`) | **managed worktree** | 🤖 resume the **worktree's own agent** to reconcile conflicts |
| `merge --ff-only <branch>` | `:407` (`:456`) | base/target | recompute after rebase |
| `push origin <target>` | `:408` (`:457`) | base/target | dedicated push-reject recovery (§4.2) |

Only **worktree-local rebase conflicts** route to the worktree's agent (it holds
the edit context; `bro_resume`-to-author convention). Base/target-repo failures
occur in a checkout the agent does not own, so they are surfaced to the operator
or handled by a base-reconcile step — the resume prompt, when used, must name the
exact repo cwd and operation.

### Gap B — Project-scoped shell hooks, executed *inside* the phased driver

**Name collision, resolved:** `crates/bro-harness/src/hooks.rs` (`Hook` trait
`:143`, `HookEngine` `:156`, `Delivery` enum `:46`) is an **agent-loop nudge**
subsystem — matchers that propose rider/system-tail nudges to the model. It is
**not** a shell-command lifecycle mechanism. Closeout hooks are new and distinct;
name them **`closeout_hooks`**, never bare `hooks`.

Because the spine is monolithic today (blocker 1), hooks **cannot** run as an
external post-step. They run at **phase boundaries inside the phased driver**:

| event | fires | `on_fail=block` meaningful? |
|---|---|---|
| `pre_push` | after local ff-merge, before `push` | **yes** — abort before publishing |
| `pre_remove` | after push, before `worktree remove` | **yes** — abort before teardown |
| `post_success` | after remove, work landed | no — advisory only (e.g. `cargo sweep`) |
| `on_discard` | discard path, before removal | yes |

This is where the disk problem closes: `pre_push`/`post_success → cargo sweep`,
`on_discard → cargo clean`, bounded by closeout cadence with incremental left on.

`FleetConfig::load()` (`crates/bro-fleet-client/src/fleet.rs:227`) is **best-effort
and silently returns default config on malformed `fleet.json`** — which would
silently drop a configured `target`/`closeout_hooks` and fall back to `main`.
`/closeout` therefore loads closeout config through a **strict** path that
surfaces parse errors as a blocking command error rather than defaulting.

Config shape (additive — `FleetConfig` has no per-project block today; `projects`
`:53` is a flat alias map):

```rust
// crates/bro-fleet-client/src/fleet.rs — new, additive, keyed by canonical repo path
pub project_closeout: BTreeMap<String, ProjectCloseout>,

pub struct ProjectCloseout {
    pub target: Option<String>,                       // default "main"
    pub allow_branch_prefixes: Option<Vec<String>>,   // default ["bro-fleet/"]
    pub closeout_hooks: BTreeMap<CloseoutEvent, Vec<String>>, // pre_push|pre_remove|post_success|on_discard
    pub hook_policy: HookPolicy,                       // cwd, on_fail (warn|block), timeout_secs (def 600)
}
```

**Config vs wire boundary (crate-checked).** `ProjectCloseout` is the on-disk
config shape and stays in `bro-fleet-client` (the cockpit owns `fleet.json`
loading). But the daemon root crate does **not** depend on `bro-fleet-client`,
and `bro-tools` does **not** depend on `bro-protocol` (verified). So config can't
simply be a `bro-fleet-client` type the daemon/driver read. Instead:

1. The cockpit **strict-loads** `ProjectCloseout` and **resolves** `target` +
   `closeout_hooks`.
2. It sends a fully-resolved **`CloseoutRequest`** DTO — defined in
   **`bro-protocol`** alongside `DispatchSpec`/`ResumeSpec`
   (`crates/bro-protocol/src/dispatch.rs`), which the cockpit links transitively
   and the daemon links directly (`Cargo.toml:76`) — over `/control/closeout`.
3. The daemon deserializes it and calls the `bro-tools` driver with **primitive
   resolved params** (`target: &str`, a `bro-tools`-local `CloseoutHooks`,
   `confirm`, `disposition`). `bro-tools` defines its own `PhaseResult` /
   `CloseoutHooks` and stays free of `bro-protocol`.

This mirrors how the daemon already translates `DispatchSpec` into harness calls
— config lives where it's read, the wire DTO lives in the shared contract crate,
and the driver takes resolved primitives.

### Gap C — Target/branch parameterization (complete surface audit)

`exit_worktree` is `main`-only across **~7 surfaces**, not three (the original
draft itself listed four bullets under "three places"). A `target` param must
cover **all** of them, and the JSON output keys need an explicit
compat-vs-rename decision:

| surface | code | treatment |
|---|---|---|
| `ensure_base_ready_for_publish` requires base on `main` | `:597-599` | accept `target` |
| fetch/merge `origin/main` (publish + merge) | `:384-385`, `:449-450` | `origin/<target>` |
| `rebase main`, `merge --ff-only <branch>` | `:406-407`, `:451-456` | `rebase <target>` |
| `push origin main` | `:408`, `:457` | `push origin <target>` |
| `branch_ahead_count` computes `main..branch` | `:618-624` | `<target>..branch` |
| `publish_preflight` `base_branch == "main"` gate | `:493` | compare to `<target>` |
| preflight JSON keys: `branch_commits_ahead_main`, `main_head`, `origin_main_head`, `main_vs_origin`; `publish_plan`/`merge_plan` strings | `:520-551` | **decide**: keep as compat aliases or rename target-neutral (`branch_commits_ahead_target`, …) |
| tool description text ("origin/main", "pushes main") | `:192-194` | update |
| publish/adopt **bail & error text** ("ahead of main", "merge into main") | `:367-373`, `:454` | update target-neutral |
| branch guard `bro-fleet/*` only | `:333-335` | independent `allow_branch_prefixes`; **detached HEAD stays fail-closed** |
| existing tests asserting `main` semantics | `:863-1060` | update |

`target` defaults to `"main"`, preserving exact current behavior; branch
eligibility is a **separate** axis from target selection (relaxing the
`bro-fleet/*` guard is a real safety change — detached HEAD and
branch-checked-out-elsewhere must remain refusals).

## 4. The `/closeout` design

### 4.1 Architecture (resolves blocker 2)

```
cockpit /closeout ──/control/closeout──► daemon endpoint ──► bro-tools phased driver
   (bro-cli, thin)      (existing plane)     (daemon links      (pub, where exit_worktree lives)
                                              bro-tools)
```

- The **phased driver** lives in `bro-tools` (made `pub`), factoring out
  `exit_worktree`'s git steps into discrete phases with hook seams and a
  structured return.
- The **daemon** hosts `/control/closeout` (it links `bro-tools` —
  `Cargo.toml:82`), alongside `/control/exec`·`/control/resume` the cockpit
  already uses (`fleet.rs:1316`,`:1335`).
- The **cockpit** `/closeout` is a thin client: register in `zone_slash_commands`
  (`fleet_tui.rs:844`), parse in `run_local_slash` (`:1976`) and route to a
  driver call rather than returning handled/steer.
- **Worktree-local reconciliation** is the one place an agent turn is used: the
  cockpit steers/resumes the worktree's agent (`resume_agent` `:2501`).

> Note: the daemon already has an `exit_worktree`-success hook that restores the
> base cwd after worktree removal, but it depends on `base_repo` in the success
> payload — which `publish`/`merge` do **not** currently emit. The phased driver's
> structured result must include `base_repo`/target cwd so this (and the cockpit)
> work correctly.

### 4.2 The state machine

```
/closeout <disp> [--dry-run]
  → driver.preflight(target)               # never mutates; structured readiness
  → render plan; if --dry-run: STOP
  → driver.run(<disp>, target, confirm, closeout_hooks)
        returns PhaseResult { phase, repo_cwd, ok, error_class, content }
        ┌── ok ───────────────────────────────────────────────► hooks already ran in-driver; done
        ├── fail @ rebase (repo = worktree) ──► resume worktree agent (conflict ctx)
        │        → on signal: driver.preflight(target) again
        │        → if worktree now CLEAN & branch ahead: driver.run(ADOPT) ── NOT re-run publish
        ├── fail @ push (repo = base/target) ──► push-reject recovery (NEVER force-push):
        │        precondition: base was clean (ensure_base_ready); only local delta is our ff-merge
        │        fetch origin/<target>; then:
        │          local target ff-ahead of origin, no remote move → retry push
        │          origin/<target> moved → reset --hard local <target> to origin/<target>
        │             (safe: discards only our just-made ff-merge), re-rebase branch onto
        │             new tip, redo local ff-merge, retry push
        │               re-rebase conflict → worktree agent reconcile leg
        │               still rejected after retry → operator
        └── fail @ base-ready / ff origin (repo = base/target) ──► operator-surfaced
```

Two corrections from review are baked in:

- **No naive disposition retry.** After a *post-commit* failure (publish already
  committed, then rebase/push failed), re-running `publish` hits the
  "no uncommitted changes but branch ahead → use adopt" bail (`:368-375`). The
  driver re-runs `preflight` and transitions to **`adopt`/`merge`** when the
  worktree is clean and the branch is ahead.
- **Push-reject is its own recovery state.** A failed `push` happens *after* the
  local target was already ff-merged; `preflight`'s booleans don't model
  "local target ahead + remote moved." The recovery state fetches, reclassifies
  against that condition, and does not assume `branch_commits_ahead > 0`.

### 4.3 Failure detection (structured, not inverse-of-success)

Both paths key on the **structured driver result** — never on the inverse of a
success-only predicate:

- **Direct path (`/control/closeout`, the normal case).** The endpoint returns
  the driver's structured `PhaseResult { phase, repo_cwd, ok, error_class,
  content }` synchronously. **No transcript parsing** — the cockpit reads the HTTP
  response. A daemon endpoint does not produce transcript tool results, so the
  `user_event_*` predicates are irrelevant here.
- **Post-reconcile retry (the driver still owns closeout).** When a
  worktree-local rebase conflict is handed to the worktree's own agent, the
  agent's *only* job is to resolve and commit the conflict in the worktree, then
  signal done (turn completion). The **driver owns every closeout git/tool call**
  — the agent does not drive closeout itself. On the done-signal the cockpit
  re-invokes `/control/closeout`, which re-runs `preflight` → `adopt`/`merge`
  (§4.2) and returns a fresh `PhaseResult`. So this path's detection is *also* the
  structured driver result — there is no `tool_use_id`-extractor hack, and the
  success-only `user_event_*` predicate (§2.3) is never inverted. (`resume_agent`
  is used purely as the reconcile-turn mechanism, not for detection.)

### 4.4 Hook execution

Hooks run **inside the driver** at the phase boundaries in §3 Gap B, in
`HookPolicy.cwd` (default target checkout), honoring `on_fail` (`block` only
meaningful at `pre_push`/`pre_remove`/`on_discard`) and `timeout_secs`. Output
surfaces as a cockpit status flash via the structured result.

## 5. Invariants & boundaries

- **Harness/daemon boundary preserved.** The driver lives in `bro-tools` (linked
  by the daemon root crate, `Cargo.toml:82`) — *not* linked directly into
  `bro-cli`. The cockpit invokes it through the daemon's `/control/closeout` HTTP
  endpoint, exactly as `dispatch`/`resume` ride `/control/exec`·`/control/resume`.
  No harness→daemon backchannel.
- **Fail-closed rails reused, never weakened.** `confirm=true`, clean-only
  removal, managed-worktree + pathspec guards, detached-HEAD refusal all retained.
  Guard refusals escalate (operator or agent), never silent `-D`/`--force`.
- **Multi-tenant worktree safety.** `paths` staging stays scoped to the managed
  worktree (already enforced `:391-395`); no `git add -A`.
- **Default-preserving.** Omitting `target`/`closeout_hooks` reproduces today's
  exact behavior; opt-in per project; closeout config strict-loaded so a typo
  fails loudly instead of silently reverting to `main`.

## 6. Phasing

1. **Decompose `exit_worktree` into the phased `pub` driver** + structured
   `PhaseResult` (incl. `base_repo`/target cwd); keep dispositions behaviorally
   identical (`target` defaults to `main`). Tests updated.
2. **Target/branch parameterization** (Gap C, full surface audit) — unblocks
   closeout against `beta/blackbox-v2`.
3. **`/control/closeout` endpoint + cockpit `/closeout` thin client + `--dry-run`**
   (happy path; failures surface structured but un-escalated).
4. **Escalation** (Gap A) — repo-classified recovery: agent-steered rebase
   reconcile + push-reject recovery + preflight→adopt transition.
5. **`closeout_hooks`** (Gap B) — config struct (strict-loaded) + in-driver
   phase execution + policy.

Phases 1–3 are independently shippable.

## 7. Open questions

- Should `closeout_hooks` inherit the dispatch env (`resolve_provider_env`
  `src/orchestration/brofile.rs:543`; `prepare_dispatch_worktree` `dispatch.rs:319`)
  so the sweep hook targets the right `CARGO_TARGET_DIR`? Likely yes.
- Escalation turn budget: cap reconcile attempts before handing to the operator.
- Concurrent closeouts into the same target (push races) — rely on the
  push-reject recovery state, or serialize per-target in the endpoint?
- Preflight JSON keys (Gap C): break compat with target-neutral names now, or
  keep `*_main` aliases for one release?

## 8. Code anchors (provenance)

| concept | file:line |
|---|---|
| `exit_worktree` (private) / tool / input | `crates/bro-tools/src/fleet_worktree.rs:323` / `:184` / `:164` |
| monolithic push+remove | `:408` (push) `:410` (remove) |
| dispositions | `:339` `:346` `:347` `:358` `:420` |
| post-commit "use adopt" bail | `:368-375` |
| bail points by repo (Gap A) | base `:596` `:385`; worktree `:406`; `:407` `:408` |
| target surfaces (Gap C) | `:333` `:384-385` `:406-408` `:449-457` `:493` `:520-551` `:597-599` `:618-624` `:192-194` `:863-1060` |
| `publish_preflight` | `:471-552` |
| managed-worktree / pathspec guards | `:577-594` / `:391-395` |
| `bro-cli` has NO `bro-tools` dep (blocker 2) | `crates/bro-cli/Cargo.toml` (verified absent) |
| daemon root links `bro-tools` | `Cargo.toml:82` |
| `FleetConfig` / `projects` / best-effort `load` | `crates/bro-fleet-client/src/fleet.rs:43` / `:53` / `:227` |
| harness nudge-hooks (do NOT conflate) | `crates/bro-harness/src/hooks.rs:143` `:156` `:46` |
| success-only predicate (not a failure detector) | `fleet.rs:758-785` (used `:726`) |
| `provider_supports_bidi` (incl MiniMax; comment omits it) | `fleet.rs:1396-1401` |
| slash surface / `run_local_slash` | `fleet_tui.rs:836` `:844` `:1930` `:1976` |
| resume mechanics (NOT failure detection) | `resume_agent fleet_tui.rs:2501`; `install_ctrl :2655` |
| `DispatchSpec` / `ResumeSpec` | `crates/bro-protocol/src/dispatch.rs:17` / `:52` |
| existing `/control/*` plane | `fleet.rs:1316` (exec) `:1335` (resume) |
| procedure doc | `prompts/CLOSEOUT.md` |

## 9. Review provenance

Reviewed by brodex/gpt-5.5 (high), code-grounded, session
`dabad663-8e8f-4fd9-890e-deb4d02d62a8`.

- **Round 1** — 2 blockers (monolithic tool; no invocation path), 6 majors
  (inverse-detector invalid; naive retry breaks post-commit; push-reject
  unmodeled; target audit incomplete; silent config load; base-vs-worktree
  failure conflation), 2 minors, 1 nit. → driver/endpoint rearchitecture.
- **Round 2** — both blockers + 6 of 8 prior findings RESOLVED; 3 PARTIAL +
  2 NEW addressed here: PhaseResult-vs-transcript path disambiguation (§4.3);
  push-reject no-force reset detail (§4.2); target audit bail-text surfaces
  `:367-373`/`:454` (§3 Gap C); config/wire boundary via a `bro-protocol`
  `CloseoutRequest` DTO (§3 Gap B); §5 RPC-wording fix.
