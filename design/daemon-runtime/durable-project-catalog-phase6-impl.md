---
title: "Durable project catalog Phase 6 implementation plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, project-identity, catalog, destructive-cut, backfill, rebuild, rollback-proof, closeout-window, bridge-retirement]
brief: "Implement the two new offline subcommands (durable-backfill for LegacyPathStoreKindV1 row stamping, path-free-rebuild), delete the path-derived project-id surface, and define the operational cut sequence with shared-lock preflight, exclusive-then-downgrade apply, sequential preflights, an operator-agreed closeout window, and offline rollback proof on a quiescent copy, retaining all v1 rollback assets."
---

# Durable project catalog Phase 6 implementation plan

Date: 2026-07-26

Governing design:
[`durable-project-catalog-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-impl.md)
sections 6.3 (version-1 import and rollout command), 7.2 through 7.3
(migration transaction and path-keyed durable-store backfill), 9.2
(observability and proof), 10.3 (catalog-mode startup), 11 (lock order),
12 (collector authority and cutback state machine), 13.2 (publisher
binding), 14 (remaining checkout-side adapters), 15 Phase 6 items 1
through 7, 16 (concurrency, recovery, security), 17 (verification matrix),
18 (repository gates).

Binding phase contracts:

- [`durable-project-catalog-phase1-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase1-impl.md)
  sections 6.2 (inventory participants), 7.3 (readiness artifact).
- [`durable-project-catalog-phase2-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase2-impl.md)
  sections 4.1 through 4.3, 6.2 through 6.4, 7.8, 8.2.
- [`durable-project-catalog-phase3-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase3-impl.md)
  section 4 (fixed decisions), 4.7 (effective source is derived),
  milestones P3-A through P3-F. Committed at a738782f.
- [`durable-project-catalog-phase4-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase4-impl.md)
  milestones P4-A through P4-F.

Phase 5 dependency: [`durable-project-catalog-phase5-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase5-impl.md)
has landed (833 lines). Phase 6's assumptions are consistent with its
section 16 exit gates. Phase 5's P5-H ("Exit proof and Phase 6 handoff")
hands Phase 6 a named inventory: every remaining bridge `ProjectRecord` use;
every remaining legacy source lane; legacy publisher store deletion
inventory; bridge cache deletion inventory; compatibility observation
counters; accepted generation GC roots and cleanup status; watcher legacy
carrier inventory; repository I/O legacy carrier inventory; static ownership
allowlist with deletion reasons; and exact bridge parity fixtures. P6-A's
deletion surface consumes that handoff inventory by name. P6-D's closeout
counters consume the compatibility observation counters from the same
handoff.

`DECISION_LEDGER.md` entries cited in this document, verified line-by-line
at authoring time (ledger holds 34 entries, D-001 through D-034): D-002
(line 33, "Do not activate v2 state before the complete v2 runtime can
preserve parity"), D-004 (line 89, "Split catalog administration by proof,
not by a claimed MCP identity"), D-006 (line 145, "Migration rehearsal
changes destination, not transaction semantics"), D-011 (line 263,
"Catalog origin makes migration-marker loss detectable"), D-014 (line 325,
"Bound accepted-publication generations and hash exact source bytes"),
D-019 (line 442, "Collision retirement ends in a durable terminal receipt"),
D-020 (line 467, "The offline catalog CLI has one versioned result
envelope"), D-021 (line 489, "CLI roots and resolution precedence are
explicit"), D-025 (line 591, "One facade owns migration authority end to
end"), D-026 (line 619, "Rehearsal is preflighted in place and binds exact
artifacts"), D-027 (line 647, "Full rebuild preserves history from
immutable generations"), D-028 (line 673, "Migration reports distinguish
executable plans from assessments"), D-029 (line 697, "A terminal committed
migration journal admits the registry-free runtime open"), D-030 (line 730,
"The catalog-mode smoke root is produced by the facade-driving test,
verified by the CLI"), D-032 (line 790, "The version-1 any-read grant is a
sanctioned bridge lane; v2 enforces recorded capabilities"), D-034 (line
861, "The bridge identity marker is identity provenance, not a scope
variant").

New decisions proposed in this plan are UNNUMBERED. No ledger number is
pre-assigned; concurrent implementation sessions are actively minting D-035
and beyond, and hard-coding a number here would collide.

## 1. Required outcome

Phase 6 is the destructive-cut phase. It converts the configured operator
project store from version-1 bridge compatibility to strict catalog mode
through an explicit, reviewed, lock-enforced operational sequence, then proves
the result is recoverable before live traffic resumes.

At the exit gate, proved against the configured live store after the cut:

- The daemon starts with an empty attachment store and serves all catalog-only
  collected code (governing section 15, Phase 6 item 6).
- Every remaining nonzero checkout operation matches exactly one row in the
  section 14 adapter table, with no silent path reconstruction from catalog
  data.
- The version-1 backup, compatibility reader, pre-cut bridge binary, every
  journal, receipt, history generation, quarantine asset, and migration backup
  are retained and GC-protected for rollback (governing section 15, Phase 6
  item 7).
- Offline rollback proof on a quiescent post-cut copy demonstrates that
  retained v1 state is restorable and serves the expected project set.

Phase 6 does not retire the bridge lane. Retirement is a later phase requiring
zero non-intentional checkout observations, cutback proven, rollback proven,
no prepared journals, verified GC roots, and operator approval.

**Deletion boundary against the Phase 5 handoff (plan-entry review ruling,
R1).** The handoff inventory
([`durable-project-catalog-phase6-handoff.md`](durable-project-catalog-phase6-handoff.md))
uses "the cut" for the bridge-lane DELETION campaign; this plan uses "the
cut" for the P6-F live migration of configured state. They are different
events. Phase 6 implements only this plan's section 5 deletion surface (the
path-derived project-id function, the direct `load_project_records`
consumer, and the legacy `open_or_create` lane). The handoff's section 5
steps 2 through 5 (legacy publisher lane, bridge carriers,
`ProjectRegistry`/`ProjectRecord`, compatibility projection) and its
section 3.7 parity-fixture deletion are LATER retirement-phase inventory.
Every bridge implementation, the compatibility reader, and the bridge
parity fixture survive through P6-H: the parity harness must stay blocking
through the whole phase because it is the instrument that proves the code
milestones changed nothing observable, and the retained bridge code is a
named rollback asset (FD-8, section 10.1).

## 2. Fixed decisions

This section states every binding decision the plan implements. Each is a
plan-stated decision with rationale, not an open question.

**FD-1. Two new verbs only; existing surface untouched.** The only new
`ProjectCatalogCommand` variants are `DurableBackfill(DurableBackfillArgs)`
and `PathFreeRebuild(PathFreeRebuildArgs)`, spelled as governing section 15
names the operations. The shipped `Migrate` and `Verify` variants keep their
current shape: `Migrate` has `ArgGroup("mode")` over `preflight|apply`
(`src/bin/blackbox.rs:207-212`), and `Verify` is its own variant taking
`--root` (`src/bin/blackbox.rs:252-258`). New verbs carry a
preflight/apply/verify mode triple internally. The bridge-down proof is
`verify --require-exclusive-availability`, not a new verb.

`durable-backfill` stamps and rewrites path-keyed durable-store rows across
the 14-variant `LegacyPathStoreKindV1` owner set, using the `LegacyPathBinding`
ledger from the migration inventory (governing section 7.3). Publisher
bindings, accepted publication pointers, and G1 content references are already
installed by the migration transaction itself (governing section 13.2, D-006,
D-014); durable-backfill verifies their presence but does not seed them.

**FD-2. Explicit target selection on every mutating apply and every
verify.** Apply and verify each select exactly one of
`--rehearsal-root <path>` or `--configured`. This generalizes the shipped
`--rehearsal-root` gate (`required_if_eq("apply","true")`,
`conflicts_with = "preflight"` at `src/bin/blackbox.rs:272-278`) so that
touching or attesting real configured state is always an explicit operator
opt-in. Preflight accepts an OPTIONAL `--rehearsal-root`, which runs the
D-026 isolated-bundle preflight that must precede rehearsal apply; without
it, preflight captures the real configured state through `ConfigArgs`
resolution (D-021), exactly as governing section 6.3 specifies (section
3.1 states the full per-mode rules). `ConfigArgs` precedence (`--config`,
`--state-dir`, `--projects-path`) is retained unchanged. The isolated
rehearsal is the only mechanical barrier between a test and a destructive
invocation on two new destructive commands (D-006, D-026).

**FD-3. Artifact vocabulary is report plus resolution.** All new commands use
the migrate artifact pair: `--report <path>` and `--resolution <path>`. A
deterministic operation with nothing to resolve uses the canonical empty
resolution artifact, exactly as first-preflight does today. No third artifact
noun is introduced; the epoch CAS is already subsumed by the four-hash
identity check (D-020, D-028).

**FD-4. Artifact hash graph is acyclic.** No artifact contains its own byte
hash, and no two artifacts contain each other's hashes. The report carries the
inventory hash, plan hash, catalog epoch, predicted post-images, and
transaction id. The report artifact hash and the resolution artifact hash live
in the preflight receipt, journal, apply receipt, and verification receipt,
never in the report itself.

**FD-5. Preflight runs under the shared lifetime lock; apply uses
exclusive-then-downgrade with point-in-time exclusivity.** See section 4.
Preflight acquires the shared lifetime lock and the store mutation lock
through `capture_migration_preflight_with`
(`project_catalog_store.rs:3901-3912`), matching governing section 6.3's
"Preflight takes a shared/read lock." A shared lock does not exclude the
daemon's own shared handle, which is why preflight runs while the bridge is
live and sees a consistent capture. Apply acquires exclusive to prove no
daemon is live at that instant, then downgrades to shared and opens the store.
Exclusivity is point-in-time; the stopped-service window plus the four-hash
recheck is the real exclusion for the transaction's duration.

**FD-6. Error codes are real or explicitly new, conforming to shipped families.
** See section 7.

**FD-7. The item-1 deletion surface is the path-derived project-id function.
** See section 5.

**FD-8. Bridge-mode code is retained through Phase 6.**
`IdentityOrigin::Bridge`, `ProjectAuthority::Bridge`, and the bridge record
path are NOT deleted. D-034 is conditional ("Revisit only if Phase 6 retires
the bridge lane"); it does not. The compatibility reader must survive for
rollback proof.

**FD-9. Preflights are sequential against real predecessor post-images.** See
section 6.

**FD-10. Apply never replans; staleness blocks for diagnosis.** See section 6.

**FD-11. Closeout window: operator-agreed duration, mandatory coverage set,
pass rule gate.** See section 8.

**FD-12. Rollback proof runs on a quiescent post-cut copy with narrowed
scope.** See section 9.

**FD-13. Retention and GC: named inventory, nothing retired.** See section
10. Marker-driven GC exclusion and refusal apply to `MigratedV1` origins only;
`FreshV2` stores carry no rollback assets and no migration marker (D-011).

**FD-14. Eight milestones; code-only strictly before operational.** See
section 11.

## 3. CLI surface

### 3.1. New commands

The two new verbs carry one exclusive mode group: `ArgGroup("mode")` over
`preflight|apply|verify`, required, exactly one. Verify is a MODE inside
the group, not a separate flag; a mode group that admitted `--verify`
alongside `--apply` would leave the combination's meaning undefined.

```text
blackbox project-catalog durable-backfill \
    (--preflight | --apply | --verify) \
    [--report <path> --resolution <path>]      # required for preflight and apply
    [--rehearsal-root <path> | --configured] \ # exactly one for apply and verify
    [--config <path>] [--state-dir <path>] [--projects-path <path>]

blackbox project-catalog path-free-rebuild \
    (--preflight | --apply | --verify) \
    [--report <path> --resolution <path>] \
    [--rehearsal-root <path> | --configured] \
    [--config <path>] [--state-dir <path>] [--projects-path <path>]
```

**Target rules per mode.** `--rehearsal-root` and `--configured` are
`conflicts_with` each other.

- **Apply and verify require exactly one of them.** Both modes operate on
  a concrete root; an implicit target would let artifacts or verification
  claims silently cross roots. `--configured` derives the real configured
  projects path and (for apply) requires the exclusive lifetime lock
  (section 4). `--rehearsal-root` derives participant paths from the
  isolated root, exactly as shipped `migrate --apply` does today.
- **Preflight targets the state it captures.** With `--rehearsal-root`,
  preflight runs against the operator-created isolated bundle: D-026
  requires rerunning preflight against that bundle before rehearsal
  apply, and the explicit flag names the bundle rather than smuggling it
  through config resolution. Without a target flag, preflight captures
  the real configured state through `ConfigArgs` resolution (D-021); this
  is the live-cut preflight of P6-F. `--configured` is not accepted at
  preflight (it is the default, and writing it would imply a choice that
  does not exist).

**Artifact rules per mode.**

- **Preflight** requires `--report` and `--resolution` as OUTPUT paths: it
  captures predecessor state, plans the deterministic post-image, and
  writes both artifacts. A first preflight may create the canonical empty
  resolution at the explicit path (D-026).
- **Apply** requires both as INPUT paths: it reads the exact reviewed
  files, verifies the four-hash identity against the CURRENT state of the
  selected target, and installs the post-image. The identity check is the
  cross-root guard: a report captured against one root fails the
  inventory-hash recheck against any other, so artifacts cannot authorize
  an apply on a root they never described.
- **Verify** takes neither. It loads the selected target root and runs
  fresh verification against durable state (journals, receipts, manifest,
  store), not against operator artifacts; the durable records already
  carry the artifact hashes they were applied from (FD-4).

Both new commands produce the D-020 versioned result envelope. The envelope
`command` values are snake_case, following the shipped shape:
`project_catalog_durable_backfill_preflight`,
`project_catalog_durable_backfill_apply`,
`project_catalog_durable_backfill_verify`,
`project_catalog_path_free_rebuild_preflight`,
`project_catalog_path_free_rebuild_apply`,
`project_catalog_path_free_rebuild_verify`.

### 3.2. Existing command changes

**`migrate` gains the configured-target capability for apply only.** Add
`--configured` to `MigrateArgs` as an alternative to `--rehearsal-root`. Both
carry `required_if_eq("apply","true")` and are `conflicts_with` each other,
matching the shipped `--rehearsal-root` gate. Preflight requires neither flag:
live migration preflight captures configured state through `ConfigArgs`
resolution with no target flag, exactly as governing section 6.3 specifies
("It can run while the v1 daemon remains available"). The D-026
isolated-bundle preflight that precedes a `migrate` rehearsal apply keeps
its shipped shape: `ConfigArgs` (`--projects-path`/`--state-dir`) pointed
at the bundle, since `--rehearsal-root` conflicts with `--preflight` on
the shipped `MigrateArgs` and this plan does not change the shipped
surface. When `--configured` is selected at apply, the command runs
`open_admin_store` (section 4).

**Lock discipline (preflight).** Governing section 6.3 states "preflight
takes a shared/read lock." This matches the shipped code: preflight capture
goes through `capture_migration_preflight_with`
(`project_catalog_store.rs:3901`), which acquires both
`ProjectCatalogMigrationLock::acquire_shared` (line 3907) and
`acquire_store_lock_nofollow` (line 3910) before invoking the capture
closure. A shared lifetime lock does not exclude the daemon's own shared
handle, so preflight runs while the bridge is live and sees a consistent
snapshot.

**`verify` gains the exclusive-availability proof mode.** Add
`--require-exclusive-availability` to `VerifyArgs`. When set, the command
attempts `ProjectCatalogMigrationLock::try_acquire_exclusive` against the
configured projects path. If it returns `Ok(Some(_))`, the bridge is down; the
guard is dropped and verification proceeds. If it returns `Ok(None)`, the
command exits nonzero with `error.project_catalog_cli_lock` and a message
stating the lifetime lock is shared (bridge is live). This replaces a proposed
`lock-status` verb: it reuses the existing `Verify` variant and result
envelope.

No other changes to the shipped surface. `Add`, `List`, `Get`, `Alias`,
`ScopeMigrate`, and `Retire` are untouched.

### 3.3. `durable-backfill` semantics

The durable backfill owns the governing 7.3 path-keyed durable-store row
stamping. No other phase implements it (Phase 2 8.2 deferred store-row
rewrites). The backfill iterates the `LegacyPathBinding` ledger
(`legacy_path_bindings` on `AttachmentSnapshotV1`, `project_catalog.rs:628`).
Each entry names a `historical_path`, `source_store`, `source_row_id`,
`inventory_epoch`, and typed status (`project_catalog.rs:601-609`). The
backfill stamps the corresponding durable-store row with the stable
`project_id` across the exact 14-variant `LegacyPathStoreKindV1` owner set: Knowledge,
Gap, Thread, Note, Pin, Roadmap, Packet, Task, Proposal, SlackBinding,
Whiteboard, Artifact, Provenance, TranscriptEdge
(`project_catalog_inventory.rs:629-644`).

Row classification and quarantine posture (governing section 7.3):

- **Mappable:** status is `Mapped { project_id, relationship }`
  (`project_catalog.rs:590-598`). The backfill stamps the row's `project_id`
  idempotently (a row already stamped is a no-op).
- **Ambiguous (quarantined):** status is `Quarantined {}`. Unresolved rows do
  NOT block the cut; they ride forward as counted quarantine. The operator may
  resolve them through an explicit disposition (below). The quarantine must be
  empty only before the later path-fallback removal gate (governing 7.3), not
  at the Phase 6 cut.
- **Unscoped:** status is `Unscoped {}`. These rows never belonged to an
  inventory-time registered root. They retain the raw string in a typed
  non-catalog lane, grant no attachment or corpus authority, and remain
  queryable only through legacy store semantics. The backfill counts them by
  store (`unscoped_legacy_counts`,
  `project_catalog_inventory.rs:1847`) but does not stamp them.

Resolution vocabulary: the resolution artifact carries the four-hash identity
binding plus, for each quarantined row, one explicit disposition:
map-to-project-id (converts `Quarantined` to `Mapped { project_id,
relationship }`), retain-quarantine (leaves the row in counted quarantine), or
classify-unscoped-by-appended-supersession (appends a superseding `Unscoped {}`
binding with matching `historical_path`, `source_store`, and `source_row_id`).
`LegacyPathBindingStatus` (`project_catalog.rs:590-598`) has exactly three
variants (`Mapped`, `Unscoped`, `Quarantined`). Governing 7.3 and Phase 2 8.4
make the ledger append-only, so both map-to-project-id and classify-unscoped
append a new binding at a higher inventory epoch with matching key fields
(map-to-project-id: `Mapped { project_id, relationship }`; classify-unscoped:
`Unscoped {}`) rather than mutating in place; the quarantined original is
retained as an audit record. `LegacyPathBindingId`s are unique randoms
and `validate_catalog` has no uniqueness constraint on the key triple, so this
plan defines the supersession rule: for equal (`historical_path`,
`source_store`, `source_row_id`), the binding with the highest `inventory_epoch`
(equivalently, latest appended order) supersedes. This rule drives BOTH the
backfill's classification counts AND the later fallback-removal gate's
quarantine-emptiness check. Dual-read is unaffected (only `Mapped` bindings
resolve, and neither status here is `Mapped`). This plan drops
`acknowledged-delete` entirely: there is no `Deleted` variant, inventing a
tombstone is unmandated new substrate, and deleting legacy rows during the cut
is destructive discretion Phase 6 should refuse.

The dual-read interaction (governing 7.3, `project_selector.rs:60`): during
compatibility, path-keyed rows resolve through path-fallback (ledger first,
catalog resolver second). After stamping, the catalog resolver becomes
primary. The path fallback is removed only after the mappable ledger is
complete, quarantine empty, every unmappable row classified, and checkout
observations show no compatibility reads for the required window. That removal
gate is LATER, not part of Phase 6.

**Epoch and mutation semantics.** Backfill apply mutates the catalog pair
ONLY when the resolution converts quarantined bindings to `Mapped` or appends
superseding `Unscoped` bindings. Mappable-row stamping writes to the
path-keyed durable stores (Knowledge, Thread, etc.) but does not bump the
catalog epoch. When no pair mutation occurs, the "backfill post-image epoch"
equals the predecessor epoch: the catalog pair is genuinely unchanged. The
backfill completion record is written OUTSIDE the pair as a standalone
versioned `BackfillCompletionJournalV1` (NEW type), placed beside the store
in `<state>/backfill-completion.json`, following the offline-journal precedent
of `ProjectRetirementJournal` (Phase 4 section 11). The journal file names:
the predecessor catalog epoch and snapshot hash, the stamp set (per-store
mappable/converted/unscoped counts), and the four-hash identity binding. It is
fsynced before apply returns. The rebuild preflight reads this journal as its
predecessor binding, so the four-hash chain is contiguous whether or not the
epoch advanced. When quarantine conversions land, the pair transaction bumps
the epoch and records the converted bindings; the journal file still records
the predecessor epoch and post-stamp four-hash identity.

**Torn backfill recovery.** If the pair commit lands but stamping crashes
midway, re-apply refuses on stale predecessor. Recovery: run a fresh
preflight, review, and re-apply. Stamps are idempotent (a partially stamped
set completes without duplication). If the `BackfillCompletionJournalV1` is
absent after a crash, recovery is a fresh preflight and re-apply; the journal's
predecessor epoch and four-hash identity are re-validated. Quarantine
conversions that landed in the committed pair are visible in the new
predecessor and are not re-stamped.

The backfill report carries: the predecessor catalog epoch and snapshot hash;
the complete `LegacyPathBinding` ledger with row classification counts by store
and status; planned stamp operations (per-store mappable/ambiguous/unscoped
counts); publisher/G1 verification result (every project expected to publish
has a seeded generation, any missing named); the predicted post-image catalog
epoch (equals predecessor when no pair mutation; bumped when quarantine
conversions land); and the `BackfillCompletionJournalV1` path for the rebuild
preflight's four-hash chain.

### 3.4. `path-free-rebuild` semantics

The path-free rebuild replaces the on-disk index with a path-free schema,
rematerializing every project's documents from the catalog identity and
immutable `RepoHistoryGeneration`s. Per D-027, a full replacement
rematerializes stale, compatibility, and active commit documents from
referenced immutable generations; ambiguous or unclaimed legacy commit
namespaces live in immutable `RepoHistoryQuarantineGeneration`s and remain
rebuild/GC roots until explicit resolution or acknowledged retirement. A
checkout is never the only rebuild source for retained history.

The rebuild drives the Phase 3 materializer and manifest machinery against the
applied catalog post-image. That machinery is a Phase 3 deliverable, not
current code: P3-D creates
`bbox-indexing/src/index/history_materializer.rs` and defines
`RepoHistoryRebuildManifestV1` with its prepared/committed states, recovery,
and generation-pinning (phase 3 plan section 8), so this phase depends on P3-D
having landed. The `path-free-rebuild` subcommand is a thin caller of that
creation path; it MUST NOT specify a parallel manifest writer, because a
second implementation would fork the crash and recovery semantics P3-D owns. The startup validation gate (section P6-C)
requires the manifest in the `Committed` state for migrated origins before the
daemon may bind any route.

**Equality proof mode is mandatory (D-036).** The Phase 6 offline rebuild
runs `prove_against_inventory` and requires
`HistoryProofModeV1::Equality`: the recorded capture-recipe source
fingerprint must match the fingerprint recomputed over the index the
rebuild consumes. A Drift-mode outcome proves only non-loss, which is
insufficient for a cut-authorizing rebuild; the rebuild REFUSES it with
`error.project_catalog_rebuild_proof_mode` (NEW, section 7.3). The proof
mode and both fingerprints are already recorded in the outcome and the
committed manifest (`proof_mode` on `RepoHistoryRebuildManifestV1`,
`history_generations.rs`); rebuild verify revalidates that the committed
manifest records Equality with matching fingerprints. The service is
stopped across the sequence (section 6.1), so the index cannot drift
between backfill apply and rebuild preflight; a Drift-mode outcome during
the cut therefore indicates an inconsistent capture and blocks for
diagnosis per section 6.2.

The rebuild report carries the predecessor catalog epoch, the backfill
post-image epoch, the planned history-generation materialization set with
complete-count/hash proofs, planned quarantine dispositions, the proof
mode with both fingerprints, and the predicted post-image rebuild manifest
hash. The rebuild resolution carries the four-hash identity binding.

## 4. Lock discipline

### 4.1. Preflight acquires the shared lifetime lock

Preflight capture goes through `capture_migration_preflight_with`
(`project_catalog_store.rs:3901-3912`), which acquires both
`ProjectCatalogMigrationLock::acquire_shared` (line 3907) and
`acquire_store_lock_nofollow` (line 3910) before invoking the read-only
capture closure. The callers are
`project_catalog_inventory_adapters.rs:2230` and `:2281`, and
`project_catalog_store.rs:3898`.

A shared lifetime lock does not exclude the daemon's own shared handle, which
is why preflight can run while the bridge is live and still see a consistent
capture. The new verbs follow the same pattern. This matches governing
section 6.3: "Preflight takes a shared/read lock, reads live v1 state, writes
no project state, and emits a complete machine-readable report. It can run
while the v1 daemon remains available."

### 4.2. Apply: exclusive-then-downgrade with point-in-time exclusivity

The shipped offline-admin pattern is `open_admin_store`
(`src/bin/blackbox.rs:620-645`):

1. `ProjectCatalogMigrationLock::try_acquire_exclusive(projects_path)` proves
   no daemon shares the store at that instant. This returns
   `Result<Option<Self>>`: a live bridge holding a shared guard yields
   `Ok(None)`.
2. If `Ok(None)`, the caller constructs the refusal:
   `error.project_catalog_cli_lock`.
3. If `Ok(Some(exclusive))`, atomically `downgrade_to_shared` so the strict
   open can take its own shared handle on the same lock file. The in-code
   comment states: "holding exclusive across the open would deadlock against
   it."
4. `ProjectCatalogStore::open_existing(projects_path)`, which itself calls
   `acquire_shared` at `project_catalog_store.rs:621`.

**Exclusivity duration (designed property).** Exclusivity is a point-in-time
proof at acquisition, not a held guard for the transaction's duration. After
the downgrade, both the CLI and any concurrent opener hold shared locks. This
plan adopts point-in-time exclusivity plus the fail-closed four-hash recheck
as the designed property, not a store-open variant that adopts an externally
held exclusive guard. The reasoning: the stopped-service window is the real
exclusion (the runbook stops the bridge before apply and does not restart
until apply and verify complete); the four-hash identity check at apply
(FD-10) refuses if the predecessor inventory advanced, leaving the service
stopped; mutation correctness is owned by the pair transaction's
`mutation_lock` (`project_catalog_store.rs:623`), not the lifetime lock; and a
store-open variant that skips the internal `acquire_shared` would couple the
store's locking to CLI guard ownership with no additional safety given the
stopped-service invariant. The failure mode of a concurrent opener is an
aborted apply, not corruption.

### 4.3. Rehearsal apply needs no exclusive lock

When `--rehearsal-root` is selected, the command derives an isolated layout.
No configured store is opened, so no exclusive lock is needed; the pair
transaction's own locks (D-006) provide correctness inside the isolated root.

## 5. Deletion surface

### 5.1. The three-way disambiguation

Governing section 15, Phase 6 item 1 says "Delete direct
`load_project_records` consumers and eight-hex selector assumptions." Three
distinct things in the tree use eight lowercase hex characters. Only the
first is in scope.

**In scope: project identity from a host path.** `entity_ref::project_id_for_path`
(`crates/bbox-corpus-core/src/entity_ref.rs:549`) derives project identity
from a host path via `hash_path` -> `hash_bytes` ->
`hex::encode(&digest[..4])`, delegating to `realpath_hash` (`:571`) and
`hash_path` (`:580`). In catalog mode, project identity comes from the catalog
store (`ProjectId`), never from a path hash. There are 15 occurrences across 8
files, re-derived by grep. Disposition per call site:

- **Convert to catalog resolution** (1 production site):
  `bbox-mcp-tools/src/mcp_tools/hybrid_search.rs:698` (thread path matching).
- **Retain, bridge lane** (2 production sites): `src/tools/scope.rs:292`
  (`resolve_hybrid_project_filter`, gated by `is_bridge()`, FD-8);
  `bbox-indexing/src/projects.rs:174` (`register_path_locked`, mints the v1
  registry id; v1 registry construction, unreachable in catalog mode per
  Phase 2 4.1). Test obligation (P6-A): no catalog-mode path may call
  `ProjectRegistry::register_path`.
- **Retained-compat** (1 production site): `bbox-refactor/src/lib.rs:1251`
  (chunk file lookup). This crate depends on `bbox-corpus-core`,
  `bbox-chunker`, and `bbox-lsp` only (`Cargo.toml`), with no daemon catalog
  path. Governing decision 1 keeps legacy ids byte-for-byte, so path-hash
  derivation yields the correct id for migrated projects. This does NOT hold
  for fresh catalog-mode `p_`-shaped ids (degraded lookup; harness-side
  injection removes the limitation in a later phase).
- **Retain** (11 test/definition sites): `entity_ref.rs:549` (definition),
  `entity_ref.rs:978`, `search.rs:1968` (test helper), `projects.rs:1016`,
  `:1020`, `:1102`, `:1106`, `:1121`, `snapshot.rs:946` (test helper),
  `scope.rs:597`, `src/tools/projects.rs:1422`.

**Retained: file-relative path hash.** `project_files::short_hash`
(`project_files.rs:2170`) computes an eight-hex hash from file-relative path
bytes (`rel_path_hash` at `:1753`). These are content-addressed file keys,
not project identity, and are legitimately retained.

**Out of scope: unrelated eight-hex id generators.** Eight-hex id generators
in pins, notes, threads, gaps, packets, and the edge sidecar derive entity
ids, not project identity. They are entirely out of scope.

### 5.2. `load_project_records` consumer

`load_project_records`
(`crates/bbox-corpus-core/src/project_record.rs:385`) has exactly one direct
consumer: `StaticProjectRecordsProvider::from_projects_path`
(`crates/bbox-corpus-index/src/index/mod.rs:208`), which calls
`load_project_records` at line 209. This is reached through the legacy
`open_or_create` path (`index/mod.rs:271`). The bridge identity construction
in `from_bridge_records` (`project_record.rs:341`) is RETAINED per FD-8.

### 5.3. P6-A deletion and test-conversion plan

P6-A converts the 1 catalog-resolution call site (section 5.1) and deletes
the direct `load_project_records` consumer. The bridge decode path, the
retained bridge-lane call site, and file-relative hashes survive.

The deletion of `open_or_create` (`index/mod.rs:271`) affects test and
offline callers that construct an index from a `projects.json` path without a
catalog records provider. Each must be converted to pass a
`StaticProjectRecordsProvider` constructed from catalog-bridge records, or
removed. The complete caller set, verified by grep: `index/mod.rs` (test:
1091, 1135, 1189, 1210, 1278, 1346, 1391, 1435, 1462, 1557, 1652, 1737),
`search.rs` (test: 1943, 2039), `migration_inventory.rs` (test: 927),
`edge_index.rs` (test: 899), `store_integration_tests.rs` (15, 125),
`writer_actor.rs` (test: 1555), `project_catalog_migration_facade.rs` (117),
`workspace.rs` (1306), `integration_tests.rs` (28).

## 6. Sequencing and replan prohibition

### 6.1. Sequential preflights against real predecessor post-images

The three operations have a data dependency chain:

- Migration apply changes the owner inventory and catalog epoch.
- Backfill apply stamps durable-store rows against the migrated catalog.
- Rebuild apply changes the index the v2 runtime reads.

Therefore only the MIGRATION preflight may run while the bridge is live. The
backfill preflight runs after migration apply; the rebuild preflight runs
after backfill apply. Each is reviewed and explicitly approved, with the
service stopped across the sequence.

### 6.2. Stale recapture: block for diagnosis, never loop

Governing 15 item 4 states the stale-recapture protocol. If apply reports
stale inventory after the bridge stops, leave it stopped, run a new preflight,
resolve and review the new exact artifacts, and invoke apply again. Apply
never reruns preflight, remints a planned identity, substitutes a new
artifact, or replans inside the exclusive mutation call. There is no loop or
numeric cap; repeated stale recapture leaves the service stopped.

## 7. Error code conformance

### 7.1. Real codes

- `error.project_catalog_cli_lock`: the CLI refusal when the lifetime lock is
  held. Used in `open_admin_store` (`src/bin/blackbox.rs:631,635,641`).
- `error.project_catalog_migration_artifact_identity`: the four-hash identity
  refusal. Used in the migration facade
  (`project_catalog_migration.rs:900,906,3727,3776,3787`).
- `error.project_catalog_lifetime_lock_busy`: used by `initialize_empty` when
  the exclusive lock is unavailable (`project_catalog_store.rs:651`).

### 7.2. Staleness suffix family

Staleness is a SUFFIXED family in `project_catalog_inventory.rs`:
`error.project_catalog_inventory_stale_report` (line 2104),
`stale_post_image` (2919), `stale_plan_input` (3193),
`stale_resolution` (3240). New commands conform.

### 7.3. New codes

Codes introduced by this plan are marked NEW and conform to the
`error.project_catalog_*` naming family:

- `error.project_catalog_rebuild_manifest_missing` (NEW): startup gate cannot
  find a committed `RepoHistoryRebuildManifestV1` for a migrated origin with
  materialized legacy commit documents.
- `error.project_catalog_rebuild_generation_unverified` (NEW): the manifest
  or record names a generation whose on-disk state does not match the
  committed count/hash proof.
- `error.project_catalog_durable_backfill_resolution_invalid` (NEW): the
  backfill resolution names a disposition inconsistent with the predecessor
  inventory (e.g., mapping or superseding a non-quarantined binding, or any
  delete disposition).
- `error.project_catalog_rebuild_proof_mode` (NEW): the rebuild's
  `prove_against_inventory` outcome, or the committed manifest's recorded
  `proof_mode`, is not `HistoryProofModeV1::Equality` (D-036). Raised by
  rebuild apply/verify and by the P6-C startup gate on cut-time manifests.

No invented codes like `error.project_catalog_bridge_still_running` or bare
`error.project_catalog_inventory_stale` appear in this plan.

## 8. Closeout window

### 8.1. Operator-agreed duration

The operator declares the duration; no fixed hours are mandated. The window
opens after Phase 5 exit gates pass and the closeout observation surface is
verified.

### 8.2. Mandatory coverage set

The window is not satisfiable until its coverage set is exercised: restart,
maintenance, indexing, publication, and cutback. Each must succeed; a missing
exercise resets the window.

### 8.3. Counter snapshots and pass rule

The window records a baseline counter snapshot at open and a closing snapshot
at close, both from the section 9.2 lease-counter surface persisted through
restart via the roll-forward observation snapshot
(`src/server/open.rs:188`, `checkout-access-observations.json`).

The pass rule: nonzero compatibility-path counters BLOCK the window, except
where a nonzero reading maps to exactly one section 14 adapter row and one
brokered lease. A counter that maps to no adapter row blocks. A counter that
maps to multiple adapter rows is ambiguous and blocks. The pass rule, not the
clock, is the gate.

Any persistence gap or blocking delta RESETS the window.

## 9. Rollback proof

### 9.1. Quiescent post-cut copy

Rollback proof stops nothing in production. It runs against a quiescent
post-cut copy of the configured state root. The copy must be storage-atomic or
captured after a proved stop.

### 9.2. Narrowed proof scope with accepted index rebuild

The post-cut copy carries the P3-E path-free index schema
(`INDEX_SCHEMA_VERSION` at `crates/bbox-corpus-index/src/index/mod.rs:16`,
value `"agentic-corpus-g10-code-source-selectors"`). The retained pre-cut
bridge binary reads this as a schema mismatch and triggers
`reset_index_on_schema_mismatch` (`index/mod.rs:995`), which deletes the index
directory and rebuilds from source.

This plan adopts a narrowed proof scope: the rollback proof accepts the
schema-mismatch reset on the copy and proves project identity, publisher
references, and collected-code rematerialization. It does not prove v1 query
parity. Steps:

1. Restore retained v1 bytes (project store, `publisher-refs.json`, and the
   code-source store at `<state>/code-sources/` containing activation records,
   stored generations, and generation descriptors) and checksums from the
   migration backup.
2. Start the retained pre-cut bridge binary with the compatibility reader.
3. Accept the schema reset: the binary deletes the path-free index and
   rebuilds from restored v1 source. For attachment-bearing projects, history
   comes from the restored checkout tree. The pre-cut binary is the Phase 0
   bridge release and predates Phase 3, so it has no P3-E spill-lane reader;
   bridge-era commit documents that exist only in `<state>/commit-spill/` are
   NOT recoverable. For attachment-less projects, collected code
   rematerializes through the code-source store's manifest/blob machinery
   (`CodeSourceStore` entries verified by
   `verify_collected_schema_migration_sources` at `index/mod.rs:1026`).
4. Prove every project resolves, publisher references match
   `publisher-refs.json`, and collected code rematerializes.
5. Stop the bridge. Live v2 state remains untouched.

The assertion set is: project-set identity, publisher reference integrity, and
collected-code rematerialization completeness. The proof does NOT assert v1
query parity. Bridge-era spill-lane documents and remote-only commit history
may not restore; the later retirement gate must account for this.

### 9.3. Why not live-state restore

A live-state restore is a second destructive mutation of a shared production
service performed only to demonstrate a capability. Governing section 15 item 7
treats rollback proof as EVIDENCE gating a later retirement, not as an
operation on live state.

## 10. Retention and GC

### 10.1. Named retention inventory

Retained and GC-protected through and beyond this phase: v1 project bytes and
checksums; `publisher-refs.json` and checksums; compatibility reader and
bridge record decode path; pre-cut bridge binary and hash; every migration
journal, apply receipt, verification receipt, and collision retirement
terminal receipt (D-019); old index views; owned, ambiguous, and unclaimed
history generations (D-027); quarantine assets; G1 assets; migration backups;
and the `BackfillCompletionJournalV1` (section 3.3), which is origin-scoped to
`MigratedV1` (a `FreshV2` store never runs a backfill and never produces one).

### 10.2. GC exclusion

External sweeps exclude transaction stage, history-rebuild stage, backup, G1,
and quarantine roots per governing section 16. Exclusion is driven by the
committed migration marker's named inventory rather than a path glob.

Marker-driven GC exclusion and refusal apply to `MigratedV1` origins only. A
`FreshV2` store (`CatalogOriginV2::FreshV2` at `project_catalog.rs:479`)
legitimately carries no migration marker (D-011: "A fresh-v2 origin does not
require a marker") and no rollback assets. The marker-absence refusal does not
fire on `FreshV2` stores.

For `MigratedV1` origins, a marker that is absent, corrupt, or incomplete
refuses GC instead of sweeping.

### 10.3. Retirement criteria (later phase, not Phase 6)

Phase 6 retires nothing. Later retirement of the bridge lane requires: zero
non-intentional checkout observations across the closeout window; cutback
proven; rollback proof completed; no prepared journals; verified GC roots
with no orphaned quarantine or G1 assets; explicit operator approval.

## 11. Milestones

Code-only milestones (P6-A, P6-B, P6-C) are strictly before operational
milestones (P6-D through P6-H). The two kinds never interleave. No code
changes occur after P6-C closes.

### P6-A (code-only): deletion surface

**Scope:** Delete the path-derived project-id surface, the direct
`load_project_records` consumer, and convert test callers of the legacy
`open_or_create`.

**Tasks:**

1. Verify every Phase 5 exit gate has landed (Phase 5 section 16). Consume the
   P5-H handoff inventory by name: remaining bridge `ProjectRecord` uses and
   legacy source lanes drive the deletion surface in tasks 2 and 3. The
   handoff is consumed for THIS section-5 surface, the compatibility
   observation counters (P6-D), and the parity/ratchet discipline only; its
   wider deletion campaign (handoff section 5 steps 2 through 5, parity
   fixture) is retirement-phase inventory outside Phase 6 (section 1
   deletion boundary).
2. Route the 1 catalog-resolution call site (section 5.1) through the catalog
   resolver or remove dead paths.
3. Delete the direct `load_project_records` consumer
   (`StaticProjectRecordsProvider::from_projects_path`, `index/mod.rs:208`)
   and the legacy `open_or_create` path (`index/mod.rs:271`).
4. Convert every test/offline caller of `open_or_create` (section 5.3) to
   pass a `StaticProjectRecordsProvider` from catalog-bridge records.
5. Retain `project_files::short_hash`/`rel_path_hash` (section 5.1) and
   `IdentityOrigin::Bridge`/`from_bridge_records`/bridge compat lane (FD-8).

**Exit gate:** No catalog-mode code path derives project identity from a host
path or calls `ProjectRegistry::register_path`. The bridge decode path
compiles and round-trips. Every test caller of `open_or_create` is converted or
removed.

### P6-B (code-only): new commands and existing-verb changes

**Scope:** Implement the two new verbs, the configured-target capability on
`migrate`, and the exclusive-availability proof mode on `verify`.

**Tasks:**

1. Add `DurableBackfill(DurableBackfillArgs)` and
   `PathFreeRebuild(PathFreeRebuildArgs)` to `ProjectCatalogCommand`
   (`src/bin/blackbox.rs:45`). Each has `ArgGroup("mode")` over
   `preflight|apply|verify` (required, exactly one), `--report` and
   `--resolution` required for preflight and apply, and target flags per
   the section 3.1 mode rules: exactly one of
   `--rehearsal-root`/`--configured` for apply and verify, optional
   `--rehearsal-root` for preflight (D-026 bundle preflight).
2. Add `--configured` to `MigrateArgs` with `required_if_eq("apply","true")`
   and `conflicts_with = "rehearsal_root"`. Preflight requires neither.
3. Add `--require-exclusive-availability` to `VerifyArgs`.
4. Implement the durable-backfill facade method: capture the `LegacyPathBinding`
   ledger from the applied migration post-image, classify rows as
   mappable/ambiguous/unscoped (section 3.3), plan stamp operations, verify
   publisher/G1 coverage, and produce the report and resolution. Apply stamps
   all mappable rows idempotently under exclusive lock; unresolved quarantined
   rows ride forward as counted quarantine (section 3.3).
5. Implement the path-free-rebuild facade as a thin caller of the Phase 3
   materializer and `RepoHistoryRebuildManifestV1` creation path (P3-D):
   capture the backfill post-image, drive the materializer to produce the
   report and resolution, install under exclusive lock. No parallel manifest
   writer.
6. Implement verify mode for both verbs: backfill verify checks that the
   applied stamp set matches `BackfillCompletionJournalV1` and the ledger is
   consistent under the supersession rule (section 3.3); rebuild verify checks
   that every named generation is present and hash-verified, the manifest
   is committed, and the committed manifest records
   `HistoryProofModeV1::Equality` with matching source fingerprints
   (D-036, section 3.4).
7. Wire `command_name()` for the six new envelope values (section 3.1).
8. Both new apply paths use `open_admin_store` (section 4.2) for
   `--configured`; both preflight paths acquire the shared lifetime lock
   (section 4.1).
9. Add the new error codes (section 7.3).

**Exit gate:** Both commands produce the D-020 versioned envelope with
snake_case `command` values. Preflight acquires the shared lifetime lock.
Apply uses
exclusive-then-downgrade. The artifact hash graph is acyclic (FD-4). Backfill
stamps `LegacyPathStoreKindV1` rows and verifies publisher/G1 coverage without
seeding.

### P6-C (code-only): validation, verification, recovery

**Scope:** Startup-before-bind validation scoped to migrated origins,
every-generation verification, GC roots, and fault injection. No code changes
after this milestone.

**Tasks:**

1. Startup validation gate: before any v2 route binds, check the rebuild
   manifest for `MigratedV1` origins only. The gate uses a two-tier coverage
   check:
   - **FreshV2** stores (`project_catalog.rs:479`): no manifest required. They
     have no legacy commit documents and boot without the gate (D-030).
   - **MigratedV1** stores (`project_catalog.rs:480`) with materialized legacy
     commit documents: two tiers.
     - **Cut-time generations** (named in the committed
       `RepoHistoryRebuildManifestV1`): require the manifest `Committed`,
       recording `HistoryProofModeV1::Equality` (D-036, section 3.4), and
       every generation in EVERY manifest bucket present on disk and
       count/commitment/hash-verified: the primary owned set, the
       `compatibility_generation_ids` bucket, and the ambiguous/unclaimed
       quarantine set. Compatibility-namespace generations have no catalog
       `Ready` owner and are reachable only through the manifest bucket
       (D-037); the gate must NEVER infer them from record `Ready`, and a
       gate that walked only `Ready` requirements would silently skip them.
     - **Live-refresh generations** (advanced through regular `transact` after
       the cut, per P3-F item 3): the manifest is NOT required to name these.
       The gate treats the RECORD as authority for the PRIMARY namespace
       only: the generation the record's current
       `RepoHistoryMaterialization::Ready { generation_id }` names must
       be present on disk and hash-verified. Quarantine records carry their
       own `RepoHistoryQuarantineMaterialization` state, distinct from
       `RepoHistoryMaterialization`. The manifest is cut-time evidence,
       not a live-coverage authority.
   Zero-legacy-namespace stores are exempt. Routine `transact` epoch advances
   (including P3-F live history refresh) are tolerated. A missing cut-time
   manifest refuses with `error.project_catalog_rebuild_manifest_missing`
   (NEW). An absent or hash-mismatched generation in any bucket refuses with
   `error.project_catalog_rebuild_generation_unverified` (NEW). A cut-time
   manifest recording a non-Equality proof mode refuses with
   `error.project_catalog_rebuild_proof_mode` (NEW).
2. GC roots: the committed migration marker's named inventory drives exclusion
   (section 10.2). Marker-driven refusal applies to `MigratedV1` origins only;
   `FreshV2` stores are exempt.
3. Bootsmokes: the catalog-mode smoke root produced by the facade-driving test
   is verified by the CLI (D-030 pattern). Add bootsmoke coverage for both new
   commands, for the fresh-v2 boot path (no manifest required), and for a
   post-cut live history refresh that advances `Ready` without a manifest write
   followed by a restart.
4. Fault injection: inject a torn backfill transaction and prove recovery to
   one coherent state. Inject a torn rebuild transaction and prove recovery.
   Inject a stale artifact at apply time and prove the four-hash identity
   refusal.
5. Cluster verification: pin one immutable ref and one binary. Run the full
   P6-A through P6-C test suite.

**Exit gate:** The startup gate refuses a missing or unverified rebuild
generation in ANY manifest bucket (primary, compatibility, quarantine) for
cut-time generations on migrated origins, and refuses a cut-time manifest
whose recorded proof mode is not Equality. Live-refresh generations
(advanced through P3-F transact without a manifest) are verified
against the record's `Ready` field, not the manifest; compatibility
generations are verified from the manifest bucket, never inferred from
`Ready`. Fresh-v2 and zero-legacy-namespace stores boot without the gate.
A post-cut routine transaction or live history refresh that advances the
epoch does not make the daemon unbootable. No code changes after this
milestone.

### P6-D (operational, non-cut): closeout window and cutback exercise

**Scope:** Observe compatibility-path counters through the agreed window and
exercise cutback.

**Tasks:**

1. Open the closeout window with a baseline snapshot, consuming the P5-H
   handoff inventory's compatibility counters as the baseline.
2. Exercise the mandatory coverage set (section 8.2): restart, maintenance,
   indexing, publication, cutback.
3. Record the closing snapshot. Verify nonzero counters map to exactly one
   section 14 adapter row and one brokered lease.
4. Freeze the evidence identity: binary hash, git ref, snapshots, and
   coverage-set log.

**Exit gate:** The window passes the pass rule (section 8.3). Evidence
identity frozen.

### P6-E (operational, non-cut): final rehearsal against a storage-atomic copy

**Scope:** Complete migration, backfill, rebuild, and v2 rehearsal against an
exact copied inventory.

**Tasks:**

1. Obtain operator approval for a storage-atomic or proved-stop final copy.
2. Run migrate preflight and apply against the copy.
3. Run backfill preflight and apply (after migration apply).
4. Run rebuild preflight and apply (after backfill apply).
5. Verify every ambiguous or unclaimed namespace materializes into an
   immutable quarantine generation (an `rhg_`-id'd generation tracked by
   `RepoHistoryQuarantineGenerationId` with
   `RepoHistoryQuarantineMaterialization` state) with complete ordered
   document commitments (D-027).
6. Run exact post-image verification: every generation in every manifest
   bucket verifies (primary, `compatibility_generation_ids`, quarantine;
   D-037), the manifest covers every `Ready` requirement, and the
   committed manifest records `HistoryProofModeV1::Equality` with
   matching source fingerprints (D-036).
7. Run the v2 daemon and verify catalog-only queries resolve, cutback
   completes, and adapter surfaces degrade per the section 14 table.

**Exit gate:** The complete rehearsal succeeds against the exact copy with no
identity refusals and no missing generations.

### P6-F (operational, THE CUT): live migration

**Scope:** Stop the bridge and apply the configured migration under the
exclusive lock.

**Tasks:**

1. Run the live migration preflight while the bridge is live (preflight
   acquires the shared lifetime lock, section 4.1). Review the report and resolution.
2. Coordinate the bridge stop per the runbook (shared-service approval).
3. Stop the bridge daemon.
4. Bridge-down proof: `verify --require-exclusive-availability` returns
   `Ok(Some(_))`.
5. Configured migration apply: `migrate --apply --configured --report <path>
   --resolution <path>`. Uses `open_admin_store` (section 4.2), verifies
   four-hash identity, installs post-image.
6. If apply reports stale inventory, leave stopped, run new preflight, review
   new exact artifacts, invoke apply again (section 6.2).
7. Verify: `verify --require-exclusive-availability --config <path>`.
   Migration verification receipt confirms exact installed state.

**Exit gate:** Configured migration applied and verified. Service stopped with
the exclusive lock proven available.

### P6-G (operational, still stopped): backfill, rebuild, rollback-proof copy

**Scope:** Apply backfill and rebuild in sequence, then capture the rollback
proof copy.

**Tasks:**

1. Backfill preflight: capture the applied migration post-image. Review the
   `LegacyPathBinding` ledger classification and publisher/G1 verification.
   Quarantined rows without an explicit disposition ride forward as counted
   quarantine (section 3.3).
2. Backfill apply: `durable-backfill --apply --configured --report <path>
   --resolution <path>`. Stamps all mappable `LegacyPathStoreKindV1` rows
   idempotently; unresolved quarantine rides forward.
3. Backfill verify.
4. Rebuild preflight: capture the backfill post-image. Review report and
   resolution.
5. Rebuild apply: `path-free-rebuild --apply --configured --report <path>
   --resolution <path>`. Drives the Phase 3 materializer to produce a
   committed `RepoHistoryRebuildManifestV1` (Phase 3 P3-D).
6. Rebuild verify.
7. Capture the quiescent post-cut rollback-proof copy (section 9.1).

**Exit gate:** Backfill and rebuild applied and verified in sequence. Rebuild
manifest committed with full coverage.

### P6-H (operational): v2 start, live checks, rollback proof, evidence

**Scope:** Start the v2 daemon, verify live behavior, prove rollback on the
copy, and publish the evidence bundle.

**Tasks:**

1. Start the exact v2 binary pinned in P6-C.
2. Startup validation gate passes for the `MigratedV1` origin: rebuild
   manifest committed in Equality proof mode, every manifest bucket and
   every `Ready` requirement covered (P6-C).
3. Catalog-only live check: every corpus-only query resolves with an empty
   attachment store.
4. Cutback live check: auth swap completes and resume converges exactly once.
5. Adapter live check: every remaining nonzero checkout operation matches
   exactly one section 14 adapter row.
6. Offline rollback proof: run the narrowed-scope proof (section 9.2) against
   the quiescent post-cut copy. Accept schema reset, prove project-set
   identity, publisher reference integrity, and collected-code
   rematerialization.
7. Retain all rollback assets per section 10. Marker-driven GC exclusion
   scoped to `MigratedV1`; nothing retired.
8. Publish the evidence bundle: frozen binary hash and git ref, closeout
   window snapshots and coverage-set log, final-rehearsal receipts,
   migration/backfill/rebuild apply and verify receipts, rebuild manifest,
   rollback-proof copy checksum, and adapter live-check results.

**Exit gate:** The v2 daemon starts with an empty attachment store and serves
all catalog-only code. Remaining nonzero checkout operations match the
adapter table. Rollback proof passes on the copy. All rollback assets retained
and GC-protected. Evidence bundle published.

## Signoff

This plan implements governing section 15 Phase 6 items 1 through 7. It
defines durable-backfill and path-free-rebuild as versioned
`blackbox project-catalog` subcommands using the v1 result envelope (D-020),
exclusive-lock exact-root preflight/apply/verify conventions, and the existing
receipt vocabulary (D-028). Durable-backfill owns the governing 7.3 row
stamping; publisher/G1 seeding remains the migration transaction's
responsibility (governing 13.2, D-006, D-014).

Lock discipline: preflight acquires the shared lifetime lock and the store
mutation lock through `capture_migration_preflight_with`
(`project_catalog_store.rs:3901-3912`), matching governing section 6.3; apply
uses exclusive-then-downgrade with point-in-time exclusivity at acquisition
and the stopped-service window plus four-hash recheck as the real exclusion
(`open_admin_store` at `src/bin/blackbox.rs:620-645`).

Startup gate: scoped to `MigratedV1` origins; cut-time generations verified
against the committed manifest, live-refresh against the record's `Ready`
field; fresh-v2 and zero-legacy-namespace stores exempt (D-011).

Backfill quarantine: unresolved rows ride forward as counted quarantine under
the supersession rule (section 3.3), blocking only the later path-fallback
removal gate, not the Phase 6 cut.

Rollback proof: narrowed scope on a quiescent copy, accepting schema reset.
The pre-cut binary rebuilds from the restored checkout tree and code-source
store; bridge-era spill-lane documents are not recoverable. Does not guarantee
remote-only commit-history recovery.

GC exclusion: marker-driven refusal scoped to `MigratedV1`; `FreshV2` stores
carry no rollback assets (D-011). Error codes conform to
`error.project_catalog_*`; envelope values are snake_case.

DECISION_LEDGER.md citations: 16 entries (D-002, D-004, D-006, D-011, D-014,
D-019, D-020, D-021, D-025, D-026, D-027, D-028, D-029, D-030, D-032, D-034),
each verified line-by-line at authoring time.
