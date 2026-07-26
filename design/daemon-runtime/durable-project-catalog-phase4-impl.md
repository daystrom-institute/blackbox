---
title: "Durable project catalog Phase 4 implementation plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, project-identity, catalog, collector, cutback, state-transitions, retirement]
brief: "Resolve collector grants from the catalog, separate auth swap from source transitions, implement the cutback-pending persisted state machine, and validate catalog/activation/descriptor/manifest agreement at startup, so token revocation succeeds while collected results remain pending."
---

# Durable project catalog Phase 4 implementation plan

Date: 2026-07-25

Governing design:
[`durable-project-catalog-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-impl.md)
sections 9 (checkout-access authority), 12 (collector authority and cutback
state machine), 15 (Phase 4), 16 (concurrency/recovery/security), 17
(verification matrix: Git and cutback rows), 18 (repository gates).

Collector companion:
[`distributed-code-source-collector-impl.md`](../../../../../design/daemon-runtime/distributed-code-source-collector-impl.md)
sections 4, 5, 6, 8, 9.

Binding phase contracts:

- [`durable-project-catalog-phase1-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase1-impl.md)
  section 6.2 (inventory participants), 7.3 (readiness artifact).
- [`durable-project-catalog-phase2-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase2-impl.md)
  sections 4.1 through 4.3, 6.2 through 6.4, and 7.8.
- [`durable-project-catalog-phase3-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase3-impl.md)
  especially section 4 (fixed decisions), 4.7 (effective source is derived,
  single-authority), and milestones P3-A through P3-F.

Phase 3 is assumed to have landed exactly as its reviewed plan specifies
(committed at a738782f). This plan depends on P3-B item 6's bounded catalog
grant arm (`GrantScopeResolution::Catalog`,
`src/server/code_source.rs:84`) and does not re-plan it.

`DECISION_LEDGER.md` entries cited in this document, verified line-by-line
at authoring time: D-002 (line 33, "Do not activate v2 state before the
complete v2 runtime can preserve parity"), D-004 (line 89, "Split catalog
administration by proof, not by a claimed MCP identity"), D-020 (line 467,
"The offline catalog CLI has one versioned result envelope"), D-029 (line
697, "A terminal committed migration journal admits the registry-free
runtime open"), D-030 (line 730, "The catalog-mode smoke root is produced
by the facade-driving test, verified by the CLI"), D-032 (line 790, "The
version-1 any-read grant is a sanctioned bridge lane; v2 enforces recorded
capabilities"), D-033 (line 822, "Closing-review residual dispositions"),
D-034 (line 861, "The bridge identity marker is identity provenance, not a
scope variant").

## 1. Required outcome

Phase 4 makes the catalog authoritative for collector grants and source
transitions without making revocation destructive.

At the exit gate, proved against isolated migrated v2 state (D-030 facade
root) and the bridge parity harness:

1. Catalog-mode grants resolve only through a pinned `CatalogSnapshotV2`,
   with zero checkout-access leases.
2. Every catalog-mode live writer emits strict scope-bearing v2 records
   (`ActivationRecordV2`, `StoredGenerationV2`).
3. Replacing auth takes effect immediately via a single immutable-snapshot
   swap, independently of any in-flight source transition.
4. Revoking the last producer leaves collected results intact when no local
   attachment can support cutback: the token stops authorizing uploads at
   the swap, the generation stays searchable, and the structural cutback
   state persists without a retry spin.
5. A later attach or reassignment wakes the pending transition through a
   post-commit observer event without polling.
6. Restart resumes any sanctioned intermediate position exactly once:
   structural cutback re-evaluates once, transient retries schedule from
   their persisted deadline, terminal failures stay terminal, collected
   activations validate, and explicit retirement converges through a
   forward-only journal.
7. Startup validates a typed relationship chain across catalog, activation,
   generation, descriptor, and workspace entries before HTTP bind; any
   disagreement fails closed.
8. The bridge daemon at the same commit passes the full parity harness;
   this phase enumerates zero observable bridge changes.
9. A pre-refusal double-migration catalog state is recoverable offline:
   the boot diagnostic names `scope-bridge-clear` mode 2, applying it
   nulls the newest untruthful bridge record so the older truthful
   record again admits the effective generation, boot succeeds, and
   new-scope activation clears the remaining bridge through the normal
   bridge-clear transaction.

## 2. Tree survey

This survey describes committed HEAD plus the fixed Phase 3 contract. It
does not treat in-flight Phase 3 working-tree edits as landed behavior.
Line anchors in this section pin committed HEAD; mechanics sections (5
through 11) use symbol names to avoid drift.

### 2.1 Code-source runtime

`src/server/code_source.rs` owns `CodeSourceSnapshot` (line 52, fields:
`enabled`, `auth: Vec<AuthEntry>`, `store: Arc<CodeSourceStore>`),
`CodeSourceRuntime` (line 58, holding `snapshot:
parking_lot::RwLock<Arc<CodeSourceSnapshot>>`,
`activating_projects: Mutex<BTreeMap<String, bool>>`,
`checkout_access: Arc<CheckoutAccessBroker>`, and
`catalog_store: Option<Arc<ProjectCatalogStore>>`), `ProducerGrant`
(line 42, mapping `PublishedScope` to project-id string), `AuthEntry`
(line 47), `build_snapshot` (line 223), `GrantScopeResolution` (line 71,
with the Phase 3 pull-forward `Catalog { catalog }` arm at line 84),
`schedule_activation` (line 626), `schedule_cutback` (line 896),
`cutback_to_local` (line 938), `activate_desired_loop` (line 1090),
`schedule_cutback_if_owner_changed` (line 669),
`resume_pending_activations` (line 686), `spawn_retirement` (line 1763).

The broker's `CheckoutAttachmentSelector::Selected` path is the existing
in-tree pattern for ladder-first attachment selection: `build_snapshot`
uses it for bridge grants (line 340) and the Git overlay path uses it for
post-activation history reads (line 1113). It dispatches through
`CheckoutAccessBroker::acquire`, which internally calls the crate-private
`resolve_candidate` (`checkout_access_v2.rs:103`, the `Selected` ladder
per D-033.3).

Defects this phase closes:

- **G1:** `schedule_cutback` retries in a `loop` with
  `std::thread::sleep` inside `spawn_blocking`, holding a worker thread
  for the entire cutback window. It spins on `NoLocalAttachment` just as
  it spins on writer-pass contention. Additionally, `cutback_to_local`
  contains its own inner staging loop with a 900-second deadline and 1s
  sleep, and `activate_desired_loop` carries a writer-pass staging loop
  with the same shape. These inner loops park blocking-pool threads for
  up to 15 minutes per attempt.
- **G2:** The cutback loop does not distinguish structural from
  transient failures. There is no `ManualRetryRequired` or `Terminal` cap
  state.
- **G3:** `ActivationRecord.cutback_pending` (line 347, v1) is a boolean.
  Restart loses the reason and retry progress.
- **G4:** Live writers emit only v1 records. `save_activation` takes
  `&ActivationRecord`, `save_activation_locked` calls
  `validate_activation_v1`. Server-side readers (`load_activation`,
  `activation_records`) decode v1 only. The v2 records exist
  migration-only.
- **G5:** No startup gate validates the catalog/activation/generation/
  descriptor/workspace relationship chain for an active collected source.
- **G6:** The cold-open matrix covers lease-derived invalid enabled
  configurations but not the catalog-mode "configured scope has no exact
  published catalog project" row from governing section 12.1.

### 2.2 Store and codecs

`crates/bbox-code-source-store/src/lib.rs` owns `ActivationRecord`
(line 335, v1, with `cutback_pending: bool` at line 347),
`StoredGenerationV2` (line 354, with `published_scope: PublishedScope`
and `descriptor: GenerationDescriptor`),
`ActivationRecordV2` (line 538, with `project_id: ProjectId`,
`published_scope: PublishedScope`, `generation_id`, `selector`,
`snapshot_id`, `cutback_pending: bool` at line 549, `diagnostic` at line
550; populated from legacy bytes by `from_v1_for_migration` at line 570;
under `deny_unknown_fields` at line 537),
`ActivationRecordV2::validate_against_generation`
(line 610, checking `published_scope`, `generation_id`,
`document_count`, `entity_inventory_sha256` agreement),
`MixedActivationRecord` (line 494, dual-read path),
`MixedStoredGeneration` (line 419, with `CurrentV2` variant),
`save_activation` (line 3773), `save_activation_locked` (line 3778, calls
`validate_activation_v1`), `load_activation` (line 3786, v1-only decode),
`activation_records` (line 3798, v1-only decode),
`mark_cutback_pending` (line 3810), `desired_generation` (line 4226).

The store AGENTS.md invariant (`crates/bbox-code-source-store/AGENTS.md`):
every durable mutation holds the shared in-process mutation mutex and the
code-owned anchor lock at `<root>/effective-source-manifest.json.lock`,
acquired in that order. This anchor serializes code-source record writes
only; manifest publication happens outside it (no cross-store atomicity
claim). The AGENTS.md also states: "V2 activation and generation records
are migration-owned artifacts with an explicit `published_scope`. The
legacy bridge must refuse V2 records instead of rewriting them as V1."

### 2.3 Catalog, authority, and selection

`ProjectCatalogStore::transact`
(`crates/bbox-indexing/src/project_catalog_store.rs:354`) is the sole
catalog/attachment pair transaction with epoch CAS. `retire_project`
(`crates/bbox-indexing/src/project_catalog_admin.rs:1449`) inventories
and optionally executes project removal in one pair transaction. Its
execute path refuses with `error.project_catalog_admin_retire_blocked`
when any blocking class is nonzero: external reference counts (including
collected generations, accepted publication state, entity refs,
project-scoped rows), active attachments, or
`history_generation_referenced` (a LocalProject-authority history record
with `Ready` materialization that is deletion-eligible).

### 2.4 Manifest and workspace

`WorkspaceIndexEntry` (`crates/bbox-edge-sidecar/src/manifest.rs:175`)
fields: `manifest`, `active_snapshot`, `dirty_overlay`,
`repo_materialization`, `code_source_selector: Option<String>` (line
184), `code_source_generation: Option<String>` (line 186). These entries
carry no scope field.

### 2.5 Startup and reload wiring

`src/server/run.rs` binds the TCP listener at line 37, then calls
`start_background_tasks` at line 39, which calls
`resume_pending_activations` (`src/server/background.rs:15`). Recovery
runs after bind today. `src/server/shutdown.rs` owns the SIGHUP reload
path (line 33, `apply_source_transitions` at line 51).

### 2.6 Retirement precedent

`spawn_retirement` (`src/server/code_source.rs:1763`) uses
`SELECTOR_RETIREMENT_RETRY_LIMIT = 8` (line 1648) and
`SELECTOR_RETIREMENT_REDRIVE_DELAY = 60s` (line 1649) for selector
retirement retry. This is selector retirement (retiring old collected
selectors), not project retirement.

## 3. Non-goals and phase boundary

Explicit deferrals, each with rationale:

- **Publisher binding, accepted-publication views, and knowledge/gap
  overlay degradation after publisher detach.** Deferred to Phase 5
  (governing section 15). Phase 4 touches the effective-source state
  machine only.
- **Blame, render, file-provider, artifact-watcher, and provenance-note
  adapter conversion.** Deferred to Phase 5.
- **Removing the `ProjectRecord` compatibility view.** Deferred to Phase 6.
- **Git overlay cutback behavior.** Phase 3 P3-F delivers the Git overlay
  as a post-activation best-effort step. Phase 4 does not change overlay
  semantics during cutback.
- **Producer credential rotation without a config reload.** The token-file
  contract remains: tokens are read at reload/startup.
- **Applying v2 bytes to configured operator state** (D-002). Catalog
  behavior is exercised on isolated migrated rehearsal roots only.
- **A new persisted effective-source store.** Phase 3 section 4.7 forbids
  it. The cutback state persists on the existing activation record.
- **Deleting v1 compatibility lanes** or eight-hex bridge compat. Phase 6.
- **Changing collector HTTP routes, bearer transport, upload pagination,
  generation-id calculation, manifest layout, or blob layout.**
- **Changing the immutable blob, manifest, or generation store formats.**
- **Touching vector storage layout, routes, or the embedding envelope.**
- **A dedicated admin retry tool for `ManualRetryRequired` and
  `Terminal`.** Phase 4 names config-event re-evaluation on reload as the
  retry surface (section 4.1, section 9.3). A CLI subcommand or admin
  tool for targeted retry without a full config reload is deferred to
  Phase 5's adapter/tool work. Rationale: the config reload path is
  sufficient and reviewable; a dedicated tool adds a new privilege
  surface that belongs with Phase 5's admin capabilities.

## 4. Fixed decisions

These are plan-level decisions the implementer does not relitigate. Where a
decision reconciles the governing document with shipped reality, the
governing document receives the surgical amendment noted here in the same
commit as the implementing milestone. New material choices are recorded in
`DECISION_LEDGER.md` (D-035 onward).

### 4.1 Cutback state lives on the activation record (R1)

Typed cutback state is an optional field on `ActivationRecordV2`, not a
new persisted effective-source store. Phase 3 section 4.7 commits: "No new
persisted effective-source store." Desired and effective source remain
derived (single-authority chain from auth table plus catalog snapshot plus
manifest plus activation record at planning time).

The closed enum is complete at substrate time and is the single reducer
input alongside desired assignment and effective activation. There is no
unclassified placeholder variant. The variants:

```text
CutbackStateV2 =
    Structural { reason: NoLocalAttachment | AmbiguousAttachment | ScopeMismatch }
  | Transient { attempt: u32, error_class: CutbackErrorClass, deadline_unix_secs: u64 }
  | ManualRetryRequired { error_class: CutbackErrorClass, attempt: u32 }
  | Terminal { error_class: CutbackErrorClass }

CutbackErrorClass = WriterContention | IoPressure | Deadline | IndexCommit
                       | ValidationFailure | SecurityFailure
```

- **Structural** reasons persist without polling (governing section
  12.2). An attachment event or config reload re-evaluates the project
  through the reducer (sections 8.1, 9.3, 9.6); a structural cause
  corrected by config change is retried without restart.
- **Transient** state persists attempt count, closed error class, and
  deadline. After the configured cap, state becomes
  `ManualRetryRequired`.
- **ManualRetryRequired** is released only by a config reload: the reload
  re-evaluates the project through the reducer (section 9.3 config-event
  re-entry cell), re-attempting the cutback if the attachment landscape
  changed. The reconciler never auto-retries it. A dedicated admin retry
  tool is deferred to Phase 5 (section 3).
- **Terminal** persists when a validation or security failure is
  unrecoverable (governing section 12.2: "Validation/security failures are
  terminal and never retry automatically"). The collected generation
  stays active and authoritative, the `Terminal` state is the GC root, and
  no automatic retry ever fires. Like `ManualRetryRequired`, a config
  reload re-evaluates through the reducer's config-event re-entry cell;
  re-assigning the producer cancels the cutback and re-activates
  collected.

Rejected alternative: a new `EffectiveSourceState` record. Phase 3 section
4.7 forbids it; a second record creates a drift surface and needs separate
GC enumeration, migration handling, and startup synchronization. Rejected
alternative two: a separate durable health record the reducer must consult.
The governing `Unavailable { reason }` wording supports either, but a
single record with the closed enum keeps GC-root semantics on one record
and the reducer's input set closed.

### 4.2 Auth swap is a single immutable-snapshot atomic swap (R2)

`reload` validates the complete candidate auth table off-lock (tokens,
producer/scope uniqueness, catalog references, limits) and atomically
swaps the single immutable `Arc<CodeSourceSnapshot>` on success; failure
retains the prior table (governing section 12.1). With cutback state on
disk (R1) and transitions enqueued to the reconciler (R4) rather than
spawned inline, the single immutable snapshot gives contention-free
revocation-first semantics. Token revocation commits at the swap; the
retained collected generation stays searchable until cutback completes or
explicit retirement discharges it.

Rejected alternative: two independent `RwLock`s. Unnecessary: the
snapshot is already an immutable `Arc` and cutback state is on disk.
Rejected alternative two: a snapshot type split. Unnecessary mechanism for
the same outcome.

### 4.3 One bounded scheduler with project-id jitter (R3)

One bounded scheduler thread (not per project) computes the minimum
`deadline_unix_secs` across all `Transient` cutback states, sleeps
until then, and re-attempts each due project exactly once. Capped
exponential backoff with deterministic project-id jitter: base 1s, factor
2, cap configurable, jitter derived from a stable hash of `ProjectId`
(0 to 25 percent of the current delay). After the cap:
`ManualRetryRequired`. Structural and Terminal reasons never enter the
scheduler.

Config bounds added to `CodeCollectionConfig`
(`crates/bbox-config/src/config.rs:482`):
`cutback_retry_base_secs` (default 1), `cutback_retry_max_secs`
(default 60), `cutback_max_attempts` (default 8, matching the selector
retirement budget). All non-zero-validated.

### 4.4 Single project-keyed reconciler, staged adoption, shared transition guard (R4)

One project-keyed owner handles activation and cutback transitions. It has
a bounded event channel; events coalesce by project id. It re-reads
authority before every commit: if revision, desired assignment, or
effective activation changed, it abandons stale work and requeues.

Adoption is staged. P4-D introduces the reconciler skeleton (event channel
plus per-project transition guard) and routes auth-swap transition
notifications through it. P4-E fills in the bounded scheduler, the
cutback attempt logic, and the post-commit observer. P4-G's exit proof
verifies the reconciler is the sole transition owner and the legacy spawn
paths are removed.

During the staged-adoption window (P4-D through P4-G), one shared
per-project transition guard covers both the reconciler and the legacy
spawn path. This guard extends the existing
`begin_activation`/`end_activation` reentrancy guard
(`CodeSourceRuntime::activating_projects`) to a mutex-per-project shape:
whoever acquires the project's transition lock owns its transitions until
release. A concurrent trigger from the other owner finds the lock held and
either coalesces (event already queued) or defers (lock released then
re-acquired). A concurrent-trigger test asserts exactly one staging pass
per project per trigger batch.

### 4.5 Post-commit observer is the event path (R5)

The single catalog event mechanism is a post-commit observer in
`ProjectCatalogStore::transact`: after durable pair publication and lock
release, emit one revision event carrying changed project ids and the
committed epoch. Consumers re-read current state. Delivery failure marks
health and triggers one bounded rescan; it never rolls back an
already-committed catalog transaction.

Rejected alternative: admin-function-level hooks or MCP-handler
broadcasts. They miss direct store callers.

### 4.6 Attachment selection through the broker Selected path, then validate (R6)

Cutback attachment selection routes through the broker's
`CheckoutAccessBroker::acquire` with
`CheckoutAttachmentSelector::Selected`, exactly like the existing bridge
grant resolution (`build_snapshot`) and Git overlay paths. The broker
internally calls the crate-private `resolve_candidate` (`Selected` ladder
per D-033.3). The cutback validates scope and local-source capability on
the selected candidate after the broker resolves it. A scope or capability
mismatch is a typed `CutbackStateV2::Structural` reason
(`ScopeMismatch`), never a silent fall-through to another attachment.
Directly calling `resolve_candidate` is impossible: it is crate-private.
The broker path keeps capability and scope revalidation in one place.

Rejected alternative: a visibility change to `pub(crate)` or `pub`. It
splits the selection logic across two call sites.

### 4.7 Startup validation is a typed relationship chain (R7)

The pre-bind validation is a chain of typed relationships, not a four-way
scope equality. For every active collected activation:

1. The catalog project exists and bears the activation's
   `published_scope`, or (sole sanctioned exception per governing
   section 7.2) the catalog scope disagrees because a
   `ScopeMigrationRecord` with a non-null `code_bridge_generation`
   (`crates/bbox-corpus-core/src/project_catalog.rs:469`) names the
   activation's generation and its `old_scope` equals the activation's
   scope. This is the open-bridge predicate defined in section 9.3;
   it is the only allowed catalog/activation scope
   disagreement at startup.
2. The activation validates against its `StoredGenerationV2` via
   `validate_against_generation`.
3. The stored generation validates descriptor scope and generation
   identity.
4. The descriptor validates the immutable manifest digest and entries.
5. The `WorkspaceIndexEntry` for the project key agrees: selector,
   generation, snapshot, manifest path.
6. The `CutbackStateV2` on the activation record is internally consistent
   (the coherence clause of section 4.10 holds; `Terminal` and
   `ManualRetryRequired` are accepted as valid persisted states).

The manifest and workspace entries carry no scope field. Any failure is
fail-closed before HTTP bind with a typed error code. A fresh store with no
collected state opens clean.

### 4.8 Retirement: offline forward-only journal with correct discharge ordering (R8)

Explicit retirement is Phase 4 scope: the governing section 15 exit gate
names "explicit retirement converge exactly once." The design is a
forward-only idempotent journal that runs **offline** under the exclusive
lifetime lock, CLI-only, with the daemon stopped. This is consistent with
the D-020 CLI envelope posture and D-004 proof-based administration. The
discharge workers are library-level primitives shared with the daemon paths
where practical; they are never live daemon spawns. Being offline
dissolves the dual-owner arbitration concern (finding 7): with the daemon
stopped, the reconciler does not coexist with the journal.

The journal discharges every `retire_project` blocking class to zero
before the catalog pair removal, which is the FINAL authority cut. The
ordering is:

1. Quiesce and verify source authority (no producer grants, no
   assignments).
2. Discharge collected generations: retire collected selectors through a
   library-level selector-retirement primitive (single-attempt, no retry
   loop), delete source records, clear entity references.
3. Clear accepted publication state and project-scoped rows.
4. Detach active attachments.
5. `retire_project(execute: true)` succeeds with zero blocking classes.
   This is the FINAL authority cut: after it, the project no longer exists
   in the catalog pair.
6. Sweep materialization: delete blobs only when P3-F reference accounting
   reaches zero.
7. Archive the completed journal.

A project whose repo history record is LocalProject-authority with `Ready`
materialization gets a typed permanent refusal
(`error.project_catalog_admin_retire_history_ready`). Rationale: the
`Ready` materialization is a deliberate operator action representing
durable repo state; retirement must not silently destroy it. The operator
must dematerialize or rehome the history record through existing history
machinery before retiring the project. The preflight detects this
condition and prints the refusal before creating the journal.

Rejected alternative: deferring retirement to Phase 5/6. The exit gate
requires it. Rejected alternative two: a live-daemon journal. It would
race the reconciler and require arbitration. Rejected alternative three:
amending `retire_project` with a journal-aware mode. Unnecessary: the
correct ordering discharges all blocking classes before the call.

### 4.9 RuntimeRecordMode is a store invariant

`RuntimeRecordMode::{BridgeV1, CatalogV2}` is store-crate data set at
open time. Catalog APIs accept only strict v2 protected records; bridge
wrappers retain v1 signatures and bytes. The store crate cannot dispatch
on `ProjectAuthority` (a server-crate type); the mode reaches the store
as `RuntimeRecordMode`. This makes the store AGENTS.md invariant
("bridge must refuse V2 records") structurally enforced.

### 4.10 Catalog emits only v2; bridge emits only v1; cutback_pending is a derived mirror

In catalog mode every live activation and generation writer emits the v2
record. The v1 record survives only on the bridge arm. Dual-write is
forbidden: a v2 activation record found by the bridge read path fails
closed.

The existing `ActivationRecordV2.cutback_pending: bool` (line 549) is
kept as a derived compatibility mirror. The typed `cutback` field is the
sole authority. The coherence clause:

- `cutback_pending == true` when `cutback` is `Some` and not `Terminal`.
- `cutback_pending == false` when `cutback` is `None` or
  `Some(Terminal(_))`.

The coherence clause is layered because `validate()` is pure and
load-time and cannot distinguish pre-classification migration state
from a record that survived classification:
- Store-level `ActivationRecordV2::validate` admits the legacy-migration
  shape (`cutback: None`, `cutback_pending: true`) without error.
- The startup relationship chain (section 10.2 step 6) is the sole
  refuser: after once-only classification (section 10.1 step 5), any
  record still carrying (`None`, `true`) that is not bridge-exempt
  (section 12.8) fails closed with
  `error.code_source_cutback_coherence`.

Live writers never emit the (`None`, `true`) shape. Once-only
classification at startup (section 10.1 step 5) runs before the
relationship chain, so the chain's coherence check never sees an
unclassified record. This classification happens exactly once at first
catalog-mode startup: the reconciler re-evaluates the project against
the current attachment epoch and reaches one of three outcomes:
bridge-exempt (the effective generation is named by an open
`code_bridge_generation`; clears the mirror and marks the project
exempt, no cutback state persisted), Structural (no valid
scope-matching attachment), or clear-and-defer (valid attachment
present; clears the mirror and defers the actual cutback to the
startup sweep). This is the only time a migrated record's bool is
interpreted independently of the typed field.

### 4.11 A second scope migration while a code bridge is open is refused

Governing section 7.2 sanctions exactly one catalog/activation scope
disagreement at startup: the open code bridge. A second scope migration
while a code bridge is open would record a second
`ScopeMigrationRecord` naming the same still-effective generation but
with a different `old_scope`, producing an untruthful record (the
generation's scope is the first migration's `old_scope`, not the
second's) and an unbootable catalog: no record admits under the
open-bridge predicate (section 9.3).

The scope-migrate surface refuses a second migration while a code
bridge is open for the project, with
`error.project_catalog_scope_migration_bridge_open` directing the
operator to clear the bridge via new-scope activation (section 12.8
step 4) before re-attempting the migration. This refusal fires on both
the attachment-proved MCP arm and the offline operator-attested CLI
arm. It is a Phase 4 amendment to the Phase 2-established scope-migrate
vocabulary: those operations gain a precondition checking for an open
`code_bridge_generation`. The implementation is owned by P4-E
alongside the bridge-clear transaction (section 9.5), which shares the
open-bridge predicate check.

A pre-existing double-migration catalog (created before this refusal
was enforced) fails closed at boot: the startup chain (section 10.2
step 1) finds no admitting record and refuses with a diagnostic naming
the recovery (`scope-bridge-clear` mode 2, section 9.5).

## 5. Milestone P4-A: substrate types, RuntimeRecordMode, cutback codec

Ownership: `crates/bbox-code-source-store/src/lib.rs`,
`crates/bbox-code-source/src/lib.rs`, `crates/bbox-config/src/config.rs`.
No daemon behavior change.

### 5.1 RuntimeRecordMode

Add `RuntimeRecordMode { BridgeV1, CatalogV2 }` to the store crate,
stored on `CodeSourceStore` and set at open time from the
`ProjectAuthority` selection (server crate passes the mode; the store
never imports `ProjectAuthority`).

### 5.2 Cutback codec

1. `CutbackReason` closed enum in `bbox-code-source` (new module
   `cutback_state.rs`): `NoLocalAttachment`,
   `AmbiguousAttachment`, `ScopeMismatch`.
2. `CutbackErrorClass` closed enum: `WriterContention`,
   `IoPressure`, `Deadline`, `IndexCommit`, `ValidationFailure`,
   `SecurityFailure`.
3. `CutbackStateV2` (the complete enum from section 4.1) with a
   `validate()` that refuses `Transient` with `attempt == 0` or a zero
   deadline, and refuses `Structural` carrying retry fields.
4. Extend `ActivationRecordV2` with
   `#[serde(default, skip_serializing_if = "Option::is_none")] cutback:
   Option<CutbackStateV2>`. `deny_unknown_fields` stays, so migration
   bytes written without the field decode to `None`.
   `ActivationRecordV2::validate` gains the `cutback.validate()` clause
   and a layered coherence clause (section 4.10): store-level
   `validate()` admits the legacy-migration shape (`cutback: None`,
   `cutback_pending: true`) without error because `validate()` is pure
   and load-time and cannot distinguish pre-classification state. The
   sole refuser is the startup relationship chain (section 10.2 step 6),
   which runs after once-only classification (section 10.1 step 5); a
   record still carrying (`None`, `true`) after classification that is
   not bridge-exempt (section 12.8) fails closed with
   `error.code_source_cutback_coherence`. The migration writer emits
   `cutback: None` when the legacy `cutback_pending` is false; when it is
   true, the migration writer emits `cutback: None` and leaves the bool
   true. Live writers never emit this shape. The v1
   `ActivationRecord.cutback_pending: bool` is unchanged.
5. `save_activation_v2(&ActivationRecordV2)` writing through
   `lock_mutation` and the anchor lock via `atomic_write_json` to the
   existing `activations/<project_id>.json` path; a mode-aware
   `load_activation_mixed` returning `MixedActivationRecord` (dispatching
   on `RuntimeRecordMode`: catalog reads v2, bridge reads v1 and refuses
   v2 bytes); a mode-aware `activation_records_mixed` enumeration. The
   existing `load_activation`, `activation_records`, and
   `mark_cutback_pending` survive as bridge-only (v1) entry points. A
   new `mark_cutback_state(project_id, CutbackStateV2)` writes the v2
   record under `RuntimeRecordMode::CatalogV2`, updating both
   `cutback` and the derived `cutback_pending` in one atomic write.

### 5.3 Retry config

Add to `CodeCollectionConfig`:
`cutback_retry_base_secs` (default 1), `cutback_retry_max_secs`
(default 60), `cutback_max_attempts` (default 8), all
non-zero-validated.

### 5.4 Verification

Codec round-trip for every `CutbackStateV2` variant including `Terminal`;
v2 decode compatibility (old bytes without `cutback` decode to `None`);
validate clause matrix (transient-with-zero-attempts refuses,
structural-with-retry refuses, coherence-clause violation refuses);
v1/v2 mixed-read coherence; config validation. Workspace nextest, clippy,
concurrency lint. Commit, push, cluster verify.

## 6. Milestone P4-B: catalog-scope grant resolution

Ownership: `src/server/code_source.rs`. Depends on Phase 3 P3-B item 6
(`GrantScopeResolution::Catalog`) and `ProjectCatalogStore::snapshot()`.

### 6.1 Mechanics

1. In catalog mode `build_snapshot` resolves each configured producer
   scope (`CodeCollectionProducerConfig.scopes`) to its catalog project by
   exact `PublishedScope` equality against the pinned
   `CatalogSnapshotV2` acquired from `ProjectCatalogStore::snapshot()`,
   acquiring zero leases. Unknown scope, duplicate scope, and
   multi-project collision fail closed.
2. The grant maps scope to the typed catalog `ProjectId`, not a path
   hash.
3. Introduce an `AuthTable` struct (separate from
   `CodeSourceSnapshot`) carrying `entries: Vec<AuthEntry>`,
   `scope_to_project: BTreeMap<PublishedScope, ProjectId>`, and
   `producer_to_scopes: BTreeMap<String, BTreeSet<PublishedScope>>`. Do
   not split locks yet (R2): the single snapshot swap remains.
4. The `GrantScopeResolution::Bridge` arm survives only on the bridge.

### 6.2 Bridge parity

In bridge mode `build_snapshot` retains its current behavior
byte-identical. The `AuthTable` struct exists but is constructed only in
catalog mode.

### 6.3 Verification

Catalog-scope resolution matrix (exact match, unknown scope, duplicate
scope, two projects sharing one scope); zero leases asserted under
`DenyCheckoutAccess`; bridge parity fixtures unchanged. Bootsmoke:
catalog-mode cold-open refuses an unresolved scope before bind. Commit,
push, cluster verify.

## 7. Milestone P4-C: strict scope-bearing v2 records and mode-aware readers

Ownership: `src/server/code_source.rs`,
`crates/bbox-code-source-store/src/lib.rs`. Depends on P4-A (codec,
mode-aware readers) and P4-B (scope source).

### 7.1 Mechanics

1. The live activation writer in `activate_desired_loop` emits
   `ActivationRecordV2` in catalog mode via `save_activation_v2`:
   `published_scope` from the P4-B grant, `project_id` typed,
   `cutback: None`, `cutback_pending: false`.
2. Before activation publication, assert the activation's
   `published_scope` equals the generation's `published_scope` (via
   `validate_against_generation`). A disagreement fails with
   `error.code_source_scope_agreement` before any selector or index
   commit.
3. The generation-state and materialization writers emit
   `StoredGenerationV2` in catalog mode. `desired_generation` resolves
   through the mixed reader.
4. `cutback_to_local` resolves identity from the catalog snapshot for
   the local-staging arm.
5. Bridge keeps the v1 records end to end; the store refuses a v2 record
   on the bridge read path.

### 7.2 Mode-aware reader conversion (finding 3)

Every server-crate activation and generation read is routed through the
mode-aware mixed reader introduced in P4-A:

- `resume_pending_activations` calls `load_activation_mixed` (not
  `load_activation`) and iterates `activation_records_mixed` (not
  `activation_records`), and calls `find_generation_mixed` (not
  `find_generation`) for each activation's generation metadata. A
  catalog-mode `None` result means "no record," not "decode failed"; the
  existing error path for "active collected source has no recovery
  record" triggers only on a genuine missing file, not on a v2 decode
  mismatch.
- `schedule_cutback_if_owner_changed` calls `load_activation_mixed`
  then `find_generation_mixed` (its `find_generation` call is the
  concrete miss today).
- `activate_desired_loop` calls `find_generation_mixed` for the
  post-save generation state check.
- A `find_generation_mixed`/`load_generation_mixed` pair over
  `MixedStoredGeneration` is added in P4-A alongside the activation
  mixed readers. Any other server call site that reads an activation or
  generation record is enumerated and converted.

The exit-gate restart matrix (section 12.4) is executed against v2
records to verify the conversion end to end.

### 7.3 Verification

v2 activation record round-trip with scope agreement against a fixture
catalog and generation; refusal matrix (activation scope differs from
generation scope, each failing before commit); bridge v1 round-trip
unchanged; all server reader call sites (`load_activation_mixed`,
`activation_records_mixed`, `find_generation_mixed`,
`load_generation_mixed`) return v2 records in catalog mode. Bootsmoke: catalog-mode collected activation writes a v2
record whose scope matches the catalog and generation, and restart
recovers it. Commit, push, cluster verify.

## 8. Milestone P4-D: reconciler skeleton and auth swap separation

Ownership: `src/server/code_source.rs`, `src/server/shutdown.rs`.
Depends on P4-B (grant table). P4-D introduces the reconciler event
channel and per-project transition guard so that auth-swap transitions are
enqueued rather than spawned inline. P4-E fills in the scheduler, cutback
attempt logic, and post-commit observer.

### 8.1 Mechanics

1. Introduce the `CutbackReconciler` struct: one project-keyed owner with
   a bounded event channel (events coalesce by project id), backed by
   one background task. The reconciler owns the per-project transition
   guard (section 4.4): a `Mutex<BTreeMap<String, TransitionGuard>>`
   where each project's guard is held for the duration of a transition
   and released on completion.
2. `reload` performs the auth swap as its only auth effect: validate the
   candidate table off-lock, swap `self.snapshot` atomically on success,
   retain the prior table on failure.
3. Source transitions are derived from the assignment diff and enqueued
   to the reconciler event channel, not spawned inline by
   `apply_source_transitions`. A removed assignment persists the
   structural cutback state (P4-E computes the exact reason); an added
   assignment marks the project for activation. On every successful
   auth-table swap, every project in any non-`None` persisted cutback
   state is also enqueued, so that a config change correcting a
   structural cause (e.g. ScopeMismatch) is re-evaluated without
   restart (governing section 12.2: "wait for an attachment/config
   event"). The per-project
   transition guard prevents double-spawn: if the reconciler has claimed
   the project, a concurrent legacy-spawn trigger finds the lock held and
   coalesces into an already-queued event.
4. The bridge arm keeps its existing immediate-spawn path so bridge
   observable behavior is byte-identical.
5. A removed assignment's collected generation is retained: the token is
   revoked by the swap, but the generation stays active and searchable
   until a matching cutback completes or explicit retirement discharges
   it.

### 8.2 Verification

Auth swap succeeds while a cutback is structural-pending (upload with the
old token is rejected, generation remains active, search unchanged);
assignment-diff produces exactly-once transitions through the reconciler
event channel; concurrent-trigger test asserts one staging pass per
project per trigger batch; bridge reload behavior unchanged. Bootsmoke:
SIGHUP reload that removes one assignment revokes its token and leaves its
generation searchable. Commit, push, cluster verify.

## 9. Milestone P4-E: durable cutback pending, bounded scheduler, and post-commit observer

Ownership: `src/server/code_source.rs`,
`crates/bbox-code-source-store/src/lib.rs`,
`crates/bbox-indexing/src/project_catalog_store.rs`. Depends on P4-A
(cutback state), P4-C (v2 activation record), P4-D (reconciler skeleton).
This is the substance core (governing section 12.2).

### 9.1 Cutback attempt: one-and-done replacing every spin

`schedule_cutback` is restructured to one attempt per invocation, then
persist the outcome and return, replacing the outer `loop` plus
`std::thread::sleep` spin (closing G1). Every inner staging loop is also
restructured: `cutback_to_local`'s 900-second inner staging loop and
`activate_desired_loop`'s writer-pass staging loop both return
transient-classified errors to the one-attempt driver instead of parking.
The one-attempt driver classifies and persists; it never sleeps. The
attempt:

a. Resolve identity from the catalog snapshot.
b. Call `CheckoutAccessBroker::acquire` with
   `CheckoutAttachmentSelector::Selected` (R6: broker Selected path).
   Validate scope and local-source capability on the selected candidate.
   No match persists `Structural(NoLocalAttachment)`. Ambiguous persists
   `Structural(AmbiguousAttachment)`. Scope disagrees on the selected
   attachment persists `Structural(ScopeMismatch)`. Return immediately
   for structural reasons.
c. Stage the local generation through `cutback_to_local`. If the staging
   call returns a transient-classified error (`WriterContention`,
   `IoPressure`, `Deadline`, `IndexCommit`), persist `Transient` with
   `attempt` incremented and `deadline` set by exponential backoff with
   project-id jitter (R3). After the configured cap:
   `ManualRetryRequired`.
d. A validation or security failure classifies as
   `Terminal(ValidationFailure)` or `Terminal(SecurityFailure)`: the
   collected generation stays active and authoritative, `Terminal` state
   is persisted (it is the GC root), and no automatic retry ever fires.
e. Success: local activation through the manifest coordinator, cutback
   state cleared, `cutback_pending` health cleared.

The named sleep-retry loops that must be eliminated from catalog-mode
paths:

- `schedule_cutback` outer loop (the spin being replaced).
- `cutback_to_local` inner staging loop (900-second deadline, 1s sleep).
- `activate_desired_loop` writer-pass staging loop (1s sleep).

Each staging call returns a typed `CutbackErrorClass` to the one-attempt
driver; the driver persists and returns. The bridge arm keeps its existing
loops so bridge observable behavior is byte-identical.

### 9.2 Bounded scheduler

One bounded scheduler thread (R3) beside `spawn_store_maintenance`
computes the minimum `deadline_unix_secs` across all `Transient` states,
sleeps until then, and re-attempts each due project exactly once.
Structural and Terminal states are never on its queue. The reconciler
signals the scheduler on every `Transient` persist through a wakeup
channel; the scheduler recomputes the minimum deadline on every wake, so a
newly persisted `Transient` with an earlier deadline is not delayed by the
previous sleep target.

### 9.3 Reconciler and complete reduction table

The reconciler (skeleton from P4-D) now owns the full reduction logic.
Its input is the tuple (desired assignment, effective activation source,
persisted `CutbackStateV2`, attachment ladder result).

Open-bridge predicate (the sole definition, referenced from sections
10.1 and 12.8): a `ScopeMigrationRecord`'s `code_bridge_generation`
(`crates/bbox-corpus-core/src/project_catalog.rs:469`; governing
section 7.2) is open for a project when it equals the project's
current effective activation generation id AND the record's
`old_scope` equals that activation's `published_scope`. A second scope
migration while a code bridge is open is refused at the scope-migrate
surface (section 4.11), so the multi-record case arises only from
pre-refusal legacy state. In that case, the newest record by
`catalog_epoch` is authority; a stale record must not admit. A catalog
where no record admits fails closed at boot (section 10.2 step 1) with
a diagnostic naming the recovery (section 9.5 stale-bridge clear).

Before consulting the table, the reducer checks the open-bridge
predicate. When it holds, the reducer clears any pre-existing
`Structural` cutback state on the project (step 1's assignment removal
may have persisted `Structural(NoLocalAttachment)` at the reload event
before step 2 created the bridge record; the bridge supersedes it),
sets health to `scope_migration_refresh_required`, and performs no
cutback attempt regardless of the desired/effective/persisted tuple.
The exemption clears when the reconciler's bridge-clear transaction
(section 9.5) nulls `code_bridge_generation` on first new-scope
activation. The complete reduction table, every cell defined:

| Desired | Effective | Persisted cutback | Ladder | Action |
|---|---|---|---|---|
| collected | collected | None | n/a | no-op |
| collected | collected | any non-None | n/a | cancel cutback: clear state, ensure collected active |
| collected | other | any | n/a | activate desired |
| local | collected | None | selected, valid | attempt cutback |
| local | collected | None | none | persist Structural(NoLocalAttachment) |
| local | collected | None | ambiguous | persist Structural(AmbiguousAttachment) |
| local | collected | None | scope-invalid | persist Structural(ScopeMismatch) |
| local | collected | Structural | selected, valid | re-attempt (attachment now available) |
| local | collected | Structural | none/ambig/invalid | no-op (still structural) |
| local | collected | Transient (due) | any | re-attempt via scheduler |
| local | collected | Transient (future) | any | no-op (not yet due) |
| local | collected | ManualRetryRequired | any | steady-state no-op (explicit retry only) |
| local | collected | Terminal | any | steady-state no-op (terminal, never auto-retry) |
| local | collected | ManualRetryRequired or Terminal | any | config-event re-entry: a config reload re-evaluates this project through the reducer, re-attempting the cutback if the attachment landscape changed |
| local | collected | Structural | any | config-event re-entry: a config reload re-evaluates the ladder; selected/valid fires the re-attempt, none/ambig/invalid stays structural |
| local | local | None | n/a | no-op |
| local | local | any non-None | n/a | clear stale state (crash between local activation publication and state clear) |
| local | Warming/Unavailable | any | n/a | valid local source present: re-stage; otherwise no-op with health record |
| retired | any | any | any | hand off to retirement (P4-G) |

Before every commit the reconciler re-pins authority. If revision, desired
assignment, or effective activation changed, it abandons stale work and
requeues. A queue overflow sets an unhealthy flag and schedules one bounded
rescan after capacity returns.

### 9.4 Post-commit observer

Add a cloneable observer handle to `ProjectCatalogStore`. On successful
`transact`, after durable pair publication and lock release, emit:
committed epoch and changed project ids. The server maps each affected id
to one reconciler event. The observer does not carry mutable records.
Delivery failure marks health and triggers one bounded rescan (R5).

### 9.5 Bridge-clear transaction on first new-scope activation

When the reconciler detects that a project's effective activation
carries the catalog's current scope (the new scope) while the project's
`ScopeMigrationRecord` still has a non-null `code_bridge_generation`
naming the superseded generation (the open-bridge predicate was true on
the prior pass), the reconciler triggers a
`ProjectCatalogStore::transact` nulling `code_bridge_generation` on the
record and clearing the project's `scope_migration_refresh_required`
health marker. This transact is the sole code-source-side path that
mutates the migration record. It fires exactly once: the first
new-scope activation that makes the open-bridge predicate (section 9.3)
false. After the transact, the superseded old-scope generation retires
through normal journals (section 12.8 step 4).

Stale-open-bridge validation and recovery: the reconciler checks every
project whose migration record carries a non-null
`code_bridge_generation` on each pass. If the named generation is no
longer the effective generation and no current-scope activation exists
to clear the bridge, the record is stale and the reconciler fails the
project's health with `error.code_source_stale_open_bridge`, requiring
operator intervention. Recovery is the offline CLI action
`blackbox project-catalog scope-bridge-clear --project <id>`, run under
the exclusive lifetime lock (consistent with the retirement journal's
offline posture, section 4.8). This is the sole operator surface for a
stale or broken bridge state; the automatic bridge-clear fires only on
new-scope activation. The CLI has two precondition-distinct modes; each
refuses the other's state:

Mode 1 (dangling-reference clear): precondition is that the named
generation is retired (not a live GC root). Null
`code_bridge_generation` on the record. This is the normal stale-bridge
recovery: a generation retired out of band while the bridge was never
cleared.

Mode 2 (double-migration truthfulness repair): precondition is the
pre-refusal double-migration state (section 4.11): exactly one older
bridge-bearing record admits the current effective generation under the
open-bridge predicate (section 9.3), and a newer bridge-bearing record
exists that does not admit (its `old_scope` disagrees with the
effective generation's scope). Null `code_bridge_generation` on the
newest bridge-bearing record, restoring the older truthful record as
the sole bridge. Boot then succeeds; the remaining truthful bridge
clears via new-scope activation (step 4 of the re-scope sequence,
section 12.8) through the automatic bridge-clear above. Mode 2 refuses
when the named generation is already retired (that is mode 1's state)
or when no older admitting record exists.

A generation named by `code_bridge_generation` is a GC root: the bridge
holds it alive until the first new-scope activation retires it or a
scope-bridge-clear mode removes the reference.

### 9.6 Event-driven resume

An attach or rebind that lands a scope-matching attachment for a project
in `Structural` state re-attempts once (the reducer table's
"local/collected/Structural/selected-valid" cell). A re-added assignment
cancels any pending cutback and retains or refreshes collected authority
(the "collected/collected/Structural" cell). A mismatched or ambiguous
attachment leaves the state pending. A config reload re-evaluates
`Structural`, `ManualRetryRequired`, and `Terminal` states through the
reducer's config-event re-entry cells (section 9.3): every successful
auth-table swap enqueues all projects in any non-`None` persisted
cutback state (section 8.1). A structural cutback whose checkout-side
cause was corrected (e.g. ScopeMismatch fixed by config change) is
re-attempted without a full restart. A bridge-exempt project's
structural re-evaluation still honors the open-bridge predicate
(section 9.3).

### 9.7 Restart re-drives

`resume_pending_activations` re-evaluates every structural cutback once
against the current attachment epoch (no spin), schedules any due
transient retry through the bounded scheduler, validates `Collected`
activations, and treats `Terminal` and `ManualRetryRequired` as no-op
persisted states. The startup reducer sweep (section 10.1 step 8) feeds
every desired/effective mismatch and every migrated-bool record to the
reducer. No state spins on restart or loses the active collected
generation.

### 9.8 Verification

`NoLocalAttachment` persists and the worker returns with the generation
retained and searchable (no held thread, closing G1);
`AmbiguousAttachment` and `ScopeMismatch` likewise; transient
writer-contention backs off to the cap then `ManualRetryRequired`;
validation/security failure persists `Terminal` and no retry ever fires;
matching reattach completes the cutback exactly once; re-add cancels it;
restart re-evaluates structural cutback once and preserves every state;
config-reload re-evaluates structural cutback: operator corrects the
checkout-side cause of ScopeMismatch and reloads, and the cutback
completes without a full restart; the complete reduction table is tested
cell by cell. Bridge-clear: a
scope-migrated project with an open `code_bridge_generation` and a
still-effective old-scope generation; activating a new-scope generation
triggers the catalog `transact` nulling `code_bridge_generation`,
clears `scope_migration_refresh_required`, and retires the old
generation; a stale open bridge (named generation retired without
new-scope activation) fails health with
`error.code_source_stale_open_bridge`. Fault injection at
every atomic-replace boundary. Bootsmoke: catalog-mode
remove-assignment-with-no-attachment returns the worker and keeps search
live; attach then completes the cutback. Commit, push, cluster verify.

## 10. Milestone P4-F: startup recovery and pre-bind agreement validation

Ownership: `src/server/open.rs`, `src/server/run.rs`,
`src/server/code_source.rs`,
`crates/bbox-code-source-store/src/lib.rs`. Depends on P4-B through P4-E.

### 10.1 Startup order

Move catalog source recovery and agreement validation into the fallible
pre-bind open path:

1. load configuration and select `ProjectAuthority`;
2. open the project/attachment pair;
3. open the code-source store in its declared runtime mode;
4. build the auth table;
5. once-only classification of migrated records with (`cutback: None`,
   `cutback_pending: true`): the reconciler re-evaluates each such
   project against the current attachment epoch. Three outcomes:
   (a) if the open-bridge predicate holds (section 9.3), the reconciler
   clears the mirror to (`None`, `false`) and marks the project
   bridge-exempt (no cutback state is persisted; any pre-existing
   `Structural` is cleared by the reducer's bridge-window action, section
   9.3; the generation stays effective);
   (b) if the project has no valid scope-matching attachment, it
   persists the typed `CutbackStateV2` as `Structural`
   (`NoLocalAttachment`, `AmbiguousAttachment`, or `ScopeMismatch`);
   (c) if the project has a valid scope-matching attachment, it clears
   the mirror to (`None`, `false`) and defers the actual cutback
   attempt to the step-8 sweep (no pre-bind staging).
   Classification performs no staging, so only `Structural` is
   reachable among the typed cutback states; `Transient` and `Terminal`
   arise only from failed staging during actual cutback attempts. In
   all cases `cutback_pending` is updated to match the typed field. This
   step completes before the relationship chain so the coherence clause
   never sees an unclassified record;
6. validate the relationship chain (section 10.2);
7. detect-and-refuse incomplete retirement journals: if a
   `ProjectRetirementJournal` is found on disk, fail closed with a typed
   diagnostic naming the CLI resume command. The daemon never executes
   journal stages (the offline lane decision, section 4.8);
8. startup reducer sweep: every project whose derived desired/effective
   sources differ (including a crash between auth swap and structural
   persist, where desired=local, effective=collected, `cutback: None` on
   a live-written record), plus every migrated record classified in step
   5 outcome (b) or (c), is queued to the reducer. Bridge-exempt
   projects (step 5 outcome (a)) are not queued: the bridge exemption
   (section 9.3) prevents the reducer from cutting back their
   generation. This is the sole startup feed for the reducer;
9. construct `CodeReadView`; then
10. return state eligible for listener bind.

Bridge startup order stays byte-compatible (classification, chain
validation, and sweep do not run).

### 10.2 Typed relationship chain

For every active catalog-mode collected activation:

1. The catalog project exists and bears the activation's
   `published_scope`, or (sole sanctioned exception per governing
   section 7.2) the catalog scope disagrees because a
   `ScopeMigrationRecord` with a non-null `code_bridge_generation`
   (`crates/bbox-corpus-core/src/project_catalog.rs:469`) names the
   activation's generation and its `old_scope` equals the activation's
   scope. This is the open-bridge predicate defined in section 9.3;
   it is the only allowed catalog/activation scope
   disagreement at startup. A pre-refusal double-migration state
   (section 4.11) where no record admits fails closed here with a
   diagnostic naming the recovery (`scope-bridge-clear` mode 2,
   section 9.5).
2. The activation validates against `StoredGenerationV2` via
   `validate_against_generation`.
3. The stored generation validates descriptor scope and generation
   identity.
4. The descriptor validates the immutable manifest digest and entries.
5. The `WorkspaceIndexEntry` agrees: project key, selector
   (`code_source_selector`), generation
   (`code_source_generation`), snapshot, manifest path.
6. The `CutbackStateV2` on the activation record is internally
   consistent (coherence clause holds; `Terminal` and
   `ManualRetryRequired` are valid persisted states). This is the sole
   refuser for the coherence clause (section 4.10): a record still
   carrying (`cutback: None`, `cutback_pending: true`) after
   classification that is not bridge-exempt (section 12.8) fails closed
   with `error.code_source_cutback_coherence`. This check runs after
   once-only classification (section 10.1 step 5), so
   pre-classification migration state cannot reach it.

The manifest and workspace entries carry no scope field; the chain walks
typed relationships, not scope equality (R7). Any failure is fail-closed
before HTTP bind. A fresh store with no collected state opens clean.

### 10.3 Cold-open matrix

Unresolved configured scope (P4-B), invalid token, duplicate producer,
conflicting assignments, and each chain failure above fail before bind.
The existing cold-open test matrix
(`cold_open_fails_closed_for_every_invalid_enabled_configuration`) is
extended with catalog-mode rows (closing G5 and G6).

### 10.4 Verification

Agreement chain matrix (each link fails closed with a typed code, the
prior generation stays authoritative, the daemon stays unready); cold-open
matrix (unresolved scope, invalid token, conflicting assignments, fresh
store clean); restart tests cover every recoverable position twice (first
restart completes or schedules, second restart performs no duplicate
write). Bootsmoke: catalog-mode boot on the migrated root opens clean; a
hand-drifted activation record refuses boot with the typed code. Commit,
push, cluster verify.

## 11. Milestone P4-G: forward-only retirement journal and exit-gate proof

Ownership: `crates/bbox-indexing/src/project_catalog_admin.rs`,
`src/bin/blackbox.rs`, `crates/bbox-code-source-store/src/lib.rs`,
`crates/bbox-code-source/src/lib.rs` (library-level retirement
primitives). Depends on P4-B through P4-F.

### 11.1 Execution lane: offline, CLI-only

The retirement journal runs offline under the exclusive lifetime lock,
CLI-only, with the daemon stopped. This is consistent with the D-020
versioned CLI envelope and D-004 proof-based administration. Because the
daemon is stopped, the reconciler does not coexist with the journal, so
finding 7's dual-owner arbitration concern does not apply. The discharge
workers are library-level primitives (not `tokio::spawn` or
`spawn_blocking`), extracted from the daemon paths where practical.

### 11.2 Preflight

Before journal creation:

- resolve one `ProjectId` from current catalog authority;
- refuse if any producer grant or assignment references the project;
- inventory all `retire_project` blocking classes
  (`external_reference_counts`, `active_attachments`,
  `history_generation_referenced`);
- detect the `Ready` materialization refusal: if the project's repo
  history is LocalProject-authority with `Ready` materialization and is
  deletion-eligible, refuse with
  `error.project_catalog_admin_retire_history_ready`. Rationale: the
  `Ready` materialization is deliberate durable repo state; retirement
  must not silently destroy it. The operator must dematerialize or rehome
  the history record first;
- compute source-owned records and blobs;
- compute P3-F shared-history reference counts;
- print the exact discharge plan for operator confirmation.

Do not infer producer assignment from migration-era source manifests. Use
the current catalog/auth assignment owner introduced by P4-B.

### 11.3 Forward journal with correct discharge ordering

Persist a `ProjectRetirementJournal` outside the catalog pair with
stages:

1. `Prepared`;
2. `SourceAuthorityQuiesced`;
3. `CollectedGenerationsDischarged`;
4. `PublicationsCleared`;
5. `AttachmentsDetached`;
6. `CatalogPairRemoved`;
7. `MaterializationSwept`;
8. `Complete`.

Protocol (discharge every blocking class to zero BEFORE pair removal):

1. Create and sync `Prepared`.
2. Revoke and verify project source authority (no producer grants, no
   assignments).
3. Advance and sync `SourceAuthorityQuiesced`.
4. Discharge collected generations: retire collected selectors through a
   library-level selector-retirement primitive (single-attempt per call,
   no retry loop), delete source-owned records, clear entity references
   and project-scoped rows. This zeroes the `collected_generations`,
   entity-ref, and project-scoped-row blocking classes. Advance and sync
   `CollectedGenerationsDischarged`.
5. Clear accepted publication state. Advance and sync
   `PublicationsCleared`.
6. Detach active attachments. Advance and sync `AttachmentsDetached`.
7. Call `retire_project(execute: true)`. At this point every blocking
   class is zero, so it succeeds. This is the FINAL authority cut: the
   project no longer exists in the catalog pair. Advance and sync
   `CatalogPairRemoved`.
8. Delete blobs only when P3-F reference accounting reaches zero. Sweep
   git materialization if applicable. Advance and sync
   `MaterializationSwept`.
9. Remove or archive the completed journal.

No catalog or auth lock is held during blob deletion.

### 11.4 Recovery

Each stage is idempotent. If authority reappears before
`CatalogPairRemoved`, recovery refuses and reports the journal. If the
project is already absent (past `CatalogPairRemoved`), recovery verifies
quiescence before sweeping.

### 11.5 Sole-ownership and loop-absence exit proof

The exit gate asserts:

- The reconciler is the sole transition owner; legacy spawn paths are
  removed from catalog-mode code.
- No `std::thread::sleep` retry loop exists in catalog-mode activation,
  cutback, or staging paths. A source-level grep for `loop` plus
  `thread::sleep` in the catalog-mode branches of `schedule_cutback`,
  `cutback_to_local`, and `activate_desired_loop` returns zero hits.
- The retirement journal's discharge workers are library-level
  primitives with no retry loops.

### 11.6 Verification

Fault every stage boundary and restart twice. Assert one project removal,
one logical source discharge, and zero deletion of shared blobs. Assert
the `Ready` materialization refusal fires correctly. Assert the
loop-absence grep. The full exit-gate proof (section 12) is the terminal
rehearsal.

## 12. Exit-gate proof

Extend the facade external-consumer acceptance test and the ignored
producer test
(`crates/bbox-indexing/tests/project_catalog_migration_facade.rs`) into
the Phase 4 acceptance block, executed in CI and live.

### 12.1 Token revocation while collected results remain pending

Configure one producer scope on a remote-only fixture project, activate a
collected generation, then reload with the assignment removed. The old
token is rejected; the generation stays active and search is unchanged;
the worker returns with `Structural(NoLocalAttachment)` persisted (no
attachment exists on the remote-only fixture). GC does not reclaim the
generation: the activation record carrying a `CutbackStateV2` (including
`Terminal`) is a GC root for its `generation_id`.

### 12.2 Reattach completes cutback exactly once

Add a scope-matching local-source attachment. The cutback completes once:
one local generation installed, collected selector retired exactly once.
Detach and re-attach: the effective source is `Local`, cutback does not
re-drive.

### 12.3 Reassign cancels cutback

Re-add the assignment. The pending cutback cancels and collected authority
refreshes; no local generation is installed and no collected generation is
retired. This includes a `Terminal` cutback: re-assigning the producer
while effective source is collected clears the `Terminal` state and
ensures collected stays active (the reduction table's
`collected/collected/any-non-None` cancellation row).

### 12.4 Restart preserves every state

For each effective state variant:

| State | Restart assertion |
|---|---|
| `Collected` (no cutback, `cutback: None`) | activation re-validates against catalog; selector confirmed active |
| `Structural(NoLocalAttachment)` | state unchanged; no retry scheduled unless attachment now matches |
| `Structural(AmbiguousAttachment)` | state unchanged; no retry |
| `Structural(ScopeMismatch)` | state unchanged; no retry |
| `Transient` (deadline elapsed) | retry scheduled immediately |
| `Transient` (deadline future) | no retry yet |
| `ManualRetryRequired` | state unchanged; no retry |
| `Terminal` | state unchanged; no retry; collected generation stays active and searchable |
| crash during `Transient` redrive window | state recovered from disk; exactly one re-attempt fires at the persisted deadline; no duplicate write |
| crash between auth swap and structural persist (live record, `cutback: None`) | startup reducer sweep (step 8) feeds mismatch to reducer; reducer classifies and persists `Structural` or attempts cutback |
| migrated `cutback_pending: true`, `cutback: None` | startup classification (step 5) converts to typed state; reducer sweep (step 8) re-evaluates |

### 12.5 Explicit retirement converges exactly once

Explicitly retire a project with an active collected generation. The
forward journal completes: every blocking class discharged to zero before
pair removal, project removed exactly once, source records deleted exactly
once, shared history blobs preserved when another project references them.
A second retire call is idempotent. A project with `Ready` materialization
is refused with the typed code.

### 12.6 v2 records and scope agreement

Every catalog-mode activation and generation record is the v2 form. The
relationship chain (section 10.2) passes for every active collected
source. A hand-drifted record fails before commit with
`error.code_source_scope_agreement`.

### 12.7 Startup agreement

A hand-drifted activation record refuses boot before bind with the typed
code; a fresh store opens clean.

### 12.8 Four-step producer re-scope restart invariants

After step 1 (remove old-scope assignment, reload): no old-scope request
authenticates; desired authority is local. The collected generation
remains effective until cutback completes. If the project has no valid
scope-matching attachment, the reducer persists
`Structural(NoLocalAttachment)` at the reload event (cleared when the
bridge opens; see below). If it has a valid attachment, the reducer may
attempt cutback immediately (see attached-path race).

After step 2 (scope migration), two paths:

Attached path (attachment-proven MCP migration): if the old-scope
generation is still effective at migration time (cutback did not
complete first), the pair transaction records `code_bridge_generation`
(`crates/bbox-corpus-core/src/project_catalog.rs:469`) naming it, the
open-bridge predicate (section 9.3) holds, and the startup chain
(section 10.2 step 1) admits the old-scope activation. The reducer
(section 9.3) treats the bridge-named generation as cutback-exempt:
even though desired=local, effective=collected, and a valid
scope-matching attachment exists, no cutback attempt fires. Any
`Structural` state persisted by step 1 is cleared by the reducer's
bridge-window action (section 9.3). Response and source metadata
retain the old published scope; health reports
`scope_migration_refresh_required`. If cutback completed before step 2
(the attached-path race), no active old-scope generation exists at
migration time, `code_bridge_generation` is not recorded, and the local
source serves until step 4 publishes a new-scope generation. Both race
outcomes are sanctioned: the bridge is recorded only when the
old-scope generation is still effective at migration time.

Operator-attested path (offline CLI under exclusive lifetime lock, zero
attachments): the pair transaction records `code_bridge_generation`
with no attachment proof. On restart before step 3, the same chain
admission and reducer exemption apply. Step 1 persisted
`Structural(NoLocalAttachment)` at the reload event; the reducer's
bridge-window action (section 9.3) clears it when the open-bridge
predicate holds. Health reports `scope_migration_refresh_required`
through the bridge window.

A second scope migration (a second step 2) while a code bridge is open
is refused with `error.project_catalog_scope_migration_bridge_open`
(section 4.11). The operator must clear the bridge via step 4
(new-scope activation) before re-attempting the migration.

After step 3 (update producer config to new scope, reload): new-scope
auth resolves only post-migration and cold-open fails closed before it.
After step 4 (publish and activate new-scope generation): the first
new-scope activation makes the open-bridge predicate false; the
reconciler's bridge-clear transaction (section 9.5) nulls
`code_bridge_generation`, clears `scope_migration_refresh_required`,
and the old-scope generation retires through normal journals.

The code and publication bridge semantics are symmetric:
`code_bridge_generation` is the code-source analogue of Phase 5's
`publication_bridge_generation`
(`design/daemon-runtime/durable-project-catalog-phase5-impl.md` sections
4.9, 7.6). Each bridge preserves old-scope truth under honest provenance
until the first new-scope advance (publication) or activation (code)
clears it. Neither immutable descriptor nor accepted snapshot is
relabeled.

### 12.9 Bridge parity

The bridge daemon at the same commit passes the full parity harness. This
phase enumerates zero bridge behavior changes.

## 13. Bridge parity contract

Every Phase 4 change is catalog-mode-scoped. Anything not listed here
remains byte-identical in bridge mode.

Allowed additions visible to bridge code: dormant `RuntimeRecordMode`
type, defaulted retry config fields, versioned doctor fields, v2 store
methods behind mode dispatch, and catalog-only error codes. None change
observable bridge behavior.

Unchanged: v1 generation, desired, and activation bytes; token/grant
semantics and collector HTTP fixtures; pagination, finalization,
generation ids, descriptors, manifests, and blobs; configured selection,
cutback, read results, and source URIs; v1 GC protection; valid reload,
startup, and bind ordering.

Proof preserves committed v1 byte fixtures, replays bridge collector and
reload tests at every milestone, byte-compares HTTP and records, and fails
any cross-mode writer use.

## 14. Concurrency, recovery, and security rules

- No lock is held across Git walking, blob reads, embedding, or index
  commit. The cutback attempt prepares its local staging off-lock and
  persists the cutback state through `lock_mutation` plus the anchor lock,
  then publishes through the manifest coordinator.
- Auth swap is a single immutable-snapshot atomic swap (R2). The
  code-source anchor lock serializes cutback-state writes with activation
  writes for the same project so they cannot interleave half-states.
- One shared per-project transition guard (section 4.4) covers both the
  reconciler and the legacy spawn path during the staged-adoption window.
  The reconciler serializes per project and revalidates after every slow
  step. Events prompt an authority re-read; they are not authority.
- The anchor lock serializes code-source record writes only; manifest
  publication happens outside it (no cross-store atomicity claim).
- Authentication happens before bounded request parsing; scope membership
  is checked before any durable upload mutation. A revoked token is
  rejected before any activation or cutback work; the retained generation
  is served from immutable blobs.
- Catalog scope authority never comes from `aka_repo_ids`, repository
  URL, computed hash, or a request body (governing section 16).
- Typed refusal vocabulary: `error.code_source_scope_agreement`
  (P4-C, P4-F), `error.code_source_record_mode` (P4-A),
  `error.code_source_cutback_state` (P4-A, P4-E),
  `error.code_source_cutback_coherence` (P4-A),
  `cutback_manual_retry_required` and `cutback_terminal` health codes
  (P4-E), `error.project_catalog_scope_migration_bridge_open` (P4-E),
  `error.code_source_stale_open_bridge` (P4-E),
  `error.project_catalog_admin_retire_history_ready` (P4-G), and
  retirement journal codes (P4-G). Every refusal preserves the last-good
  read view and the active collected generation.
- Operator acknowledgement for `ManualRetryRequired` and `Terminal` retry
  is config-event only in Phase 4: a config reload re-evaluates through
  the reducer's config-event re-entry cell. A dedicated admin retry tool
  is deferred to Phase 5 (section 3).
- GC pins generations named by an activation record carrying a
  `CutbackStateV2` (any variant including `Terminal`): the activation
  record is the GC root, and the cutback state names the retained
  generation id.
- No absolute path, token value, or host identity leaks into
  `ActivationRecordV2` or `CutbackStateV2`. Logs identify producer and
  project ids but redact token bytes and digests.

## 15. Test and validation plan

### 15.1 Fixture strategy

Start from configured roots migrated by the Phase 2 facade (D-030), then
reopen through catalog APIs. The rehearsal root is produced by
`produce_migrated_smoke_fixture_from_env_root`
(`crates/bbox-indexing/tests/project_catalog_migration_facade.rs`).
Hand-edit only one agreement edge after producing a valid root.

### 15.2 Milestone matrix

| Milestone | Unit and integration focus | Fault proof | Bootsmoke |
|---|---|---|---|
| P4-A | strict v2 codec, mode closure, GC, coherence clause | atomic record replace | none |
| P4-B | auth swap, grants, zero leases | stale assignment | none |
| P4-C | live v2 writers, scope agreement, mode-aware readers | mismatch before commit; v2 restart recovery | catalog activation |
| P4-D | reconciler skeleton, auth separation, transition guard | revocation while readable; concurrent trigger | SIGHUP revoke |
| P4-E | reducer, pending, retry, observer, loop elimination | every cutback boundary | no-attachment pending |
| P4-F | pre-bind chain and recovery | every sanctioned startup position; double-migration offline recovery drill | bind refusal and valid resume |
| P4-G | offline journal, discharge ordering, Ready refusal, loop absence | every retirement stage | full exit rehearsal |

### 15.3 Fault-injection inventory

Name stable failpoints for: auth swap, generation rename, activation
rename, cutback-state sync, catalog publication, post-commit observer
delivery, startup reconciliation enqueue, each retirement journal advance,
and the crash-during-transient-redrive window. Each failpoint test reopens
the facade root twice and asserts convergence plus absence of duplicate
durable writes.

### 15.4 Cluster gates

Each milestone runs narrow nextest, pinned format, and relevant
concurrency lint. P4-G runs workspace full nextest, clippy, concurrency
lint, and isolated bootsmokes through the cluster verifier. Tests never
mutate installed services.

## 16. Live bootsmoke protocol

After committing each milestone:

1. `cargo check --workspace --all-targets`.
2. `cargo nextest run --workspace -E 'package(bbox-code-source-store) |
   package(bbox-indexing) | package(bbox-code-source)'`.
3. For P4-C onward, run the catalog-mode bootsmoke against an isolated
   rehearsal root (D-030 facade root).
4. `cargo clippy --workspace --all-targets -- -D warnings`.
5. Push and run the full cluster verification.

## 17. Bookend protocol

### Before implementation

A Kimi plan-review session reads this document, the governing design
(sections 9, 12, 15 through 18), the companion collector design, the Phase
3 plan (section 4, milestones P3-A through P3-F),
`DECISION_LEDGER.md` (D-002, D-004, D-020, D-029, D-030, D-032, D-033,
D-034), and the current code. Correct every finding and resume the same
session until its verdict is `PASS`.

### After implementation

Format and run proportionate local and single-crate checks, commit and
push, then run the full cluster verification on the pushed ref. A separate
Kimi implementation-review session inspects the full scope. Correct
findings, push and rerun the full gate, and resume until `PASS`.

## 18. Reviewer checklist

- [ ] Every grant resolution in catalog mode uses `CatalogSnapshotV2`
      only, with zero checkout-access leases.
- [ ] Every catalog-mode live writer emits `ActivationRecordV2`; bridge
      emits v1 unchanged; the store refuses v2 on the bridge read path.
- [ ] Every server-crate activation reader (`load_activation_mixed`,
      `activation_records_mixed`, `schedule_cutback_if_owner_changed`)
      routes through the mode-aware mixed reader in catalog mode,
      including `find_generation_mixed`/`load_generation_mixed` over
      `MixedStoredGeneration`.
- [ ] `RuntimeRecordMode` is set at open time; the store dispatches
      internally without importing `ProjectAuthority`.
- [ ] `CutbackStateV2` is a closed enum with `Structural`, `Transient`,
      `ManualRetryRequired`, and `Terminal`. The complete reduction table
      covers every (desired, effective, persisted, ladder) cell.
- [ ] Auth swap is a single immutable-snapshot atomic swap. A config
      reload that changes only tokens does not touch cutback state on disk.
- [ ] Structural cutback reasons never poll. Transient backoff is capped
      with project-id jitter; after the cap, `ManualRetryRequired`.
      `Terminal` never auto-retries.
- [ ] Attachment selection routes through the broker's `Selected` path,
      then validates scope and capability. A mismatch is a typed refusal.
- [ ] Every sleep-retry loop is eliminated from catalog-mode paths:
      `schedule_cutback` outer, `cutback_to_local` inner,
      `activate_desired_loop` staging. Staging calls return
      transient-classified errors.
- [ ] One shared per-project transition guard covers both the reconciler
      and the legacy spawn path during staged adoption.
- [ ] Startup validates the typed relationship chain before HTTP bind.
      `Terminal` and `ManualRetryRequired` are valid persisted states.
      A fresh store opens clean.
- [ ] `cutback_pending: bool` on `ActivationRecordV2` is a derived mirror
      of the typed `cutback` field. The coherence clause is layered:
      store-level `validate()` admits the legacy-migration shape
      (`cutback: None`, `cutback_pending: true`); the startup
      relationship chain (step 6) is the sole refuser for a record
      surviving classification that is not bridge-exempt (section 12.8).
      Migrated records are classified once at first startup (step 5)
      before the relationship chain (step 6). The
      (`cutback: None`, `cutback_pending: true`) shape never reaches
      the chain.
- [ ] `ManualRetryRequired` and `Terminal` are released only by a config
      reload through the reducer's config-event re-entry cell. A dedicated
      admin retry tool is deferred to Phase 5.
- [ ] Startup detects incomplete retirement journals and fails closed
      with a typed diagnostic naming the CLI resume command. The daemon
      never executes journal stages.
- [ ] The startup reducer sweep (step 8) feeds every desired/effective
      mismatch and every migrated-bool record to the reducer.
- [ ] Token revocation leaves collected results searchable while cutback is
      pending. GC roots generations named by any `CutbackStateV2` variant
      including `Terminal`.
- [ ] Reattach completes cutback exactly once. Reassign cancels it.
      Restart preserves every cutback state including the crash-during-
      transient-redrive window.
- [ ] The retirement journal runs offline under the exclusive lifetime
      lock. Every blocking class is discharged to zero before catalog pair
      removal (the FINAL authority cut). `Ready` materialization is
      refused with a typed code.
- [ ] The sole-ownership exit proof asserts loop absence in catalog-mode
      activation, cutback, and staging paths.
- [ ] The four-step producer re-scope survives restart after every step.
- [ ] A second scope migration while a code bridge is open is refused
      with `error.project_catalog_scope_migration_bridge_open`
      (section 4.11) on both the attachment-proved and
      operator-attested arms.
- [ ] The open-bridge predicate (section 9.3) is the sole sanctioned
      catalog/activation scope disagreement. The bridge-clear catalog
      transaction (section 9.5) nulls `code_bridge_generation` on first
      new-scope activation and clears `scope_migration_refresh_required`.
      A stale open bridge fails health with
      `error.code_source_stale_open_bridge` and is recoverable via the
      offline `scope-bridge-clear` CLI (mode 1: dangling reference;
      mode 2: double-migration truthfulness repair).
- [ ] Every Decision Ledger citation in this document is verified against
      `DECISION_LEDGER.md`.
- [ ] No `CorpusProjectId` appears anywhere; the type is `ProjectId`.
- [ ] The bridge daemon at the same commit passes the full parity harness
      with zero enumerated changes.
