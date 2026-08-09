---
title: "Blame locality transport: checkout fact, corpus enrichment"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
  - bro-harness
  - corpus
tags: [locality, blame, git, workspace, transport]
brief: "Move Git blame execution to the checkout owner while preserving bbox_blame as the user-facing corpus-enriched tool. The daemon plans against path-free corpus identity and joins a bounded authenticated local fact; it never receives a path root, file bytes, or Git objects."
---

# Blame locality transport

> **Status: BL-A through BL-C are implemented and workspace test-verified as of
> 2026-08-09.** This is the active executable arc in
> [locality-first-decomposition.md](locality-first-decomposition.md). It
> preserves the public `bbox_blame(file,line|entity_ref)` contract and the
> corpus-owned anchor/session/brofile/thread join. It moves only the Git and
> working-tree operation to the checkout owner. The authenticated operator
> route, measured overlap store, five-minute quiet-window gate, checksummed
> marker, restart projection, and covered-request refusal are implemented.
> Applying a production marker remains a separate operator-authorized
> ceremony; this document claims no production marker.

## 0. Outcome

For a managed workspace-bound harness:

1. `bbox_blame` remains the only model-facing blame tool;
2. the corpus daemon resolves an entity ref, project, relative path, requested
   line/byte offset, and exact indexed snapshot commit without acquiring a
   checkout lease;
3. the harness validates that plan against its bound workspace and published
   scope, runs Git locally, and returns one bounded typed blame fact;
4. the daemon re-derives the plan, validates the fact, and joins it to corpus
   anchors, sessions, brofiles, threads, and prior reads; and
5. no absolute checkout path, repository root, file body, Git object, pack,
   diff, or shell command crosses the boundary.

Path-addressed blame intentionally describes the bound workspace's current
state, including dirty lines. Entity-addressed blame intentionally describes
the exact commit recorded by the corpus Git overlay. Neither may silently
downgrade into the other.

## 1. Current owner and caller map

The executable path is now split at the locality boundary:

- `src/tools/graph.rs::bbox_blame` derives a path-free plan for an authenticated
  workspace-bound or blame-scoped operator session and joins a returned fact
  without constructing a checkout broker. It re-derives the plan on resolve,
  rejects stale/cross-scope facts, and refuses a bound request that omits
  locality transport. After a BL-C marker is applied, corpus-identity requests
  and stable session-project path requests for covered projects refuse before
  the legacy broker. Raw unscoped paths and bridge calls remain named
  compatibility lanes.
- `crates/bro-harness/src/locality.rs` wraps only the qualified daemon
  capability tool in a live managed workspace. It strips caller transport
  fields, executes current or exact-snapshot Git blame inside the bound
  checkout, and returns the typed fact for central enrichment.
- `crates/bbox-mcp-tools/src/mcp_tools/blame.rs` retains the public schema and
  deterministic corpus join. Its internal transport field is omitted from
  JSON Schema; the legacy Git executor and fact enrichment share one response
  renderer.
- `crates/bbox-corpus-core/src/blame_transport.rs` owns the bounded plan/fact
  contract. `crates/bbox-corpus-core/src/git.rs` owns the hardened current and
  exact-commit Git subprocesses plus the porcelain parser.
- `crates/bro-cli/src/blame.rs` is the operator checkout owner. It derives the
  committed scope and durable workspace id locally, authenticates MCP
  initialization with the existing scope-bound producer token, executes the
  shared plan locally, and can run one legacy comparison during overlap.
- `crates/bbox-indexing/src/blame_locality_observations.rs` persists bounded
  positive-control and checksum-only parity evidence. The separate
  `blame_locality_cutover` facade requires path and entity controls, exact
  equal comparisons, stable producer/catalog authority, and no changed
  daemon-side `Blame` checkout counter through a minimum five-minute window
  before writing its checksummed marker.
- Bridge parity, catalog capability, remote-only refusal, exact snapshot,
  dirty-line, deleted-working-file, path-confinement, response-parity, expired
  binding, and zero-checkout-observation tests pin the split and legacy
  behavior.

The Git-history producer transport cannot answer blame centrally: its bounded
fragments contain commit metadata and changed paths, not the historical blobs
and line ancestry Git blame requires. Extending it into a Git object transport
would violate the locality program and is rejected.

## 2. Fixed decisions

### BL-D1: Split execution from enrichment, not the public tool

The public schema and response remain `bbox_blame`. In a workspace-bound
harness, the existing MCP tool object is wrapped locally. The wrapper performs
an internal plan call, executes the plan, and performs an internal resolve
call. Only the final enriched result reaches the model.

The internal request arm is absent from JSON Schema and rejected unless the
MCP session has a live daemon-minted workspace binding. The wrapper removes
any caller-supplied internal fields before constructing its request, so model
JSON cannot select or forge the transport arm.

### BL-D2: Plans are path-free authority plus a checkout-local request

`BlameExecutionPlanV1` binds:

- version, project id, published scope, and workspace id;
- either a caller path plus explicit line for current-state blame, or a safe
  project-relative path plus line/byte offset and exact overlay commit for
  corpus-entity blame; and
- a bounded display hint that never becomes file authority.

An entity plan is derived from the pinned corpus read view. A path plan is
bound to the session's project/workspace; the checkout owner alone resolves
the caller's path. The plan carries no daemon attachment id or host root.

Resolve re-runs planning from the original public arguments and requires exact
plan equality. A corpus generation or overlay change between the two calls is
a typed stale-plan refusal, not a fact accepted against new authority.

### BL-D3: Facts contain the answer, never the substrate

`BlameFactV1` contains only:

- project/scope/workspace binding;
- safe Git-relative and project-display-relative paths;
- the resolved 1-based line;
- whether execution used current workspace state or one exact snapshot
  commit; and
- optional Git attribution: introducing commit, bounded author/time, and the
  porcelain-reported relative path.

`not_found` is an explicit fact arm. Paths are relative, SHA values are
canonical for the repository object format, strings and total encoded bytes
are bounded, and no fact carries a repository root.

### BL-D4: Trust is the existing managed workspace capability

The first implementation accepts internal plan/resolve calls only on the live
workspace-bound self-MCP session. That capability already binds task, session,
project, published scope, workspace id, and expiry, and is not inherited by
shell grandchildren. The harness is the trusted checkout owner; the model is
not. This adds no credential family and does not expose the binding secret.

An operator-attended `bro blame` path later needs an equally explicit
scope-bound capability. It must not turn a public unauthenticated MCP caller
into an arbitrary fact authority merely to retire the last daemon adapter.

### BL-D5: Preserve exact semantic asymmetry

- `file + line` runs against the bound working tree. Relative inputs resolve
  under the harness cwd; absolute inputs must remain inside the bound project.
  Symlink escape and non-files refuse.
- `entity_ref` runs at the exact Git overlay commit and resolves byte offset
  against that commit's file bytes. A missing commit/file refuses; it never
  reads the current working file.
- The attributed commit returned by Git may precede the execution snapshot;
  the fact carries both concepts and validation never conflates them.

### BL-D6: Corpus enrichment remains central and deterministic

The local fact is joined using the existing exact-path-first, same-commit
fallback anchor selection, prior-read window, session, brofile, thread, and
text/JSON rendering. Refactoring extracts this join from Git execution; it does
not change ranking or output fields.

## 3. Implementation phases

### BL-A: Pure contract and split domain helper — complete

1. Add bounded serializable plan/fact types below both runtime implementers.
2. Move exact-commit line/offset execution into the hardened Git leaf so the
   daemon legacy adapter and harness use one implementation.
3. Split `bbox-mcp-tools` blame into local execution and corpus enrichment,
   preserving byte-for-byte response behavior.

Gate: contract validation, dirty/committed/not-found fixtures, and existing
blame tests unchanged.

The landed gate validates relative/bounded plans and facts, exact authority
matching, the shared exact-commit leaf, and byte-for-byte equality between the
legacy Git response and enrichment of the equivalent typed fact.

### BL-B: Managed harness route — complete for managed workspaces

1. Extend the harness locality wrapper to replace only the daemon capability
   server's `bbox_blame` in a live bound workspace.
2. Add daemon plan/resolve arms that use the session workspace grant and never
   call the checkout broker.
3. Prove path mode, entity mode, dirty lines, exact old commit after checkout
   advance, deleted working file at snapshot, cross-project refusal, stale
   plan refusal, caller internal-field stripping, expiry, and no secret/path
   logging.

Gate: every positive control executes real local Git; the matching bound
request adds zero `Blame` checkout observations; public response parity holds.

The landed gate exercises real current, dirty, exact old-commit, and
deleted-working-file Git states; outside-project, cross-project, stale-plan,
expired-binding, and caller-forged transport refusals; absence of the internal
arm from public schema; absence of absolute checkout roots from the transport;
and an unchanged checkout-broker observation sequence across plan and resolve.

### BL-C: Overlap, operator path, and strict retirement — complete in code

1. Add an authenticated scope-bound operator CLI route using the same plan and
   fact types; do not accept unauthenticated facts.
2. Observe daemon `Blame` leases by caller category over a declared window and
   compare representative legacy/local results.
3. Close the daemon adapter only for covered callers, prove no fallback after
   capability/source loss, then retain bridge or other named compatibility
   lanes until their own authorization.
4. Update the governing adapter inventory and operations runbook before moving
   to project render.

Gate: path/entity parity, bounded payloads, dirty and committed state,
scope/commit binding, restart behavior, zero-use evidence for every retired
category, exact full cluster verification, and explicit operator authority for
any production cutover.

The landed route uses a blame-only initialization grant rather than the wider
managed-workspace capability. The token never enters the URL, result, shell
environment, or fact. `bro blame --verify-overlap` runs the legacy adapter once
in a separate unprivileged MCP session, hashes the canonical response, and
persists only identity plus local/legacy checksums. A mismatch refuses.

The offline preflight accepts an explicit project-id set and requires operator
positive controls plus equal comparisons for both path and entity targets. It
captures the catalog identity, unique producer assignment, comparison records,
and project/kind-specific checkout counters. Apply is unavailable until at
least 300 seconds have elapsed, holds the offline catalog lifetime claim, and
refuses any changed catalog, assignment, comparison, or `Blame` checkout
counter. The marker is checksummed and startup fails closed if it is corrupt.
Once loaded, its covered-project set is monotonic no-fallback authority.

The covered-request proof exercises the real `bbox_blame` path and demonstrates
that both a stable-project path request and corpus-entity target refuse before
the first checkout observation. An uncovered catalog positive control still
executes the legacy adapter. Bridge and unscoped raw-path compatibility are not
silently retired. The production runbook is in `docs/operations.md`.

## 4. Non-goals

- No Git object, pack, patch, diff, repository mirror, or remote filesystem
  transport.
- No precomputed whole-repository blame index.
- No model-selected project, workspace, scope, commit, fact, or internal phase.
- No change to anchor matching, prior-read selection, or public response shape.
- No reuse of the knowledge cutover marker; blame has a distinct adapter gate.
- No retirement of bridge or unbound operator behavior merely because managed
  harness routing is complete.

## 5. Parent-plan effect

BL-A through BL-C remove managed and explicitly covered operator blame
execution from blackboxd. Project render is next; the local project-file walker
remains governed by the collector cutover.
