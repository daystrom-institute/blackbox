---
title: "Project render locality: authorized view, checkout-owned projection"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
  - bro-harness
  - knowledge
tags: [locality, render, knowledge, workspace, transport]
brief: "Keep bbox_render model-facing while moving project-file writes into the checkout owner: the bound harness for workspace-bound sessions, and the code collector that owns the checkout for every other MCP caller. The corpus supplies a bounded published render plan; a bound harness overlays its checkout's uncommitted knowledge onto it locally, the owner invokes the shared renderer, and returns an exact path-free receipt."
---

# Project render locality

> **Status:** the bound-harness route and the checkout-owner collector route
> are implemented. Bridge and `LegacyLocal` project renders, and catalog
> projects no owner covers, keep their named compatibility adapter, which a
> daemon without checkout authority never takes.

## 0. Outcome

`bbox_render` remains the model-facing tool. A project render completes in
the checkout that owns the project, through one of two appliers:

- a managed workspace-bound harness applies renders for its bound workspace;
- the code collector that owns the project's checkout applies renders for
  any other MCP caller, bound or unbound to a checkout, when a producer grant
  makes it the project's owner.

In both lanes:

1. the corpus builds a bounded path-free render plan from the published
   knowledge view;
2. the applier validates that plan against its own authority (bound published
   scope and workspace, or delivered operation and committed checkout
   identity), and a bound harness overlays its checkout's uncommitted
   knowledge onto a workspace plan (RL-D3); the applier invokes the shared
   renderer and may write only fixed provider
   filenames and `.bbox/guidance` satellites at its project root;
3. the applier returns projection hashes, byte counts, local `PROJECT.md`
   presence, and per-output dispositions, never a checkout root or file body;
   and
4. the daemon validates every receipt field against the plan without
   acquiring a checkout.

Global rendering remains daemon/operator-host local. For `scope=both`, the
daemon performs the global half first, then the owner performs the project
half, preserving the existing operation order and combined response. A failed
global half issues no project operation.

## 1. Executable owner map

- `crates/bbox-project-render` is the dependency-clean leaf every applier
  links: the knowledge entry model, provider projections, plan and receipt
  contracts, the collector render-lane wire contract, and hardened checkout
  IO. It links no store, chunker, indexer, or daemon/runtime crate, so the
  collector stays inside `scripts/acceptance-code-collector-deps.sh`.
  `bbox-knowledge` re-exports its types and renders through it.
- `src/tools/render.rs` routes the call: a live workspace binding uses the
  locality exchange; otherwise a catalog project with a checkout owner goes
  to `src/server/render_owner.rs`, and only a project no owner covers, on a
  daemon that holds checkout authority, reaches the compatibility checkout
  lease.
- `src/server/render_owner.rs` selects the owner, builds the plan under
  producer authority, waits a bounded time for the owner, revalidates the
  receipt, and serves explicit recovery of an earlier operation.
- `src/server/render_operations.rs` persists operations and their immutable
  plan bytes, tracks render-lane presence per producer, delivers
  `render_project` commands, pages plans, and records results.
- `crates/bbox-code-collector/src/render_lane.rs` polls the render lane,
  verifies each delivery against its configured projects and committed
  identity, applies the plan, journals the result, and reports the receipt.
- `crates/bro-harness/src/locality.rs` wraps only the qualified daemon
  `bbox_render` capability in a live managed workspace. It strips any
  caller-supplied transport field, keeps an absolute project path local, and
  delegates global-only calls unchanged.
- `crates/bbox-indexing/src/render_issuances.rs` remembers, in memory and
  bounded, the workspace plans this daemon issued, so a workspace completion
  can be ordered by its plan's issuance.

## 2. Fixed contracts

### RL-D1: transport carries selected knowledge, never a checkout path

`ProjectRenderPlanV1` binds version, project id, published scope, exactly one
authority (a workspace id, or a producer id with operation id and sequence),
provider selection, dry-run flag, normalized request scope, view,
authorized project entries, and bounded diagnostics. The view is the fixed
wire constant `published`: the plan field stays so every reader of transport
version 3 decodes the same shape, and a reader refuses a plan naming any
other view. Project entries are
rewritten to a constant transport scope; every entry must carry the plan's
project id. Count, diagnostics, and encoded plan bytes have hard bounds.

The wrapper converts a path that resolves to its bound project into the opaque
`$bound-workspace` selector before calling the daemon. Stable project ids and
aliases may still cross as selectors. Another path, a moved root, or a
workspace/scope mismatch refuses locally.

### RL-D2: the shared renderer remains the byte authority

Every applier invokes the same leaf projection used by the daemon
compatibility path and the candidate-tree check. Projection
generation has a pure seam parameterized by the local nonempty-`PROJECT.md`
fact, so the daemon can independently validate every provider hash without
seeing the project root.

Only these project targets are valid:

- `CLAUDE.md` for `claude`;
- `AGENTS.md` for `agents`, `codex`, or `vibe`; and
- `GEMINI.md` for `gemini`.

Satellites are content-addressed `.bbox/guidance/<generation>/<provider>-<topic>.md`
files. Unknown or path-shaped provider names fail before a target is joined.
Existing hand-authored provider files retain the established refusal
behavior; generated provider files are replaced. The receipt records each
disposition.

### RL-D3: plans are published; a bound harness overlays its own checkout

Every plan carries the published knowledge view, whoever applies it. A
collector-applied plan is rendered exactly as issued; the collector's
working-tree knowledge is never substituted, and no workspace binding is
invented.

A bound harness diffs its checkout's `.bbox/knowledge` working files against
the checkout HEAD: a changed or new entry file upserts its entry, and a
deleted or retired entry tombstones its id. It applies that overlay to the
workspace plan locally (`ProjectRenderPlanV1::with_local_overlay`) before
invoking the renderer: tombstoned ids leave the plan, upserts replace or add
rows by id, and every applied row is rebound to the plan's project authority
so local bytes cannot widen the render. Working reads are nofollow and
bounded (a symlinked member is skipped), and an invalid working entry
refuses the render before any write. Uncommitted knowledge
never travels to the daemon, and a producer plan is never overlaid. With no
local changes the harness executes the published plan unchanged.

### RL-D4: completion is exact but path-free

`ProjectRenderReceiptV1` binds the same project, scope, and authority and
contains one fixed-output record per selected provider entrypoint and
satellite. Receipts are bounded and path-free. Each
record carries disposition, expected SHA-256, and expected byte count. The
daemon reconstructs the plan, recomputes projections from the shared
renderer, and rejects stale plans, provider cardinality drift, mismatched
bytes, or impossible dispositions before recording completion.

A receipt for an overlaid execution carries `local_overlay_sha256`, the digest
of the overlay the harness applied. The daemon cannot predict those bytes, so
it validates such a receipt by shape instead: workspace authority, a
well-formed digest, one entrypoint per selected provider in order, guidance
satellites after them under `.bbox/guidance`, unique outputs, dispositions
coherent with the plan's dry-run intent, and no entrypoint published over a
failed satellite. A producer receipt naming an overlay is refused. A receipt
without the digest is validated byte-exactly against the plan.

The user-facing result stays local because it includes the local path. Only
the receipt crosses back to the daemon.

### RL-D5: the daemon writes a checkout only with checkout authority

An unbound render of a catalog project resolves stable identity and completes
through its checkout owner whenever one covers it. With no owner, the daemon's
own `RenderFileProvider` write lease is the compatibility adapter, and only a
daemon that holds checkout authority takes it. A daemon declared with
`daemon.no_checkout_authority` refuses before the checkout broker with the
owner-required message, so no checkout access is attempted or counted. A
managed bound call always requires locality transport.

### RL-D6: owner authority is the scope grant

The collector owner of a catalog project is the producer whose config pin or
durable claim grants the project's published scope, resolved to that exact
catalog project. Only after the grant selects exactly one producer do three
live facts decide whether it can complete now: a render-lane poll within the
presence window, support for the current transport version, and the
collector's own report that it holds a configured or enrolled checkout for
that scope. Enrollment-root prefixes and checkout paths never select an owner,
so identical paths on different hosts cannot redirect a render. Each failure
is a distinct refusal that names the setup restoring completion (ambiguous or
mismatched grants, a collector that never polled the lane or is stale, an
unsupported transport, a scope the collector does not hold) and never falls
back to a daemon checkout. With no owner, the refusal names enrollment of the
checkout with its code collector and the onboarding resource; it never names a
session type as the remedy.

The collector re-verifies each delivery before applying it: the scope must be
one it configures or enrolled, the root must be a main worktree, and the
committed `.bbox` identity at HEAD must equal the delivered scope.

### RL-D7: operations are durable, ordered, and explicitly recoverable

Each fresh collector render is a new operation: a random `ro-` id and a
per-project sequence, bound into the plan's producer authority, with the
immutable plan bytes persisted before delivery. A newer operation supersedes
older pending ones for the same project; superseded operations are no longer
delivered or paged. Delivery redelivers an unacknowledged operation after the
redelivery window. Plan pages recheck the producer grant on every page.

The owner journals each operation's preflight record (every output's
observed state and the local `PROJECT.md` fact) durably before its first
write, and records the receipt before reporting it. A redelivered operation
that already applied reports its recorded receipt without applying again. A
redelivered operation that was interrupted after its preflight record is
reconciled under the checkout lock without writing: an output holding the
planned bytes is reported written, one still holding its preflight bytes as
not published, and anything else as a conflict, so owner edits made since the
interruption survive. An output that cannot be inspected is reported as a
conflict and the receipt is incomplete; a reconciliation that cannot run,
including when the owner cannot verify its checkout, leaves the operation
pending for redelivery with its preflight record intact. Neither is ever reported as
an application that wrote nothing. An operation whose sequence is older than
the newest one applied to that scope is refused without writing. The daemon records the owner's exact result; an identical
duplicate is `already_settled`, a different one conflicts.

The MCP call waits a bounded time. A pending operation returns its id and the
recovery arguments; `bbox_render(project, operation)` retrieves that
operation's recorded outcome and never re-applies it. Completion is reported
as current only when the owner and the rebuilt plan still match the issued
plan and no newer operation exists for the project; otherwise the receipt is
reported as stale or historical. The
validation that held at completion is kept as history; present validity is
rechecked on every response, so a receipt stops being current when knowledge
or owner authority changes even if no newer render was issued. An incomplete
receipt, from either applier, is accepted and reported as incomplete. A
bound-workspace completion that may have written makes older owner receipts
of its checkout scope historical: it is ordered by its plan's issuance when
this daemon remembers issuing that plan, and after every issued render
otherwise, so a delayed older completion never makes a newer render
historical. A fresh render with unchanged knowledge still starts a new operation, so it
re-observes `PROJECT.md` and restores deleted generated outputs. Operation
state survives daemon and collector restarts.

### RL-D8: owner IO is preflighted, serialized, and truthful

An applier preflights every target before writing: a symlinked root, parent,
or target, or a special file, refuses the whole render. Satellites publish
before entrypoints. An entrypoint that already holds the planned bytes is
re-observed before it is reported written. Otherwise the output stages in a
unique sibling, the current target is moved aside, and the staged output is
published without clobbering only if the moved bytes are exactly the bytes
preflight observed; owner bytes written at any point are restored or kept
beside the target and the receipt reports a conflict. The moved-aside
original is never unlinked by a render, because a writer may still hold it
open: after publication it moves into the bounded `.bbox/local/render-backups`
directory (or stays beside the target), and every error or uncertain exit
restores it or leaves it under its sibling name. Only the oldest backups
beyond the bound are pruned. A publication failure
after preflight is reported per output as a partial render, a failure that
may follow an effect (such as the directory sync) marks the receipt
incomplete, and only failures before the first write are reported as having
written nothing. A receipt cannot claim an entrypoint published ahead of a
failed satellite.

Renders of one checkout, from the harness, the collector, or the daemon
compatibility adapter, serialize on an advisory lock of the checkout root
directory. Under that lock a freshness fence compares the plan's daemon-clock
issuance (the producer authority's, the workspace plan's first chunk, or the
compatibility adapter's render instant) with the newest issuance already
applied to the checkout, recorded in ignored `.bbox/local` state. An older
plan is refused before any write, and an application advances the fence
durably before its first output write, so a delayed applier never replaces
output that a newer render produced, whichever applier produced it and even
if that render was interrupted.

### RL-D9: both upgrade directions stay compatible

The render lane is separate from the producer enroll channel. A collector
proves the capability by polling the render lane; one that never polls it is
never offered a `render_project` command, and the enroll command encoding is
unchanged. A new collector that finds no render lane on an older daemon backs
off and keeps enrolling and publishing. Workspace plans serialize without the
producer field, so their plan bytes are unchanged; the first plan chunk gains
an optional issuance that older harnesses ignore and newer ones pass to the
fence.

## 3. Compatibility and parity

Catalog compatibility render remains available only for projects no checkout
owner covers, on a daemon that holds checkout authority, and is gated directly on
`render_output`; it no longer takes an unrelated
`repo_mutation` capability. Accepted catalog rows render by stable project id
rather than disappearing behind the historical path-only filter. Bridge
render keeps its existing worktree/base-path behavior.

The test gates cover:

- caller transport stripping and absence of checkout roots on the wire;
- actual writes through the shared renderer under a canonical bound root;
- fixed-target confinement and provider path-injection refusal;
- published-only plan contents for bound and unbound callers;
- local overlay upserts and tombstones in a bound harness render, and
  shape validation of the overlaid receipt;
- exact candidate-tree check parity;
- zero daemon checkout observations for plan and completion;
- positive catalog compatibility for a project no owner covers;
- pre-broker refusal, with zero checkout observations, on a daemon without
  checkout authority;
- owner completion from an unbound caller with zero daemon checkout
  observations;
- owner selection by grant, and each distinct owner refusal;
- operation timeout and recovery, daemon and collector restart, lost
  acknowledgment, duplicate results, supersession, stale completion, and
  partial renders; and
- unsafe-target refusal, concurrent-edit preservation, and serialized
  application.

## 4. Non-goals

- No arbitrary output filename or project root in the transport.
- No second renderer in the harness or the collector.
- No transport of uncommitted knowledge to the daemon.
- No movement of global render authority into a remote workspace.
- No implicit retirement of bridge, uncovered, or `LegacyLocal` adapters.

## 5. Parent-plan effect

Project render is not a checkout reach-in for any project a checkout owner
covers, nor for any project on a daemon without checkout authority.
