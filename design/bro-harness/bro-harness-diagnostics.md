---
title: "bro-harness diagnostics (window-0 analyzer feedback)"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - bro-harness
  - surfaces
brief: "Synchronous, per-mutation analyzer feedback for the bro-harness edit loop: a diagnostic for the state an edit produced rides that edit's tool result before the agent can act again (window=0). The organizing constraint is the agent's TRUST in the channel, not latency — stale or unconfirmable diagnostics bankrupt the channel (chase + alarm-fatigue), so the design trades latency and fussiness for precision. Affordable because the diagnostics that compound catastrophically (errors/types/borrow) are language-server-instant while the slow ones (lints) don't compound. Transport is LSP, consumed in-process: bro-harness owns its own warm session via shared-crate code and stays fully functional with the daemon down — never a runtime backchannel to blackbox. Per-language semantics live in a thin classification adapter whose size tracks the gap to a unified Roslyn-like toolchain. The truth tier is not a harness concern at all: it is owned by whoever performs the ownership transfer (the orchestrator at collection, or an explicit solo act), because the harness — as the payload being transferred — cannot observe the boundary."
---

# bro-harness diagnostics (window-0 analyzer feedback)

> **Status.** Proposed. Grounded against code 2026-05-30:
> `crates/bro-harness/src/{agent_loop.rs,bound.rs,session.rs}` (edit loop, the
> tool-result rider mechanism, the `side` persistence spine),
> `src/lsp/session_manager.rs` (`LspSessionManager`, `with_session`,
> `wait_for_diagnostics`, `(project_root, Language)` keying, `launch_argv`),
> `src/projects.rs` (`Language` enum — `Csharp` already scaffolded),
> `src/config.rs` (`LspConfig`, `roslyn_lsp_bin`), `src/code_nav/semantic.rs`
> and `src/refactor/{rust.rs,java/leaf_plans.rs}` (existing LSP consumers).
> Note: `LspSessionManager` lives in the `blackbox` lib (daemon-local) today; the
> workspace contract (root `Cargo.toml`) forbids the Anthropic-harness crates from
> depending on the `blackbox` lib, so this design extracts a minimal session core
> into a shared crate (see "The dependency constraint" below). Latency figures
> below were measured this session on this workspace.

> **Implementation status (shipped 2026-05-30).** The **instant/error tier MVP is
> built and on `main`** (window-0 waves 1–2):
> - `crates/bro-lsp` — in-process LSP client: session pool, persistent `didChange`,
>   pull diagnostics + `publishDiagnostics` fallback, version-correlated drop-stale,
>   fail-closed. No dependency on the `blackbox` lib (DX-9; the session core was
>   built fresh in the shared crate, the daemon's `src/lsp/` left untouched — the
>   migrate-vs-copy fork is still open).
> - harness substrate — `EditSink` pre-image capture (model-facing result
>   untouched) + the `side`-persisted `LspBaselines` cell.
> - the integration — `diagnostics::{engine,render}` + the `agent_loop`
>   `append_window0_diagnostics` seam: drain edits → run `bro-lsp` → stable-key
>   (span-text, not line) diff vs baseline → classify (error tier) → rider on the
>   tool result, with an end-to-end test gated on rust-analyzer.
>
> **The check and truth tiers are deferred — sound design, low value now.** The
> instant tier already captures the catastrophically-compounding diagnostics
> (types, borrows, unresolved names). The check tier (flycheck lints —
> `unused_imports`, `dead_code`, clippy) targets diagnostics this very doc argues
> *do not* compound, largely duplicates the `cargo check`/`clippy` validation an
> agent already runs before committing, performs worst on the hub crate where it
> would help most (~8.8–16s vs ~380ms on a leaf crate), and pulls in
> disproportionate machinery (≥2-check persistence gating, the `Scope`/`candidate`
> config-fragility model, the no-fix-from-fragile re-verification — the last
> existing only to handle a hazard the tier itself creates). The truth tier is
> orchestrator-owned (see "The truth tier" below) and further out still.
> **Revisit trigger:** config-fragile lints become a real pain point, or crates are
> split enough that check-tier latency is leaf-crate-fast. The
> `{class,tier,confidence,scope}` envelope vocabulary already lives in
> `diagnostics/mod.rs`, so the deferred tiers are additive; the `Scope`-on-
> `DiffResult` decision is recorded on that struct and in `note-b59cadc5`. One
> verification owed before that wave: confirm whether the MVP already surfaces RA
> flycheck warnings crudely through the `publishDiagnostics` drain — if so, the
> deferred work is gating + scope-honesty, not new surfacing.
>
> The actionable residual (check-tier gating, the orchestrator-owned truth tier,
> and the open migrate-vs-copy `bro-lsp`/`src/lsp` fork) is tracked as a pickup
> item in [`backlog-diagnostics-truth-tiers.md`](./backlog-diagnostics-truth-tiers.md).
> The full rationale for *why* those tiers are shaped this way stays in this
> as-built record; the body below is retained as the authority for the shipped
> instant tier and the deferred design.

This doc specifies how the harness should give an agent analyzer feedback on
its own edits. It is a sibling of [anthropic-harness](anthropic-harness.md) (the
loop), [bro-harness-hooks](bro-harness-hooks.md) (the volatile-injection seam
this rides), and [bro-harness-neuralyze](bro-harness-neuralyze.md) (the rewind
primitive a bad-foundation edit might trigger). See the cluster map in
[bro-harness](bro-harness.md).

## The thesis: the scarce resource is trust, not latency

A diagnostics channel an agent **stops trusting** is worse than no channel. Two
failure modes bankrupt it, and they are the design's real adversaries:

1. **Stale-chasing.** A diagnostic computed against bytes the file no longer has
   sends the agent fixing a phantom. Observed directly: see "the reference is
   async" below.
2. **Alarm-fatigue.** After enough false or transient signals, an agent learns
   the pattern "these are usually noise" and **ignores the channel — including
   the real diagnostics.** This is the more dangerous failure: a once-useful
   channel becomes a blind spot.

Both are *trust-bankruptcy* events, and both are **latency artifacts**: staleness
is what a delayed diagnostic becomes when the code moves underneath it. So the
design objective is not "fast diagnostics" — it is **never emit a diagnostic you
cannot stand behind**, and we spend latency and machinery to protect that. The
bet: synchronous, correct feedback pays for itself in *fewer goose chases* and a
channel the agent keeps trusting. Against turns that take minutes, a few hundred
ms of synchronous checking is lopsidedly worth it.

## Core principle: window = 0

**The diagnostic for the state an edit produced rides that edit's tool result,
synchronously, per mutation.** The agent cannot take its next action without
first seeing the consequence of its last one. No "next boundary," no "once it
compiles," no debounce, no deferral.

The cost of a late diagnostic is not constant — it **compounds**. Every agent
action taken between a flaw being introduced and the agent being told is another
layer of implementation built on a bad foundation, and unwinding N edits built
on a flaw is super-linear. So the only defensible target is **zero** intervening
actions.

This is also why **deferral is strictly worse than immediacy** — it causes *both*
failure modes at once: it opens the build-on-smell window *and* it guarantees the
eventual diagnostic is computed against older bytes (staleness). Any "wait until
X" mechanism re-introduces the exact disease it claims to cure.

### Why window=0 is affordable: the latency/compounding alignment

The latency curve and the compounding-cost curve are **aligned, not opposed**:

- Diagnostics that compound *catastrophically* — type errors, unresolved names,
  borrow errors — are exactly the ones a warm language-server index delivers
  **in <100ms** (rust-analyzer's salsa-memoized in-memory tier; the equivalent
  in tsserver/Roslyn/jdtls). A wrong foundation is the thing you must never build
  on, and you get window=0 on it essentially for free, every edit, every crate.
- Diagnostics that are *slow* (the check-time lint tier: unused import,
  dead_code, clippy) are exactly the ones that **don't compound** — an unused
  import you stack five edits on top of is still just an unused import; it does
  not infect what you built on it.

So immediacy is cheap precisely where it matters most, and expensive precisely
where the window is least harmful. The tension mostly dissolves.

**Corollary — crate granularity is a feedback-latency knob.** Per-edit latency
for the lint tier is just the touched crate's compile cost. Measured on this
workspace (rust-analyzer 1.95.0):

| Scope | Latency |
|---|---|
| RA in-memory (errors/types/resolution) | <100ms |
| Leaf crate (`bro-harness`), warm incremental `cargo check` | ~380–420ms |
| Main `blackbox` crate, one-module touch → recheck | ~8.8s |
| `blackbox` lib, full warm check | ~16.4s |

A monolithic crate is itself what makes the fast tier slow. Splitting it shrinks
the window=0 budget for every edit an agent makes in it — a measurable payoff
not usually attributed to crate-splitting.

## The dependency constraint: shared crate, never a runtime backchannel

bro-harness and the blackbox daemon are **complementary sibling projects with
orthogonal use cases**, not two halves of one runtime. Running blackbox with no
harness is valid; running bro-harness with no daemon is valid. The workspace makes
this a hard, written invariant (root `Cargo.toml`):

> The Anthropic-harness crates intentionally do NOT depend on the `blackbox`
> lib — the daemon↔harness contract is the Claude stream-json envelope on stdout,
> not shared types.

`bro-tools`'s own header confirms the direction of sharing: *"reusable by the
daemon or a future in-process executor"* — daemon→shared-crate, never
harness→daemon. The diagnostics design must respect this boundary:

- **No runtime backchannel.** The harness MUST NOT acquire diagnostics by calling
  a daemon MCP tool (`bbox_code_diagnostics` or otherwise) in its edit loop. That
  inverts the lifecycle: the daemon *spawns* the harness as a subprocess "exactly
  like `claude`," so a child that RPCs into its parent's MCP surface on every edit
  makes core harness function depend on daemon availability, reentrancy-safety, and
  dispatch backpressure. window=0 in the inner loop is the worst possible place to
  add a network round-trip and a second staleness vector (file-on-disk vs. what a
  remote session last `didChange`'d).
- **Sharing is by crate, in-process.** LSP session management is shared as
  **code**, not as a running service. The session core lives in a shared workspace
  crate. Today `LspSessionManager` lives in the `blackbox` lib (daemon-local), so
  this design requires extracting a minimal session core into `bro-tools` (or a new
  `bro-lsp`) that both the daemon and the harness link. Each process owns its own
  warm session against its own files. Shared code, separate runtimes.
- **"Duplicate language server" is a non-objection.** Two processes running
  rust-analyzer against *different* trees (the daemon's registered project root vs.
  a drone's isolated worktree) are not duplicating work — they index different
  files with real process isolation. The honest cost is one warm RA per harness
  process (not per edit), amortized over the whole dispatch and idle-evicted. That
  memory/startup cost is the price of the standalone invariant, and it is the right
  price.

The daemon may *also* expose a `bbox_code_diagnostics` MCP tool — but for **its
own** consumers. A `claude`-CLI-dispatched agent cannot host an in-process Rust LSP
session (it is the `claude` binary, not bro-harness), so the daemon serves it the
same envelope over MCP, reusing the same shared classification adapter. Same code,
two delivery surfaces: in-process rider for the harness; synchronous-unary MCP for
daemon-side non-harness agents. The two are orthogonal; neither sits on the other's
critical path.

## The reference is async — our sync model is *better than* it, not a copy

Empirically (observed this session in the Claude Code harness), the upstream
"diagnostics after edit" feature is **not** synchronous. Diagnostics arrive as a
separate injected reminder that lands on **whatever the next turn boundary is** —
observed riding a `Bash` result, a later `Edit` result, and a user interrupt —
debounced and decoupled from the causing edit, and demonstrably **stale** (it
reported warnings for lines that had already been reverted). So the upstream
model exhibits both failure modes by construction. The window=0 design here is a
deliberate correction, not an imitation.

## Why Rust forces tiers at all (and why C# is the exception)

Rust made the opposite architectural bet from .NET. There is no "rustlyn":

- **rustc** (truth, batch) and **rust-analyzer** (responsive, IDE) are *separate
  codebases* that approximate each other. RA gives an instant semantic model but
  **defers the canonical rustc lints** (`unused_imports`, `dead_code`) to
  flycheck = `cargo check`. So the lint tier is irreducibly check-time.
- **tree-sitter** (used in `code_nav`) is syntax-only: it can produce a *fast
  unused-import guess* by parsing `use` items and scanning identifiers, but that
  is **unsound** on macros, `cfg`-gating, glob imports, and shadowing — it
  manufactures exactly the untrustworthy diagnostics this design exists to avoid.
- **C# (Roslyn)** is the exception that proves the rule: live == build, one
  engine, so the fast/slow split *collapses* — the language server's diagnostics
  are authoritative and near-instant. **Java (jdtls)** sits in between.

This is the root cause of the multi-tier shape: in a unified toolchain "live
analyzer" and "trustworthy" are the same thing; in a split one they are not, and
the harness must treat them as separate tiers because the toolchain does.

## Diagnostic model and the trust mechanics

Every diagnostic the harness surfaces carries a universal envelope, not a bare
message. This is what lets the agent calibrate rather than trust/ignore in
binary:

- **`class`** — `error` (won't compile) vs `lint`.
- **`tier`** — `instant` (in-memory) vs `check` (flycheck) vs `truth`
  (workspace/all-config).
- **`confidence`** — `confirmed` (hard error, or a lint sound at the proven
  scope), `candidate` (config-fragile — e.g. an `unused_import` valid only under
  the checked feature set), `deferred` (needs a truth-tier run).
- **`scope`** — the exact `(pkg, features, targets, triple, rev)` the verdict was
  proven under. Never `unused import: Bar`; always
  `unused import: Bar [pkg=X, features=default, targets=lib, rev=<blob>]`.

Soundness taxonomy by required scope (drives `confidence`/`scope`):

| Class | Examples | Sound at | Fast-tier confidence |
|---|---|---|---|
| Hard errors | type, borrow, unresolved name, syntax | crate as compiled | `confirmed` |
| Function-local lints | `unused_variables`, unreachable | crate | `confirmed` |
| Crate-global, config-sensitive | `unused_imports`, private `dead_code` | crate × (features×targets×cfg) | `candidate` |
| Workspace-global | unused `pub` item, **unused dependency** | whole crate graph | `deferred` (per-crate rustc cannot answer) |

Trust-preservation rules that follow:

- **Precision over recall.** Withhold class-4 (workspace-global) diagnostics from
  the fast tier entirely rather than emit unconfirmable ones. A missed lint is
  cheap (the truth gate / CI catches it); a wrong lint is expensive (chase +
  trust). Silence beats a false positive.
- **Diff-vs-baseline with stable identity.** Surface only *new/changed*
  diagnostics relative to the pre-edit state of the touched compilation unit, and
  identify diagnostics structurally (not by line number) so "shifted down 3
  lines" is not reported as "new." The agent must always be able to answer "did
  *I* just break this, or was it already there?" The per-crate baseline rides the
  harness `side` spine, surviving across turns. This is the single largest
  noise-reducer — re-showing pre-existing warnings on every edit is how an agent
  is trained to ignore the channel.
- **A clean fast tier is a *scoped* claim**, not "all good." Only the truth gate
  converts scoped-clean into clean-period; otherwise the agent over-trusts a green
  fast tier and the trust problem returns from the optimistic side.

## Trigger model: code-state, not agent phase

There is no reliable "the agent is done editing" signal — the assistant stream
interleaves thinking / text / tool calls arbitrarily. So **do not infer agent
phase.** Trigger on facts that are mechanically true: does the touched crate
**compile**, and has a diagnostic **persisted**.

- **Errors** (parse, type, resolution): surface **instantly, every mutating tool
  call.** Mid-refactor breakage *is* the useful signal ("you renamed the def,
  here are the callers that no longer resolve") — it is the agent's worklist, not
  noise.
- **Lints** (`unused_import`, dead_code, clippy): compute every mutation but
  **surface only when (a) the crate compiles clean of errors AND (b) the lint
  persisted across ≥2 consecutive checks.** The compile-clean gate kills the big
  transient-noise case (no unused-import spam while a refactor is half-applied);
  the persistence gate kills the small one (add-import-then-use-next-edit:
  vanishes by the next check, never surfaced). Errors are never delayed; lints eat
  at most one mutation-step of latency. Zero phase inference.

## The truth tier: owned by whoever performs the transfer, never the harness

The expensive pass (`cargo check --all-features --all-targets`, clippy,
`cargo-udeps` — the class-4 workspace-global diagnostics) cannot be window=0, and
**it is not a harness responsibility at all.** This is the load-bearing
correction: ownership-transfer is *intrinsically not harness-observable*, because
the harness is the thing being transferred *from*. You can only observe a transfer
if you perform or receive it; the harness is the payload, not either endpoint.

Concretely, the harness cannot locally know *which* boundary is the transfer:

- **A commit is not a transfer.** An agent commits constantly — WIP, checkpoints,
  progress saves — and in a private worktree none of those share anything; nothing
  is shared until the worktree is collected or merged. "A commit happened" carries
  no information about whether *this* commit is the private→shared instant. Which
  one is, is a fact about the orchestration topology (is this worktree about to be
  collected?) that the harness does not hold.
- **"End of dispatch" is the forbidden boundary renamed.** "Is this my final
  turn?" is identical to "is the agent done?" — the exact unobservable cognitive
  boundary the trigger model above refuses to infer. The loop runs until the model
  stops; "stopped this turn" vs. "done with the task" is invisible from inside it.

So the truth tier is **owned by the entity that performs the transfer**, which
observes the boundary because it *is* the boundary:

| Context | Who owns the truth tier | How it knows the boundary |
|---|---|---|
| **Ensemble** | the orchestrator | it created the drone's worktree and decides when to collect — running the check is part of *performing* collection |
| **Solo** | the operator, or the agent by *explicit* act | a pre-push hook, a human run, or the agent **deliberately calling** `diagnostics_full(scope)` |

- **Ensemble: the orchestrator runs it, not the drone.** A drone's worktree is just
  a directory the orchestrator owns. At collection the orchestrator runs
  `cargo check --all-features` against those files *itself*, in its own process,
  before merging the lane. "Runs in the drone's worktree" means *against those
  files*, not *executed by the drone process*. The drone runs nothing, knows no
  boundary, and there is no harness→daemon call. Per-lane attribution is preserved
  (the check is scoped to lane A's files) without the drone participating.
- **Solo: an explicit act, never an inference.** The trigger is a pre-push hook,
  the human, or the agent calling `diagnostics_full(scope)`. The agent-call case is
  legitimate precisely because it is a *deliberate tool call surfaced as an
  observable act* — the agent declaring "I am done," not the harness guessing it.
  That distinction is the whole line between legal and illegal here.
- **Self-correcting drone (optional).** If a drone should see its own truth-tier
  results and fix them in-lane before returning, the check must run while its loop
  is live — triggered by the agent's *explicit* `diagnostics_full` call or a
  **daemon→harness "finalize" control signal**. Daemon→harness control is allowed
  (the parent signals its child); a harness→daemon query is not. The harness still
  never autonomously detects the boundary.
- **Cross-lane conflict is integration-time-irreducible.** Drone A deletes a
  `pub fn` it proved unused *in its lane*; drone B in parallel started calling it.
  Both lanes individually pass; the conflict only exists after merge. No
  feedback-latency policy can see a conflict that does not yet exist — and this is
  another reason the gate is orchestrator-owned: only the orchestrator sees all
  lanes at once.
- **Cost at fan-out:** truth gates are N parallel workspace-scope checks — N × the
  hub-crate cost. The orchestrator *schedules* them as part of fan-in; it does not
  assume every lane can run `--all-features --all-targets` simultaneously. This is
  only schedulable by the orchestrator — the harness has no view of N.
- **Idle prewarm is the only sanctioned async:** the orchestrator may run the truth
  pass in the background when a worktree's dirty set is stable, so it is warm at
  collection — but the result **must pass a staleness gate before use** (drop if the
  worktree moved). Async is a prewarm only, with drop-stale, never the delivery
  path.

## Cross-language integration: LSP transport + a thin classification adapter

The protocol is **LSP**, and the harness already speaks it. The cross-language
seam is not a new plugin framework:

- **Transport — exists as code, daemon-local today.** The session-management
  logic — `LspSessionManager` pools warm sessions keyed by `(project_root,
  Language)`, resolves binaries from `LspConfig` (`rust_analyzer_bin`, `jdtls_bin`,
  `roslyn_lsp_bin`), evicts on idle, and fails closed (`error.lsp_unavailable`, per
  RX-V3) — already exists, but lives in the `blackbox` lib. Per the dependency
  constraint above it is **extracted into a shared crate** so the harness owns its
  own instance in-process; the daemon links the same crate. Adding a language is a
  4-site registration (`Language` enum + `LspConfig` bin + `launch_argv` +
  `init_timeout`). **C# is already scaffolded** (`Language::Csharp`,
  `roslyn_lsp_bin`); it is unused, not unbuilt.
- **LSP standardizes the *transport*, not the *semantics* this doc is about.** An
  LSP `Diagnostic` carries range/severity/`code`/`source`/tags (including
  `DiagnosticTag::Unnecessary` — the wire-level "unused"), but **not** `tier`
  (server-internal: in-memory vs flycheck is invisible on the wire), `scope`
  (config-sensitivity), or whether live==build. Those are added by a per-language
  **classification adapter** that maps raw LSP fields → the universal envelope
  above.
- **Adapter size tracks the gap to Roslyn.** This is the payoff of the whole
  design:

| Language | Server | Adapter complexity | Why |
|---|---|---|---|
| **C#** | Roslyn LSP | ≈ identity | live==build; publishDiagnostics *is* truth; no tier split |
| **Java** | jdtls | moderate | incremental compiler → fairly authoritative incremental diagnostics |
| **Rust** | rust-analyzer | the full apparatus | two-engine split, flycheck tier, config-sensitivity, out-of-band truth tier |

  The design here is the **worst-case (Rust) shape**; the interface must let each
  language opt into only the tiers it needs. C#'s adapter is nearly empty
  *because Roslyn already solved unification internally* — the final confirmation
  that languages with a "rustlyn" need the least integration glue.

### The diagnostics provider seam (net-new, small)

The diagnostics push channel is **already half-plumbed**: `wait_for_diagnostics`
(`src/lsp/session_manager.rs`) already drains `textDocument/publishDiagnostics`
— but discards the payload, using only arrival as a readiness signal. The deltas:

1. **Stop discarding** the `Diagnostic[]` already received.
2. **Document sync for live edits.** Today only `didOpen` is sent (open → query →
   forget navigation). Window=0 needs a persistent open document with
   `textDocument/didChange` on each edit.
3. **Prefer pull diagnostics** (`textDocument/diagnostic`, LSP 3.17) where the
   server supports it: it is request/response and **version-correlated**, which
   implements the drop-stale invariant *at the protocol level*. Push
   (`publishDiagnostics`) is fire-and-forget with weak version correlation — that
   is the staleness hazard on the wire; fall back to it only with client-side
   version correlation.

### Crate boundary: shared code, separate runtimes

Resolved by the dependency constraint above — the harness never depends on the
`blackbox` lib or its running daemon:

- **window=0 is in-process harness loop policy.** The harness links the shared LSP
  crate, opens a persistent document, `didChange`s on each edit, pulls diagnostics,
  runs the classification adapter, and riders the envelope onto the tool result
  (same append mechanism as `bound.rs`) — all inside the harness process. No
  daemon, no MCP call, no network. After writing to disk it `didChange`s from
  on-disk content and pulls; version = file blob hash.
- **Daemon-down is a supported state.** With the daemon stopped, the harness still
  produces window=0 diagnostics; only the daemon-owned shared-state tools
  (search/knowledge) go away, exactly as the orthogonal use cases require.
- **Ensemble falls out for free:** sessions key on `(project_root, Language)`, and
  a drone's worktree *is* its `project_root` — each harness process gets its own
  isolated warm session against its own tree. Isolation is by process, not by
  key-within-one-shared-pool.
- **Truth tier is orchestrator-owned, not harness-local:** the ownership-transfer
  pass (`cargo check --all-features --all-targets`, clippy, udeps for Rust) is run
  by whoever *performs* the transfer — the orchestrator against the worktree's
  files at collection, or an explicit solo act — never autonomously by the harness,
  which cannot observe a boundary it is the payload of (see the truth-tier section).
  The harness owns only window=0. `truth_check(scope)` is an orchestrator-invoked
  entry point, optionally run in-lane via a daemon→harness finalize signal; for C#
  the LSP server *is* the truth tier.
- **The daemon's own MCP tool is orthogonal.** A daemon-side
  `bbox_code_diagnostics(file, version)` may exist for daemon-dispatched agents
  that are *not* bro-harness (e.g. a `claude`-CLI agent, which cannot host an
  in-process Rust LSP session). It reuses the same shared classification adapter
  and is never the harness's window=0 path.

## Invariants

- **DX-1 (window=0).** A fast-tier diagnostic rides the causing mutation's tool
  result, synchronously, before control returns to the model. No deferral.
- **DX-2 (never stale).** Every diagnostic is correlated to the file version it
  was computed against; a result computed against superseded bytes is **dropped**,
  never shown. A latency window degrades to silence-or-truth, never stale-lie.
- **DX-3 (precision over recall).** Withhold a diagnostic the current tier cannot
  prove rather than emit it. Silence beats a false positive; the truth gate / CI
  is the safety net for misses.
- **DX-4 (scope-honest payload).** Every diagnostic carries `{class, tier,
  confidence, scope}`. No bare claims; a clean fast tier is a scoped claim, not
  "all good."
- **DX-5 (no fix from fragile).** A destructive fix (notably deletion) derived
  from a `candidate`/`deferred` diagnostic is gated on re-verification under the
  union of configs that could use the symbol, or refused.
- **DX-6 (code-state triggers).** Errors surface instantly every mutation; lints
  surface only on compile-clean + ≥2-check persistence. No agent-phase inference.
- **DX-7 (truth tier is not a harness gate).** The expensive workspace-global pass
  is owned by whoever *performs* the ownership transfer — the orchestrator (it
  created the worktree and runs the check at collection) or an explicit solo act
  (push hook / human / the agent's deliberate `diagnostics_full` call). The harness
  owns only window=0; it never autonomously detects a transfer boundary, because as
  the payload being transferred it cannot observe one. Cross-lane smell is
  integration-time-irreducible and orchestrator-owned.
- **DX-8 (LSP transport, adapter semantics).** Transport is LSP; per-language
  semantics live in a classification adapter whose size tracks the gap to a
  unified toolchain. Adding a language adds an adapter, not harness-core changes.
- **DX-9 (no runtime daemon dependency).** The harness obtains diagnostics
  in-process from shared-crate code, never by calling the daemon. bro-harness
  produces window=0 diagnostics with the daemon stopped; the daemon↔harness
  contract stays the stdout stream-json envelope. LSP session management is shared
  as a crate (`bro-tools`/`bro-lsp`), not as a service.

## Dependency map

```
side persistence spine (BUILT) ──→ per-crate diagnostic baseline (diff-vs-baseline)
bound.rs rider mechanism (BUILT) ──→ fast-tier rider delivery
hooks volatile-injection seam (BUILT) ──→ alt delivery for non-tool-result surfacing
LspSessionManager (BUILT, daemon-local, nav-only) ──→ EXTRACT session core to
    shared crate (bro-tools / bro-lsp) ──┬─→ extract diagnostics (stop discarding)
                                         ├─→ didChange document-sync (net-new)
                                         └─→ pull diagnostics where supported (net-new)
classification adapter per Language (NET-NEW, in shared crate) ──→ universal envelope
in-process diag call in harness file_edit/file_write (NET-NEW) ──→ window=0 rider (no daemon)
truth_check(scope) per Language (NET-NEW, ORCHESTRATOR-owned, partly non-LSP) ──→ run at collection against worktree files; harness never triggers it
bbox_code_diagnostics daemon MCP tool (NET-NEW, ORTHOGONAL) ──→ daemon-side non-harness agents only
supervision AlertKind::loop (BUILT) ──→ escalation signal (build-on-smell spiral)
```

**Built / reusable:** the `LspSessionManager` *logic* (+ `(project_root, Language)`
keying, idle eviction, fail-closed) — but daemon-local today and must be **extracted
to a shared crate** before the harness can link it; the `publishDiagnostics` drain,
the `side` spine, the `bound.rs` rider, the hooks injection seam.
**Net-new:** the shared-crate extraction of the session core, diagnostic payload
extraction, `didChange` document-sync, pull diagnostics, the per-language
classification adapter, the diff-with-stable-identity differ, the harness's
in-process window=0 diagnostics call, the per-language orchestrator-invoked
`truth_check` (run against worktree files at collection; an optional daemon→harness
finalize signal lets a drone self-correct in-lane), and (orthogonal, optional) the
daemon-side `bbox_code_diagnostics` MCP tool for non-harness agents.

## Open questions

- **Session-core extraction boundary.** What is the minimal slice of
  `src/lsp/` that moves to the shared crate (session pool, `with_session`,
  `wait_for_diagnostics`, `launch_argv`, `LspConfig`) versus what stays daemon-only
  (MCP adapters, `code_nav`/`refactor` wiring)? And does the daemon *migrate* its
  own LSP consumers onto the shared crate (single implementation) or keep its
  current daemon-local copy (the shared crate is harness-facing only)? Single
  implementation is cleaner but couples a daemon refactor into this work.
- **Streaming drones without a discrete collect.** The orchestrator owns the truth
  gate because it observes the moment it performs collection — but a long-lived
  drone that streams partial results back gives the orchestrator no single
  private→shared instant to gate on. Does the orchestrator then run the truth pass
  against the worktree at each streamed checkpoint, or is the harness's per-lane
  window=0 the only guarantee until a final collect?
- **Pull-diagnostics support matrix.** `textDocument/diagnostic` (3.17) is the
  clean version-correlated primitive; servers vary. Where unsupported, the
  push+version-correlation fallback is more fragile — quantify which of
  rust-analyzer / jdtls / Roslyn / tsserver support pull.
- **Feature-powerset coverage.** The truth tier's `--all-features` is a
  simplification; the real config space (feature combinations × cfg × target
  triples) is exponential. The truth gate covers the *union*, not the powerset —
  some config-specific breakage is only an integration/CI fact. Where is the
  honest cutoff?
