---
title: "Durable project catalog Phase 6 post-cut repair implementation plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [catalog, post-cut, repair, vectors, checkout-lifecycle, cutback, selector-retirement]
brief: "Repair the production liveness and durability defects exposed after the catalog cut: remove full-graph vector diagnostics from mutation paths, make selector retirement batch-durable, make checkout lifecycle writers fair without self-deadlocking existing write leases, and serialize cutback redrive through the catalog reconciler."
---

# Durable project catalog Phase 6 post-cut repair implementation plan

Date: 2026-08-07

Status: proposed. Production remains stopped. This plan must receive an exact
`PASS` from the frozen Kimi catalog-plan reviewer before implementation starts.
Implementation must receive the frozen Kimi code-review `PASS`, the full
workspace gates, and separate operator approval before production is started.

Governing design:
[`durable-project-catalog-impl.md`](durable-project-catalog-impl.md), especially
sections 9 through 17.

Binding phase contracts:

- [`durable-project-catalog-phase4-impl.md`](durable-project-catalog-phase4-impl.md)
  for the closed cutback state, one project-keyed reconciler, transition guard,
  scheduler, and selector-retirement rules.
- [`durable-project-catalog-phase5-impl.md`](durable-project-catalog-phase5-impl.md)
  for checkout leases, publication fences, and capability health.
- [`durable-project-catalog-phase6-impl.md`](durable-project-catalog-phase6-impl.md)
  for the completed destructive cut, retained rollback assets, and the absolute
  bridge-retirement gate.
- [`distributed-code-source-collector-impl.md`](distributed-code-source-collector-impl.md)
  for selector activation/retirement, vector tombstones, and full-rebuild
  equivalence.
- [`DECISION_LEDGER.md`](../DECISION_LEDGER.md) remains the decision authority.

Material implementation-time choices are recorded in `DECISION_LEDGER.md`
before the milestone that consumes them lands. In particular, changes to the
lifecycle state machine, publication-pin ownership, registration outcome/CAS,
retirement coordinator, or persisted compatibility posture are not left as
code-only decisions.

Implementation baseline: `beta/blackbox-v2` at `50632160`. An exploratory
post-incident patchset is retained outside the active branch for forensic
comparison only. No implementation from that patchset is adopted by default.

## 1. Required outcome

This is a bounded repair phase after the catalog operational cut. It restores
daemon liveness and closes the correctness holes found while diagnosing the
stalled production daemon. It does not reopen the completed Phase 6 code span
or authorize bridge retirement.

At the exit gate:

1. Normal vector deletion, checkpointing, automatic store compaction selection,
   and cheap status collection never run full HNSW connectivity diagnostics.
   The explicit quiesced workflow compaction/status lane retains diagnostics as
   its connectivity candidacy and policy-trigger axis.
2. Selector retirement deletes vector rows in bounded batches with one durable
   batch boundary, explicit partial-failure accounting, and no per-entity
   full-partition scan or checkpoint.
3. Checkout lifecycle mutations have bounded writer preference. New readers
   cannot starve a waiting lifecycle writer, while a publication fence backed
   by an already-held write lease cannot refuse itself.
4. Exact idempotent checkout registration does not acquire exclusive lifecycle
   authority. The unchanged/changed decision is made by the registry authority,
   not by a stale adapter snapshot.
5. Every catalog cutback state mutation is serialized by the one project-keyed
   reconciler and guarded by an exact activation identity check. Readiness
   notifications enqueue reducer events; they never perform direct durable
   load-modify-write transitions.
6. Error classification is typed or exact-marker-based. Incidental words in
   paths, wrapper messages, or diagnostics cannot create a terminal cutback.
7. A selector-retirement row that is temporarily active remains durable,
   observable, and re-driven when authority changes. Completion wakes the
   affected catalog transition without bypassing `ManualRetryRequired` or
   `Terminal` policy.
8. A fresh production build passes all gates and frozen Kimi review before any
   redeploy. Production remains offline on any failed gate or smoke probe.

## 2. Incident evidence and repair boundary

The production sample attributed nearly all sampled CPU time to
`HnswIndex::metrics`, reached from selector retirement through vector deletion.
The installed binary predated the exploratory patchset, so the hang is a
baseline production defect rather than a bad deployment of that patchset.

The exploratory patchset did identify useful test surfaces, but review exposed
additional defects: an unguarded cutback redrive could clobber newer durable
state; classifier substring ordering could turn incidental `security` text
into `Terminal`; a daemon-local not-ready ceiling reset rather than converged;
writer preference could reject the nested publication fence of the write lease
it was waiting for; and batch deletion could return before the final durability
flush. The shipped reducer also has a dead Phase 4 policy cell:
`ManualRetryRequired` returns `NoOp` for config reload as well as every other
event, so the sanctioned config-reload release has never worked unless a
producer reassignment first cancels the cutback. This plan re-derives the repair
from the certified baseline and treats the exploratory mechanisms as rejected
until independently proved.

The repair touches only:

- `bbox-vectors` mutation accounting and diagnostic separation;
- the index writer's selector-retirement vector sink;
- checkout lifecycle admission, publication fencing, and registry registration;
- catalog reconciler event provenance, cutback persistence, and live selector
  retirement scheduling;
- focused status/health projections and tests required to prove those changes.

## 3. Fixed decisions

### 3.1 Cheap vector accounting is authoritative on hot paths

`PartitionMetrics` is split conceptually into two costs:

- cheap structural accounting, derived from slab and partition metadata; and
- explicit HNSW diagnostics, derived from graph traversal.

The existing public partition status remains cheap. Its `active_count`,
`deleted_count`, and `deleted_ratio` come from slab truth, not HNSW graph
metrics. This is load-bearing after WAL replay because the rebuilt HNSW contains
only active rows while the slab still owns deletion history.

Full HNSW diagnostics move behind a separately named diagnostic method and
type. The diagnostic method may compute edge counts, layer distribution,
disconnected nodes, and zero-in-degree nodes. It is never called by:

- WAL append or replay;
- `Partition::delete`, batch delete, push, or checkpoint;
- `flush_derived_files` or `flush_derived_full`;
- automatic store `needs_compaction` or its cheap candidate ranking;
- ordinary status/health endpoints;
- selector retirement.

`meta.json` fields needed for restart or compaction are written from cheap
partition counts. Connectivity diagnostics are observational and are not
checkpoint authority.

The existing serialized `PartitionMetrics.hnsw: Option<_>` field is retained
for compatibility. Cheap status returns it as `None`; it is neither removed nor
silently populated by a full traversal. An explicit embed diagnostics action
returns the same outer partition DTO with `hnsw: Some(_)`, is documented as
expensive, accepts a bounded route set and deadline, and reports
unavailable/timeout rather than fabricating healthy connectivity.

Every current consumer is converted explicitly:

- workflow `exec_read_vector_status` obtains explicit diagnostics in bounded
  route pages and preserves `max_connectivity_route` and
  `max_connectivity_ratio`; the `embed/compaction-policy` packet therefore
  retains its connectivity greater-than-5-percent compact trigger and
  greater-than-2-percent notify trigger end to end;
- workflow `exec_compact_vector_partitions` retains two independent candidacy
  axes: cheap deleted ratio and explicit connectivity risk. It obtains bounded
  diagnostics for the quiesced route inventory before ranking, unions both
  candidate sets, and orders by the worse ratio exactly as today. A breached
  graph becomes a preferred compaction candidate because rebuild is the repair;
  it is never refused for being breached. Diagnostic unavailability refuses
  only that route's connectivity-driven compaction and is reported distinctly
  from healthy. Deleted-ratio candidacy for a route remains available from
  cheap metrics;
- the workflow records before/after connectivity diagnostics around rebuild so
  the repair's recall effect remains observable;
- the inbox attention pass uses the nonblocking bounded diagnostics action: a
  breached graph raises the existing alert, while diagnostic unavailability is
  reported separately from a healthy result;
- `bbox_embed_status` remains cheap by default and exposes diagnostics only
  behind its explicit request flag/action;
- doctor explicitly requests the bounded diagnostic response when evaluating
  connectivity; hybrid-search status DTOs preserve the optional serialized
  field but cannot call `connectivity_breach` on `None`;
- tests and fixtures that construct `PartitionMetrics` state explicitly whether
  they are testing cheap status or a diagnostic response.

`connectivity_breach` moves to the explicit diagnostic type. There is no API
whose missing `hnsw` value evaluates to `false`. Compaction eligibility and
candidate ordering inside the vector store use cheap deleted ratio plus a
stable tie-breaker. The explicit workflow lane continues to merge diagnostic
connectivity candidacy with that cheap axis, so the invariant that previously
detected and repaired disconnected graphs is retained.

### 3.2 HNSW id lookup is derived, active-only, and O(1) per requested id

`HnswIndex` gains an in-memory `active_ordinal_by_id` map. It is derived state,
not a new snapshot or WAL field.

The invariants are:

- at most one active ordinal exists for an entity id;
- historical tombstoned duplicates may remain in the append-only vectors;
- build, snapshot load, and WAL recovery reconstruct the map from active rows;
- `push` tombstones the prior active ordinal through the map before inserting
  the replacement and updates the map only after the new row is valid;
- single delete removes the active map entry and tombstones that ordinal;
- batch delete resolves requested ids through the map, tombstones each active
  ordinal once, and repairs the entry point once after the batch;
- validation refuses two active rows for the same id instead of choosing one.

The map is not serialized. Reconstructing it is linear at open/rebuild time;
normal deletion is proportional to requested ids rather than partition size.

### 3.3 Vector batch durability has one owner and an explicit result

Add a route-local batch primitive returning a typed result with at least:

- requested id count;
- WAL tombstones appended;
- active rows removed;
- ids already absent;
- whether a checkpoint completed.

The vector store groups selector-retirement ids by route, chunks them at a
fixed bounded size, and holds a partition write lock for one chunk at a time.
For each chunk it:

1. validates and deduplicates input without mutation;
2. appends the unconditional delete tombstones as one WAL batch;
3. applies slab and HNSW tombstones through their batch APIs;
4. synchronizes the WAL and writes derived files once;
5. releases the lock before the next chunk.

Unconditional tombstones for absent ids are preserved. An all-absent batch
still durably records its tombstones, because selector retirement must survive
a crash even when local derived state was already missing.

The operation does not promise transactionality across routes or chunks. On
failure it returns the completed chunk count and the failing route/chunk. Any
mutation after a successful WAL append remains recoverable from WAL. The code
must attempt the chunk's durability boundary before returning an error caused
after in-memory mutation; it must not silently skip the last flush. A retry is
idempotent because delete tombstones and active-row deletion are idempotent.

The index writer consumes this batch result. It reports the exact completed and
remaining entity counts in bounded health/log fields. It does not issue one
`delete_entity_all_routes` call per entity.

Batch adoption covers every internal multi-entity deletion lane in this phase:
selector retirement, embed-queue deletion, and the currently unwired history-GC
tombstone sink. The history-GC sink trait becomes batch-shaped even though the
production sweep remains disabled. A public/single-entity compatibility wrapper
may remain, but no loop in these lanes calls it. This prevents the new primitive
from becoming retirement-only while another bulk lane retains per-id flushing.

Cross-route unconditional absent-id tombstones are an explicit retained
semantic, not accidental amplification: one requested entity produces one
tombstone in every addressed route exactly as before, now grouped into WAL
batches. Deduplication prevents repeated input ids from multiplying records,
WAL accounting exposes appended tombstone counts, and ordinary checkpoint/
compaction reclaims the resulting log history. The repair does not introduce a
route-membership oracle that could suppress a crash-relevant delete.

### 3.4 Lifecycle admission is a state machine, not a zeroing flag

The checkout lifecycle gate has three logical states in one guarded authority:

- shared pin count;
- one pending exclusive claimant;
- exclusive held.

Only one exclusive claimant may be pending. Once pending is installed, new
shared-pin admission refuses with `LifecycleBusy`; existing pins continue to
completion. The claimant waits on a condition variable for a bounded duration,
then atomically becomes exclusive only when the shared count is zero. Timeout
clears its own pending claim and wakes waiters. Exclusive drop clears only the
exclusive state; shared-pin drop decrements only the count. No drop path uses
`store(0)` against a composite word.

The production lifecycle mutation API uses the bounded wait. A zero-wait
`try_` form is private or test-only. No synchronous mutex or condition wait is
performed on an async executor thread; async callers use the existing blocking
boundary.

The bound is configuration-owned as
`daemon.checkout_lifecycle_writer_wait_ms`, default `500`, validated in the
inclusive range `1..=5000`. `CheckoutAccessBroker` receives the resolved
duration at construction; tests may inject shorter deterministic durations.
The value is reload-stable for a broker lifetime and a config change takes
effect on daemon restart rather than mutating an in-flight gate.

The state transitions have model/property tests for:

- readers admitted before pending drain normally;
- readers arriving after pending are refused;
- the writer acquires after the last prior reader drops;
- timeout removes pending without corrupting the pin count;
- only one pending writer exists;
- no underflow, leaked pending bit, or exclusive-plus-pin state is reachable.

### 3.5 A write lease is already a publication pin

`ValidatedCheckoutLease` retains its acquisition intent. A write lease owns a
lifecycle mutation pin for its complete lifetime. Its publication fence
therefore revalidates authority but does not acquire a second pin.

Publication guard rules are:

- a read lease acquires one new publication pin before revalidation and the
  returned guard owns that pin;
- a write lease revalidates under its already-held pin and contributes no new
  pin, but the returned guard clones and owns the write lease's combined
  lifetime guard (including its mutation pin) for the guard's complete
  lifetime;
- a multi-lease publication acquires at most one new pin, and only when no
  contributing write lease already supplies the global lifecycle fence;
- all leases are revalidated after the required fence exists;
- the lease iterator is collected and an empty set refuses with
  `InvalidRequest` before any pin acquisition, even while an exclusive writer
  is pending;
- guard construction failure releases only a pin it acquired itself.

Because lifecycle authority is global to the broker, one valid write-lease pin
fences the atomic multi-lease publication. This explicitly prevents a pending
lifecycle writer from refusing the completion fence of the write operation
whose existing pin it is waiting for.

### 3.6 Idempotent registration bypasses exclusivity inside registry authority

`CheckoutRegistry::register` returns a typed `RegistrationOutcome`:
`Unchanged` or `Changed`. The equality check is against the current on-disk
store loaded under the registry's existing mutation lock. Adapter code does not
compare a cached snapshot and then decide whether to take lifecycle authority.

The broker/registry integration uses a two-stage operation:

1. registry preflight under registry/store locking proves exact equality and
   returns `Unchanged` without a lifecycle mutation;
2. if change is required, acquire the lifecycle mutation guard and call a
   compare-and-apply operation bound to the preflight store revision/hash;
3. if the store changed between preflight and apply, release authority and
   retry from preflight rather than overwriting concurrent state;
4. retry at most three compare conflicts, then return a typed
   `CheckoutRegistryChanged`/conflict result to the caller. There is no
   unbounded preflight loop.

The registry mutation persists once and updates the in-memory cache once.
`register_dark_knowledge_checkout` starts/refreshes its watcher after either
successful outcome, but exact repeated registration cannot block on unrelated
active checkout work.

Lock order is lifecycle authority before the registry write lock during the
actual changed commit. Preflight never holds the registry lock while waiting
for lifecycle authority.

### 3.7 Reconciler events retain origin and latest authority

`ReconcileEvent` gains a closed origin:

- assignment/config reload;
- catalog commit (all post-commit observer notifications, including attach,
  detach, scope migration, promotion, alias, and other admin commits);
- transient deadline;
- selector-retirement completion;
- startup recovery;
- activation completion.

Coalescing is project-keyed. The pending value retains an origin set, the latest
authoritative scope/revision input, and the desired transition kind. Merging a
deferred event into a newer pending event cannot replace the newer scope or
lose a config-reload origin. The reducer always re-reads desired assignment,
effective source, activation identity, and manifest authority under the
project transition guard before choosing an action.

`ManualRetryRequired` may reattempt only when the event set contains an
assignment/config-reload origin and the current reduction table still calls
for cutback. A selector-retirement, transient-deadline, catalog-commit, startup,
or activation-completion event cannot release it. `Terminal` never reattempts
automatically; normal reducer cells may still cancel it when collected
authority is restored.

`CatalogCommit` is intentionally unified because the current post-commit event
provides changed project ids, not a trustworthy mutation subtype. It always
forces an authority re-read but never gains the privilege of config reload.
This avoids inferring attach versus scope/admin provenance from the resulting
catalog snapshot. The repair adds the previously dead config-release reducer
cell and tests both sides: config reload may release `ManualRetryRequired`,
while every catalog-commit and readiness origin remains `NoOp` for that state.

`ActivationCompletion` is emitted exactly once by the activation/cutback worker
after its durable publication or typed attempt outcome and `end_activation`.
Because the worker still owns the project transition guard, the event coalesces
into the deferred set and feeds one post-transition reducer pass after release;
that pass drives the existing `collected/collected/non-None -> CancelCutback`
cell and crash-window cleanup promptly. It has no manual-retry release privilege
and a steady-state `NoOp` emits no further completion event.

### 3.8 One guarded compare-and-apply owns cutback persistence

The cutback worker receives the reconciler's project guard and captures an
`ActivationFence` before slow work:

- project id;
- published scope;
- generation id;
- selector;
- snapshot id;
- previous typed cutback state;
- the relevant catalog/auth revision.

After slow staging and before every durable state commit, it re-reads authority.
The store exposes one compare-and-apply method that, under its mutation lock,
verifies the fence and applies exactly one outcome: clear, structural,
transient, manual, or terminal. A stale fence returns a typed conflict; the
worker records no state and enqueues a fresh reducer event.

There is no public or helper redrive that loads a cutback state, modifies it,
and writes it outside the project transition guard. Readiness callbacks only
enqueue events. Attempt increments and transient-to-manual promotion happen in
the compare-and-apply method, not in caller-side separated reads and writes.

The live catalog staging path never calls the legacy
`mark_cutback_pending_mixed` mirror writer. In catalog mode
`cutback_pending` remains strictly derived from `cutback`; the migration-only
`cutback: None, cutback_pending: true` shape is never emitted by live code.

### 3.9 Error classification is typed first and conservative by default

Introduce typed staging errors for the locally owned conditions that matter:

- selector retirement queued;
- vector store warming;
- writer pass contention;
- scope/coherence validation refusal;
- security refusal.

Classification walks the error chain for those concrete types. I/O errors are
transient `IoPressure`. Unknown errors default to transient `IndexCommit` and
log the bounded full chain as a classifier gap.

Free-form substring matching for `security`, path names, or general wrapper
messages is forbidden. Exact legacy code markers may be temporarily supported
only at the boundary that emits them, with anchored equality/prefix parsing and
a test proving incidental text does not match.

Selector-retirement-queued is a readiness deferral, not a failed staging
attempt. The worker does not mutate `CutbackStateV2` or its attempt count for
that outcome. The durable retirement row, current activation, and current auth
table already preserve the work across restart; a bounded health record reports
the wait. Retirement completion enqueues a reconciler event. Startup recovery
also enqueues the mismatch. There is no daemon-local 900-second tally and no
new cutback enum variant; the Phase 4 closed persisted state remains unchanged.

Vector warming and actual writer contention are transient failures and use the
ordinary persisted retry ladder. There is no parallel readiness retry budget.

### 3.10 Selector retirement is durable, deduplicated, and event-driven

The durable retirement row remains keyed by selector, but enqueue semantics
become collision-safe:

- absent row: create it atomically;
- byte-identical row: idempotent success;
- different snapshot/generation identity for the same selector: typed refusal,
  never overwrite.

One daemon-lifetime retirement coordinator owns live retirement attempts. It
runs bounded work on the blocking pool; no ordinary, collision-exact, or
selectorless collision row owns an unbounded sleeping thread. Its closed work
enum is:

- `Ordinary(RetirementRecord)`, keyed by selector;
- `CollisionExact { record, project_id, generation_id }`, keyed by selector and
  completed through `repair_and_complete_collision_retirement`;
- `CollisionSelectorless(CollisionRetirementWorkV1)`, keyed by project and
  generation and completed through the same collision repair without an index
  selector mutation.

Startup first reconciles and loads collision lifecycle/work records, builds the
exact collision identities, then loads the ordinary retirement queue. A queue
row that matches collision work is classified as `CollisionExact`, never as
ordinary. A conflicting queue/collision identity fails closed with durable
health. Remaining queue rows are ordinary. Selectorless collision work joins
the same coordinator under its generation key. No completion kind is guessed
from the selector-keyed row and no on-disk schema change is required.

Before mutation the coordinator re-reads the manifest under the manifest
coordinator:

- if the selector or exact snapshot is active, leave the row durable, persist
  bounded `retirement_deferred_active` health, and park it;
- manifest publication that removes that authority re-enqueues the selector;
- a bounded periodic scan is only a lost-wakeup backstop, not the primary
  progression mechanism;
- if inactive, execute one selector-retirement attempt;
- retryable failures use the coordinator's bounded retry schedule and retain
  the row; non-retryable failures retain the row and durable health;
- completion re-checks selector and snapshot inactivity before deleting files
  or completing the exact row.

Successful completion clears retirement health and enqueues
`selector-retirement completion` for the row's project. It does not directly
edit cutback state. `complete_retirement` continues to compare the exact queued
record before removal and preserves retained-generation ownership. Collision
completion advances `CollisionRetirementLifecycleV1` through the existing
repair-and-complete path before emitting the same project wake. Selectorless
completion also emits the wake after its lifecycle row is durably completed.

### 3.11 Bridge parity remains blocking

Expected bridge-observable behavior is byte-for-byte unchanged. The lifecycle
gate, publication fence, checkout registration, and vector store are shared
lanes, so the canonical bridge-parity harness remains a blocking gate through
every milestone. `bridge_parity_holds_against_canonical_fixtures` must continue
to pass, including its dark-knowledge registration path. Any red fixture row is
an implementation defect; fixture regeneration is not an accepted repair.

## 4. Milestones

### P6R-0 — Reviewed plan and status landing

- Repair every frozen Kimi plan-review finding and resume the same session to
  exact `PASS`.
- Amend the governing and Phase 6 status blocks to record that the operational
  cut completed, production is stopped after the incident, and this repair
  phase is open.
- Record any material plan decisions required by the ledger.
- Commit the reviewed plan/status/ledger set as a clean plan-only milestone
  before source implementation begins.

Exit: exact Kimi plan `PASS`; no open design or policy question; the plan-only
commit contains no source change and preserves unrelated operator-owned files.

### P6R-A — Regression harness and typed contracts

- Add cheap-vs-diagnostic vector metric types and compile-time call-site
  separation.
- Add batch deletion result/error types and typed staging/retirement errors.
- Add `RegistrationOutcome`, registration preflight token, `ActivationFence`,
  reconciler origins, and collision-safe retirement enqueue result.
- Land failing regression tests for every incident claim before behavior edits.

Exit: all new contracts compile in isolation; tests demonstrate the baseline
hot-path metrics call, lifecycle self-refusal, stale cutback overwrite, and
retirement lost wakeup. Canonical bridge parity remains green.

### P6R-B — Vector hot-path and batch durability repair

- Move ordinary metrics to slab truth and isolate graph diagnostics.
- Add/rebuild the active-id ordinal map.
- Implement WAL/slab/HNSW batch deletion and one checkpoint per chunk.
- Convert selector retirement, embed-queue deletion, and history-GC tombstone
  sink to the batch vector API with exact accounting.
- Convert workflow compaction, attention, embed status, doctor, and hybrid
  status consumers to the explicit diagnostic contract without weakening the
  connectivity gate.
- Keep public compatibility wrappers only where a non-retirement caller still
  needs them; wrappers delegate to a one-id batch and do not restore hot scans.

Exit: a synthetic large partition proves delete cost does not scale with total
partition rows; checkpoint/meta and compactor selection do not call diagnostic
metrics; the workflow status operation preserves the greater-than-5-percent
compact and greater-than-2-percent notify signals; breached low-tombstone graphs
remain connectivity-driven compaction candidates; only unavailable diagnostics
refuse that route; before/after rebuild connectivity is retained; crash/reopen
after each injected batch failure preserves every successfully WAL-appended
tombstone.

### P6R-C — Checkout lifecycle fairness and idempotent registration

- Implement pending-exclusive admission and bounded wait.
- Make write-backed publication fences reuse their lease pin.
- Add revision-bound registry preflight/apply and wire dark-knowledge
  registration through it.
- Add and validate the 500ms lifecycle writer-wait configuration and thread it
  into every broker constructor.
- Audit every lifecycle mutation and publication call site for intent and lock
  order.

Exit: deterministic concurrency tests prove writer progress, write publication
completion during pending exclusivity, read publication refusal after pending,
write-guard pin retention after the source lease is dropped, empty-set error
precedence, timeout recovery, bounded registry conflict, and zero exclusive
acquisition for exact registration. Canonical bridge parity remains green.

### P6R-D — Cutback serialization and classification

- Add origin-preserving project event coalescing.
- Add activation-fenced compare-and-apply for all typed cutback outcomes.
- Remove live catalog mirror-only pending writes and all out-of-guard redrives.
- Replace substring terminal classification with typed errors and exact legacy
  markers.
- Implement the Phase 4 config-reload release for `ManualRetryRequired` and
  preserve manual/terminal policy under every non-config readiness or catalog
  event.

Exit: schedule/config/retirement races cannot overwrite a newer activation or
attempt; incidental `security` text is transient; only config reload releases
manual retry; terminal never auto-retries; catalog live writers never persist a
migration-only activation shape.

### P6R-E — Retirement coordinator and readiness wakeups

- Replace per-row sleeping retirement threads with the coordinator.
- Make enqueue collision-safe and active rows parked/observable.
- Wake parked work from manifest authority change, with bounded scan backstop.
- Route ordinary, collision-exact, and selectorless collision work through the
  coordinator with their exact completion semantics.
- Wake catalog reconciliation after every exact retirement completion.
- Prove restart recovery and stale-row behavior with real store/manifest files.

Exit: an active selector row survives without spinning, is retired exactly once
after authority changes, unblocks same-selector staging, and resumes the correct
project without bypassing manual/terminal policy. Collision lifecycle entries
advance to completed and no retirement lane owns a sleeping per-row thread.

### P6R-F — Integrated proof and deployable artifact

- Run the focused crate tests after each milestone.
- Run the pinned formatter check, workspace clippy/lints, and
  `cargo nextest run --workspace --profile full` through the project cluster
  gate.
- Run the frozen Kimi code reviewer and repair/resume the same session to exact
  `PASS`.
- Build the macOS arm64 daemon locally only after all review/test gates pass.
- Exercise an isolated copied state root: startup, status, one synthetic
  selector retirement, cutback re-entry, shutdown, and reopen.

Exit: the candidate binary and evidence bundle are ready, but production is
still offline pending the rollout approval in section 7.

## 5. Verification matrix

| Surface | Required proof |
|---|---|
| Vector metrics | Cheap counts equal slab truth before/after replay; explicit diagnostics preserve connectivity values; mutation/checkpoint call graph contains no diagnostic invocation. |
| Connectivity consumers | Workflow status preserves max route/ratio and the >5% compact / >2% notify triggers; breach selects and rebuilds even at low deleted ratio; only unavailable diagnostics refuse that route; attention alerts on breach and distinguishes unavailable from healthy; cheap status keeps `hnsw: null`; explicit status/doctor returns `hnsw: Some`; hybrid DTO compatibility and before/after rebuild stats hold. |
| Active id map | push replacement, delete, batch delete, snapshot load, WAL replay, compaction rebuild, and duplicate-active refusal. |
| Batch durability | absent ids, duplicate input, multi-route chunks, exact WAL amplification accounting, injected WAL failure, injected derived-file failure, reopen/replay, partial-result accounting. |
| Selector retirement | tens of thousands of ids retire with bounded lock holds and checkpoints; repeated call is idempotent; no O(ids × partition-size) scan. |
| Lifecycle state | pending writer fairness, timeout, concurrent claimant, read-pin drain, no underflow, no leaked state. |
| Publication | read-only fence, write-backed fence retained after source lease drop, mixed leases, pending writer, failed revalidation, empty-set precedence. |
| Registration | exact unchanged, changed, three concurrent revision retries then typed conflict, corrupt-store recovery, watcher behavior, lock-order probe. |
| Event merge | pending/deferred ordering, latest scope, sticky config origin, unified catalog-commit origin, transient deadline, retirement completion, guard deferral. |
| Cutback CAS | activation replacement, state advancement, config reload race, scheduler race, successful clear, stale conflict requeue. |
| Classification | each typed class through nested anyhow context, incidental security/path text, unknown fallback, I/O chain, no readiness attempt burn. |
| Retirement coordinator | active parking, manifest wake, lost-wakeup scan, restart, retry exhaustion health, exact-row collision, collision-exact completion, selectorless completion, project wake. |
| Bridge parity | Canonical fixtures pass unchanged after each shared-lane milestone; regeneration is refused. |
| Integration | copied post-cut store starts, remains responsive during retirement, converges, restarts, and reports bounded health. |

Test fixtures use canonicalized tempdirs and never touch real HOME, configured
operator state, the production port, or the production daemon.

## 6. Rejected alternatives

- Do not merge the exploratory patchset wholesale. Its useful tests may be
  rewritten against the contracts above; its mechanisms carry no authority.
- Do not cache full HNSW metrics and invalidate them on every mutation. The hot
  path needs cheap authoritative counts, not another synchronization surface.
- Do not make missing diagnostics mean healthy connectivity; safety consumers
  explicitly obtain diagnostics or refuse/degrade visibly.
- Do not solve batch deletion only by chunking calls to the existing linear
  `HnswIndex::delete`; that remains O(ids × partition-size).
- Do not skip the durability boundary for all-absent tombstone batches.
- Do not use an unbounded lifecycle writer wait or clear a composite state word
  with `store(0)`.
- Do not let adapter snapshots decide registration idempotence.
- Do not let readiness callbacks mutate activation records.
- Do not add a fifth persisted cutback variant or a second retry budget. The
  Phase 4 state is closed and retained rollback readers must keep decoding it.
- Do not use a daemon-local elapsed-time ceiling to emulate durable readiness.
- Do not classify terminal failures from generic message substrings.
- Do not delete or overwrite a queued retirement row merely because its
  selector is currently active.
- Do not retain separate sleeping threads for collision retirement while
  claiming coordinator ownership of the ordinary lane.
- Do not restart production as a validation shortcut.

## 7. Rollout, rollback, and operator gate

Production stays unloaded throughout implementation and review.

After P6R-F, present the exact candidate commit, Kimi session/result, cluster
gate results, isolated-state smoke result, and binary hash to the operator. A
new explicit approval is required to mutate the shared launchd service. That
approval explicitly acknowledges that the repair activates the Phase 4
contract that config reload may release `ManualRetryRequired`; shipped code has
incorrectly treated that cell as `NoOp` until this repair. No other event gains
that authority.

On approval:

1. preserve the installed binary as a timestamped backup;
2. copy the candidate `blackboxd` and matching standalone `bro-harness`;
3. sign both with the operator's stable-signing helper;
4. bootstrap/kickstart `com.daystrom.blackbox`;
5. verify port 7264, bounded roster/status latency, daemon CPU, retirement queue
   progress, cutback health, and vector maintenance counters;
6. keep a short observation window before declaring recovery.

Rollback means stop the candidate and restore the prior signed binary only if
its state readers remain compatible with bytes emitted by this repair. The plan
intentionally adds no serialized vector-map field and no cutback enum variant.
If compatibility cannot be proved, leave production stopped and repair forward
on a copied state root. The known CPU-stalling prior binary is not treated as a
safe availability fallback.

Bridge retirement remains forbidden. Its existing zero-observation, cutback,
rollback, journal, GC-root, and operator-approval criteria are unchanged.

## 8. Definition of done

This repair is done only when all of the following are true:

- frozen Kimi plan review returned exact `PASS` after any revisions;
- the plan/status/ledger set landed as a clean plan-only commit before source
  implementation;
- each milestone landed as a reviewable commit from the certified baseline;
- frozen Kimi code review returned exact `PASS` after any revisions;
- pinned formatting, lints/clippy, and full workspace nextest gates passed;
- copied post-cut state smoke passed without touching production;
- no unresolved Critical, Important, or Moderate correctness finding remains;
- production restart received separate operator approval and its live smoke
  passed, or production remains deliberately offline with the repair artifact
  ready;
- no bridge-retirement step was taken.
