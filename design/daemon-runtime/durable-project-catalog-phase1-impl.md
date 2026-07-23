---
title: "Durable project catalog Phase 1 implementation plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, project-identity, catalog, attachments, migration, phase-1]
brief: "Implement the path-free catalog contract, strict catalog and attachment stores, crash-safe transaction owner, and rehearsable v1 migration without activating v2 operator state before the Phase 6 cut."
---

# Durable project catalog Phase 1 implementation plan

Date: 2026-07-22

Governing design:
[`durable-project-catalog-impl.md`](durable-project-catalog-impl.md), especially
sections 5, 6, and 15.

This is the executable plan for Phase 1 only. It does not replace or narrow the
governing design. The independent reviewer must read this document, the complete
governing design, every governing companion listed there, the current code, and
the fixed baseline `monolith-decomposition-pre-attempt-2`.

## 1. Required outcome

Phase 1 creates the durable identity, persistence, and migration substrate that
later phases can exercise without cutting configured operator state:

1. pure, typed, path-free catalog models and strict codecs in
   `bbox-corpus-core`;
2. a separate strict host-local attachment model;
3. one journaled owner for every catalog-only, attachment-only, paired, or
   multi-participant migration mutation;
4. a deterministic v1 inventory, hash-bound resolution format, and complete
   migration engine;
5. migration-only accepted-publication G1 persistence and collected-source
   quarantine participants;
6. a dedicated offline `blackbox project-catalog` CLI;
7. a derived `ProjectRecord` compatibility view with explicit legacy-v1 input
   types; and
8. proof that Phase 1 does not remove or break the bridge daemon's existing
   behavior.

Phase 1 does not activate v2 bytes at the configured operator store. The live
resolver, path-free index, collector state machine, publisher/view cut, and
remaining adapters land through Phases 2 to 5. Phase 1 can apply only into an
explicitly selected isolated rehearsal root and verify the exact resulting
post-images. Phase 6 removes that guard only after the complete v2 runtime can
preserve every existing project operation.

This boundary is intentional. A command that installs bytes the only runnable
daemon cannot consume is not a complete migration feature. A bridge daemon that
silently accepts both authority models would make the migration cut and
rollback ambiguous.

## 2. Survey of the current tree

### 2.1 Landed Phase 0 bridge

The current tree at `73f093fb` contains:

- response-local `built_from` provenance for knowledge and gaps;
- published and provisional knowledge/gap views;
- the closed checkout-access kind set, broker, validated leases, observation
  counters, health, and deny-access test authority;
- lease-backed adapters for the named checkout surfaces;
- a recoverable checkout census at `checkout-registry.json`;
- distributed code-source storage, upload, activation, collected indexing, and
  cutback primitives;
- the process-lifetime `project-catalog-migration.lock`; and
- a version-1 `ProjectRegistry` that holds that lock for its lifetime.

The exact-HEAD baseline boots after stable signing with isolated state, serves
HTTP and MCP initialization, and creates the migration lock without creating a
catalog.

### 2.2 Missing Phase 1 authority

The current tree does not contain:

- typed `ProjectId`, `CorpusProject`, `ProjectScope`, `RepoHistoryId`,
  `CommitNamespace`, or repository-authority wrappers;
- a path-free catalog snapshot;
- a strict authoritative attachment snapshot;
- cross-snapshot validation;
- a paired-store epoch or journaled transaction owner;
- migration report or resolution schemas;
- a complete preflight/apply engine;
- accepted-publication generation/pointer persistence;
- migration participants for source-selection quarantine and G1 seeding;
- a `blackbox` administration executable; or
- a catalog-derived compatibility join.

`ProjectRecord` still combines logical identity and a canonical host path.
`ProjectStore` is version 1 and performs only serde decoding, with no complete
store validation. `ProjectRegistry` persists independently through
`StorePersister`. The checkout census is intentionally recoverable by
recomputation and therefore cannot be promoted into attachment authority.

### 2.3 Fixed baseline for final parity

`monolith-decomposition-pre-attempt-2` resolves to `254cabf0`. From that tag to
the Phase 0 head:

- no tracked file was deleted;
- `git grep -n -F '#[tool(' <ref> -- 'src/tools/*.rs' | wc -l` counts 172
  declarations at `254cabf0` and 173 at Phase 0 head `73f093fb`; and
- the current full suite, workspace clippy, concurrency lint, exact-SHA cluster
  verification, stable-signed isolated boot, HTTP probes, and MCP initialize
  are green.

These observations are the start of the parity matrix, not final proof. Every
later phase must keep the baseline tag as the comparison boundary. Counts alone
never prove behavioral parity.

## 3. Non-goals and phase boundary

Phase 1 does not:

- route live selector surfaces through the catalog;
- change live register, rename, unregister, init, eject, publisher advance, or
  query semantics;
- make remote-only projects visible to the current runtime;
- rewrite index schemas or remove absolute path fields;
- activate catalog-driven collected sources;
- move remaining checkout adapters;
- apply a migration to production or persistent dev state;
- restart or replace a shared daemon service; or
- claim parity for the entire six-phase arc.

Those exclusions must not be implemented as missing functionality in an
activated v2 runtime. Phase 1 stays additive and rehearsable. Phase 6 performs
the coherent operator-state cut after every runtime consumer is present.
The configured live service remains on the last deployed bridge-compatible
binary through Phases 2 to 5; later binaries run only against isolated v2 state
until the approved Phase 6 replacement.

## 4. Milestone P1-A: pure contract and strict codecs

### 4.1 Ownership and files

Add `crates/bbox-corpus-core/src/project_catalog.rs` and export it from the
crate root. This module owns pure types, constructors, validation, canonical
ordering, v1 decode input, v2 catalog decode, and catalog/attachment
cross-validation inputs. It must not depend on another bbox crate, inspect the
filesystem, run Git, or know daemon configuration.

Keep filesystem canonicalization, committed-config reads, Git proof, and store
mutation in `bbox-indexing`.

### 4.2 Bounded identity wrappers

Add validated newtypes:

```text
ProjectId
RepoHistoryId
RepoHistoryGenerationId
RepoHistoryQuarantineGenerationId
RecordedRepoAuthority
RepoBootstrapHint
CommitNamespace
AttachmentId
```

Each wrapper:

- constructs only through `parse` or a code-owned minting function;
- uses custom serde deserialization that invokes the same parser;
- has a bounded display representation;
- exposes `as_str` without exposing unchecked construction;
- derives deterministic equality, hashing, and ordering; and
- rejects control characters, whitespace, separators, traversal tokens,
  percent escapes, and platform prefixes where applicable.

`ProjectId` accepts 1 through 96 ASCII alphanumeric, `_`, `-`, or `.`
characters, except `.` and `..`. New ids are `p_` plus 32 lowercase
hexadecimal characters generated from operating-system randomness. Minting
checks catalog membership and retries a bounded number of times.

`RepoHistoryId`, `RepoHistoryGenerationId`,
`RepoHistoryQuarantineGenerationId`, and `AttachmentId` use distinct
code-owned prefixes so logs and diagnostics cannot confuse them with project
ids or with one another. Their values are still opaque and never encode a
path, alias, scope, repository URL, token, or Git ref. The two generation ids
are content-addressed by the Phase 3 materializer under the governing
domain-separated contracts; Phase 1 only validates and preserves them.

`RecordedRepoAuthority`, `RepoBootstrapHint`, and `CommitNamespace` are
different types even when legacy bytes happen to match. No caller may infer
authority or provenance from length or formatting.

### 4.3 Catalog model

Implement the governing model:

```text
CorpusProject
ProjectScope
RepoHistoryRecord
RepoHistoryAuthority
AmbiguousNamespaceRecord
ScopeMigrationId
ScopeMigrationRecord
CatalogOriginV2
CatalogSnapshotV2
```

Use `BTreeMap`/`BTreeSet` for durable keyed collections and stable
serialization. A snapshot has:

```text
version = 2
epoch: u64
origin: fresh_v2 | migrated_v1 { transaction_id }
projects: BTreeMap<ProjectId, CorpusProject>
repo_histories: BTreeMap<RepoHistoryId, RepoHistoryRecord>
ambiguous_namespaces: BTreeMap<CommitNamespace, AmbiguousNamespaceRecord>
scope_migrations: BTreeMap<ScopeMigrationId, ScopeMigrationRecord>
```

`CorpusProject` contains no path-bearing field. `display_name` is bounded
presentation data, not a selector or authority. Languages use the existing
`Language` enum. Accepted operator aliases and nominated aliases remain
separate collections.

`ProjectScope::Published` carries the existing `PublishedScope` only after a
new strict `PublishedScope::validate` check. `LegacyLocal` carries no producer
wire authority. Existing raw `PublishedScope` construction remains available
only through a bounded compatibility constructor until its callers migrate.

`RepoHistoryAuthority` distinguishes recorded repository authority,
server-minted `LocalProject(ProjectId)` authority, and imported legacy
namespace provenance. A new attached Git `LegacyLocal` project receives a
random project-bound commit namespace and may refresh local history through a
validated Git-history attachment. That authority cannot publish, satisfy a
producer grant, identify another project, or claim cross-host sameness.
Non-Git `LegacyLocal` projects create no commit history.

`RepoHistoryRecord.materialization` is exactly `NotBuilt` or
`Ready { generation_id: RepoHistoryGenerationId }`.
`AmbiguousNamespaceRecord.materialization` is exactly `NotBuilt` or
`Ready { generation_id: RepoHistoryQuarantineGenerationId }`. Phase 1
migration post-images always write both forms as `NotBuilt`: the importer
inventories namespace ownership and complete commit/vector commitments but
does not copy commit-document bodies or create history assets. Phase 3 is the
sole creation owner and advances these fields through the regular catalog
transaction only after strict generation verification.

`ScopeMigrationRecord` is the path-free logical audit and compatibility bridge
defined by governing section 7.2. It lives inside the catalog snapshot, keyed
by a bounded random `ScopeMigrationId`, so scope, attachment, and migration
record changes share the regular catalog/attachment pair transaction. Records
carry their embedded id, committed catalog epoch, bounded operator invocation,
typed `ProjectScope` old/new values, and kind. `promotion` is
`LegacyLocal -> Published`; `relpath_move` and `repo_authority_change` are
`Published -> Published`. Records form one nonbranching transition chain per
project. They are provenance and bridge state, never current selector
authority. They contain no host-local attachment id. Attachment-proved
migrations store their matching checkout and scope evidence as
`ScopeMigrationAttachmentProof` in the attachment snapshot; operator-attested
migrations have no proof row.

`CatalogOriginV2::FreshV2` identifies a store initialized directly at v2.
`CatalogOriginV2::MigratedV1` carries the migration transaction id. Strict open
requires a committed migration marker with the same transaction id only for
the migrated origin. The complete plan hash stays in the marker and journal,
avoiding a hash cycle through the catalog post-image. This makes marker loss
detectable without rejecting a genuine fresh v2 store.

### 4.4 Authoritative attachment model

Implement:

```text
CheckoutAttachment
AttachmentKind
AttachmentCapabilities
AttachmentStatus
LegacyPathBindingId
LegacyPathLedgerEntry
ScopeMigrationAttachmentProof
AttachmentSnapshotV1
```

The attachment snapshot is host-local strict state:

```text
version = 1
epoch: u64
attachments: BTreeMap<AttachmentId, CheckoutAttachment>
scope_migration_proofs:
  BTreeMap<ScopeMigrationId, ScopeMigrationAttachmentProof>
legacy_path_bindings: BTreeMap<LegacyPathBindingId, LegacyPathLedgerEntry>
```

Paths may appear only here. `checkout_dir` and `checkout_project_dir` must be
absolute, normalized paths at codec validation time and canonical paths when
admitted by the indexing owner. An attached entry must have a nonempty
`checkout_id`, an existing catalog project reference, a valid project-relative
monorepo discriminator, and explicit capabilities. A directory's existence is
not a capability.

An authoritative attached `checkout_id` must come from a valid
`.bbox/local/checkout-id` marker. The bridge's deterministic `v1-root` read
fallback is inventory evidence only and can never enter this snapshot.
Migration normalizes eligible markerless roots through the planned,
idempotent identity actions in section 6.

The path-bearing `LegacyPathBinding` ledger from governing section 7.3 lives in
the host-local attachment snapshot, never the catalog. Entries are mapped,
unscoped, or quarantined and retain their bounded historical path,
source-store/row identity, relationship, and inventory epoch. Attachment
relocation appends a binding in the same pair transaction that changes the
attachment and any catalog scope-migration record.

An attachment-proved scope migration stores its attachment id, checkout id,
revalidated old/new scope evidence, and proof timestamp only in
`ScopeMigrationAttachmentProof`. The path-free catalog record stores the
logical provenance class but never a host-local attachment identity.

The existing `CheckoutRegistry` stays a recoverable discovery census. No
catalog or lease code may accept a census row as attachment authority.

### 4.5 Complete validation

Pure `validate` functions reject the complete snapshot when any of these hold:

- unsupported version or zero/invalid epoch;
- map key and embedded id disagree;
- duplicate project id, published scope, accepted alias, repository authority,
  repo-history id, or active commit namespace;
- an accepted or nominated alias violates the governing 1–96-byte alias
  contract, or an accepted alias collides with a project id or another
  accepted alias;
- a project references a missing repo-history record;
- a scope-migration key and embedded id disagree, references a missing project,
  has equal old/new scope, branches or breaks continuity in a project's
  migration chain, or claims an epoch newer than the snapshot;
- a scope-migration kind disagrees with its typed old/new `ProjectScope`
  transition;
- a project's current scope disagrees with the final transition in its chain;
- a recorded repo-history authority disagrees with a published project using
  that record;
- a `Ready` repo-history or ambiguous-namespace record carries the wrong
  generation-id type or a generation id that fails its strict parser;
- a primary namespace also appears as incompatible or ambiguous ownership;
- an ambiguous namespace is accepted for ordinary commit resolution;
- an attachment references a missing project;
- attachment and catalog published scopes disagree;
- active `(project_id, checkout_id, project_root_relpath)` uniqueness is
  violated;
- two active attachments for different projects share one
  `(checkout_id, project_root_relpath)`;
- a mapped legacy path binding references a missing project, has a malformed
  absolute historical path, or disagrees with its embedded id/status;
- a scope-migration proof references a missing migration record or attachment,
  disagrees with the record's provenance class, or fails old/new scope
  revalidation;
- an attachment-proved migration record lacks exactly one matching proof, or an
  operator-attested record has any proof row;
- an attached path or checkout identity is malformed; or
- detached entries claim an active capability.

Nominated aliases may conflict because they are untrusted proposals. They
never participate in selector resolution until explicitly accepted.

Serde structs for strict snapshots use `deny_unknown_fields`. Decoders place a
hard byte cap before parsing and a hard collection cap after parsing. Error
messages name the bounded record id and field class, never secret config or an
unbounded path.

### 4.6 Explicit legacy input and compatibility join

Add `LegacyProjectStoreV1` and `LegacyProjectRecordV1` as the only v1
persistence types. They preserve exact current serde compatibility, including
missing defaulted fields.

`ProjectRecord` remains the temporary public compatibility DTO, but add one
constructor:

```text
ProjectRecord::from_catalog_attachment(project, validated_attachment)
```

It refuses a detached, cross-project, scope-mismatched, or insufficiently
validated attachment. A catalog project with no attachment has no
`ProjectRecord`; no fake path is synthesized.

During Phase 1, the bridge `ProjectRegistry` continues persisting
`LegacyProjectStoreV1` and maps its records to the existing compatibility DTO.
This makes the persistence distinction explicit without cutting live
consumers early.

### 4.7 P1-A tests and gate

Tests cover every accepted and rejected character class, bounded mint retry,
serde rejection bypass attempts, deterministic ordering, every uniqueness
rule, dangling references, scope disagreement, monorepo attachment uniqueness,
detached capability refusal, local-project history authority isolation,
promotion-compatible namespace preservation, typed history-generation id
separation, `NotBuilt`/`Ready` round trips and malformed-ready refusal, legacy
fixture decoding, and compatibility joins.

Run:

- targeted nextest for `bbox-corpus-core` and `bbox-indexing`, always with
  `--workspace`;
- `scripts/fmt.sh --check`;
- workspace clippy on the cluster after push;
- `scripts/lint-concurrency.sh`; and
- the stable-signed isolated bootsmoke in section 9.

Commit and push P1-A only after the smoke passes.

## 5. Milestone P1-B: strict transaction owner and recovery

### 5.1 Ownership and paths

Add `crates/bbox-indexing/src/project_catalog_store.rs`.

Derive all paths from the configured `projects.json` path:

```text
projects.json
project-attachments.json
project-catalog-transaction.json
project-catalog-migration.json
project-catalog-stage/
project-catalog-backups/
projects.json.lock
```

The catalog remains at `projects.json`. The attachment store, journal,
migration marker, stage directory, and backups are siblings. The short mutation
lock is the existing canonical `projects.json.lock` returned by
`with_store_lock(projects_path)`, so the bridge `StorePersister`, preflight,
regular v2 transactions, and migration transactions exclude one another.
Path derivation is centralized and unit-tested for an arbitrary configured
filename and parent.

The process-lifetime migration lock and `projects.json.lock` have different
jobs:

- the lifetime lock prevents bridge/v2 daemon and offline apply overlap;
- `projects.json.lock` serializes bridge writes, snapshot reads, recovery, and
  transactions.

Never reuse the recoverable checkout-census lock as either authority.
Lock order is lifetime migration lock, `projects.json.lock`, then auxiliary
participant locks in deterministic role/path order.

### 5.2 Strict open

`ProjectCatalogTransactionOwner::recover` runs before any participant opens. It
uses the journal kind to require the complete code-owned participant registry;
a migration journal cannot be recovered by a catalog-only fallback.

After recovery, `ProjectCatalogStore::open`:

1. acquires the appropriate lifetime lock;
2. acquires `projects.json.lock`;
3. rejects any unrecovered prepared journal;
4. reads both snapshots with byte caps and no-follow regular-file checks;
5. validates each snapshot and all cross references;
6. verifies matching nonzero epochs; and
7. returns immutable snapshots plus their hashes.

Missing both files is a valid empty-v2 initialization only for an explicitly
new store. Exactly one missing file, v1 bytes at the catalog path, corrupt JSON,
unsupported versions, unknown fields, mismatched epochs, symlinks, and cross
validation errors fail closed.

An empty store initializer writes both snapshots through the same transaction
protocol. It does not create one file and infer the other.

Strict open inspects `CatalogOriginV2`. `FreshV2` requires no migration marker.
`MigratedV1 { transaction_id }` requires a committed marker with the matching
transaction id after recovery, even when both strict snapshots are otherwise
valid.
Missing or mismatched evidence fails
`error.project_catalog_migration_incomplete`.

### 5.3 Mutation API

Expose one owner API:

```text
transact(expected_epoch, build_post_images)
transact_migration(validated_plan)
```

The caller receives immutable current snapshots and returns complete catalog
and attachment post-images. The owner:

- rejects stale `expected_epoch`;
- increments both epochs exactly once;
- preserves `CatalogOriginV2` byte-for-byte for every regular transaction;
- validates complete post-images before filesystem mutation;
- writes both post-images even for a catalog-only or attachment-only change;
- verifies installed bytes before publishing the new in-memory pair; and
- returns the new epoch and hashes.

No public method writes one strict file directly.

`transact_migration` uses the same owner, journal codec, filesystem discipline,
and recovery classifier. Catalog and attachment are mandatory participants.
The validated plan may add only code-owned participant roles described in
section 6. It cannot supply arbitrary target paths or invoke an independently
committing pair transaction.

### 5.4 Journal and durable artifacts

The journal includes:

```text
version
transaction_id
kind: regular_pair | v1_migration
state: prepared | committed
plan_hash?
old_epoch
new_epoch
participants: [
  {
    role,
    old: absent | { hash, backup_name },
    new: absent | { hash, stage_name }
  }
]
immutable_assets: [{ role, hash, validated_name }]
monotonic_checkout_identity_actions: [{ observation_id, planned_id }]
created_at
committed_at?
```

Names are validated basenames generated by the owner. They cannot contain path
separators or caller data. Roles derive targets in code. A regular transaction
has exactly the catalog and attachment participants. A migration additionally
uses the closed roles in section 6.

Transaction order:

1. hold `projects.json.lock` and re-open or recover current state;
2. serialize canonical post-images, validate the complete plan, and compute
   hashes;
3. write and fsync every immutable asset;
4. write and fsync verified backups for every present old mutable image;
5. write checksum-named mutable stages and fsync their directories;
6. atomically write and fsync the prepared journal and its parent;
7. execute any journaled monotonic checkout-id preparation from section 6;
8. install every mutable participant in code-owned role order, fsyncing each
   parent;
9. verify all installed hashes, epochs, and cross-store invariants;
10. atomically write the committed journal and fsync its parent;
11. publish the immutable in-memory pair; and
12. retain backups, journal, marker, and pinned immutable assets until bounded
    cleanup proves the committed state and final closeout permits removal.

Phase 1 cleanup may remove only artifacts from an older committed transaction
whose installed participants still match that transaction's new hashes. It
never removes the v1 migration backup, legacy publisher-ref backup, prepared
journal, committed marker, pinned G1 or quarantine assets, or the newest
committed recovery evidence. External storage GC excludes the transaction
stage and backup roots.

### 5.5 Recovery classification

Recovery classifies each mutable participant independently as `old`, `new`,
`other`, or `missing`, and verifies staged, backup, and immutable-asset hashes.

Forward recovery is allowed only when, for every participant, the new post-image is
either already installed or available in its verified stage. It installs any
remaining staged target, verifies the complete plan, and commits the journal.

Rollback is allowed only when, for every participant, the old image is either
still installed or available in its verified backup. It restores present old
images, removes an `old = absent` target only when its bytes exactly match the
transaction-created new hash, verifies the complete old state, records a
rolled-back recovery result, and leaves evidence for inspection. Successfully
installed planned checkout-id markers are monotonic bridge-compatible
preparation and are never deleted or reminted on rollback.

If neither a complete forward set nor a complete rollback set exists, open
fails closed with `error.project_catalog_recovery_incomplete`. It never chooses
an old/new mixture, never regenerates missing bytes, and never deletes the
evidence. A v2 catalog produced by migration without a valid marker or
recoverable prepared journal fails with
`error.project_catalog_migration_incomplete`.

### 5.6 Fault injection

Use an injected `CatalogStoreIo` trait in tests, with production
`RealCatalogStoreIo`. The trait is private to `bbox-indexing` and covers only
the protocol's filesystem operations. Tests fail before and after every:

- backup write/fsync;
- staged write/fsync;
- directory fsync;
- prepared-journal write;
- every participant install or deletion;
- every immutable-asset verification;
- every monotonic checkout-id action;
- complete-plan verification;
- committed-journal write; and
- cleanup operation.

After each injected failure, reopen with real I/O and assert exactly one
coherent state, matching hashes, no mixed participant set, and the documented
retained evidence. P1-B runs this matrix for regular empty and nonempty pair
transactions, including a catalog scope-migration record paired with its
attachment proof and legacy path binding. P1-C repeats it with every migration
participant and immutable asset.

### 5.7 P1-B gate

In addition to P1-A gates:

- run the complete transaction fault matrix;
- run concurrent stale-epoch writers and prove one succeeds;
- prove preflight and the bridge `StorePersister` contend on the same
  `projects.json.lock`;
- prove a live v1 bridge lifetime lock prevents exclusive apply;
- run the stable-signed isolated bridge bootsmoke; and
- assert the bridge created no v2 strict files.

Commit and push P1-B only after the smoke passes.

## 6. Milestone P1-C: migration inventory, resolution, and apply engine

### 6.1 Modules

Add:

- `crates/bbox-indexing/src/project_catalog_migration.rs`;
- `crates/bbox-indexing/src/project_catalog_inventory.rs`; and
- `crates/bbox-indexing/src/accepted_publication_store.rs`;
- migration-only `CollisionRetirementLifecycleV1` persistence in
  `bbox-code-source-store`; and
- versioned fixture trees under
  `crates/bbox-indexing/tests/fixtures/project-catalog-migration/`.

The migration engine consumes explicit paths and immutable inventory inputs. It
does not read global environment variables. The thin CLI resolves configured or
explicit paths and passes them in.

P1-C exposes one high-level public facade for preflight, rehearsal apply, and
complete migration-aware verification. The facade accepts explicit resolved
paths and typed options and returns typed results and errors. It owns assembly
of the complete participant registry and is the only public entry point that
may execute or verify a migration transaction. P1-D must not expose individual
participant internals or reconstruct the registry in the CLI.

The public executable surface is
`project_catalog_migration::ProjectCatalogMigrationFacadeV1` with exactly
three operations:

```text
preflight(ProjectCatalogMigrationPreflightRequestV1)
    -> ProjectCatalogMigrationPreflightResultV1
apply_rehearsal(ProjectCatalogMigrationApplyRequestV1)
    -> ProjectCatalogMigrationApplyResultV1
verify(ProjectCatalogMigrationVerifyRequestV1)
    -> ProjectCatalogMigrationVerifyResultV1
```

Requests carry non-serializable, already-resolved typed layouts plus explicit
report, resolution, and optional sensitive-report artifact paths. They never
carry caller-decoded reports, caller-built observation rows, participant
drafts, or a boolean claiming that a path is safe. The facade itself:

- opens every owner store and captures every owner lane;
- decodes, validates, and writes the bounded persisted artifacts;
- constructs deterministic report and post-image inputs;
- owns the complete `MigrationParticipantRegistry`;
- is the only caller of `validate_migration_plan`, `transact_migration`, and
  `ProjectCatalogStore::open_existing_after_migration`; and
- returns path-redacted receipts while retaining any compatibility paths only
  in a non-serializable host-local projection.

The lower inventory capture facade becomes crate-private. Inventory runtime
bindings, owner snapshots, participant roles and drafts, the registry,
validated plans, and migration-aware store open remain crate-private. A
consumer outside `bbox-indexing` can complete preflight, rehearsal apply, and
fresh verification without importing any of them.

The one public `ProjectCatalogMigrationError` preserves stable underlying
inventory, adapter, store, and lock error codes where they are already
specific. Facade-only codes cover unsafe or overlapping layout, bounded
artifact I/O, noncanonical or stale artifact identity, missing owner adapter,
compatibility join failure, and installed-state mismatch. Its public message is
control-free, bounded to 512 bytes, and contains no source path or private row
value. Apply errors also carry one typed mutation disposition:
`NoDurableMutation`, `RecoveredToOldState`, `RecoveredToCommittedState`, or
`RetryExactPlanRequired`. The CLI never classifies errors by matching strings.

`Clean`, `ResolutionRequired`, and `Refused` are successful preflight domain
results. The result states the status and bounded counts; P1-D maps those typed
statuses to its documented exit policy. Apply accepts only `Clean`.

### 6.1.1 Resolved source and rehearsal layouts

`ProjectCatalogMigrationResolvedLayoutV1` is an opaque, validated,
non-serializable path bundle. Its constructors are:

```text
from_config(config, { projects_path?, state_dir? })
from_rehearsal_root(root, config)
```

The bundle contains the exact projects path, code-source root,
publisher-reference source, index and vector roots, edge manifests, Git and
checkout inputs, knowledge/gap/coordination stores, artifact and provenance
roots, accepted-publication paths, every transaction/backup/stage/marker/GC
root, and the exact configured `StoreLimits`. The shared
config-to-`StoreLimits` conversion lives in `bbox-indexing` and is used by both
daemon startup and this facade; neither side carries a private duplicate.
Any current owner location that is still derived privately at open time
(including vectors, edge manifests, Git metadata, and the provenance-notes
ref) becomes an explicit resolved config/layout field first. Migration never
calls `dirs::*`, guesses a ref, or opens a create-on-read default to discover
an owner.

An explicit `--state-dir` re-roots the complete conventional source bundle,
including publisher refs at `<state-dir>/bro/publisher-refs.json`. An explicit
`--projects-path` overrides only the projects member and wins when both are
present. With neither override, every resolved config path is retained,
including publisher refs under the configured `bro_home`. P1-D only chooses
one constructor; it never derives individual participant paths.

`from_rehearsal_root` derives one fixed relative layout under the supplied
root: state-owned stores live under `state/`, publisher refs under
`state/bro/`, and checkout replicas under `checkouts/`. Paths already derived
from the projects path, including attachments, accepted publications,
journal, stage, backups, marker, and locks, retain their code-owned sibling
layout below `state/`. The exact relative path table is one code-owned constant
tested against daemon path resolution; it is not repeated in the CLI.

The facade never copies configured live state. A rehearsal root is an
operator- or test-prepared isolated v1 bundle, and preflight must be rerun
against that copy before apply. A report captured from configured live paths
is diagnostic only during Phase 1 and cannot authorize an isolated apply.
Apply receives both the isolated layout and the protected configured layout.
It canonicalizes every existing parent, opens sources and artifacts no-follow,
rejects symlinks and non-regular files, and proves that every mutable source,
participant, checkout root, immutable asset, backup, stage, marker, lock, and
GC root is contained by the rehearsal root and disjoint from every protected
live path and source authority. Canonical ancestor, descendant, inode-alias,
and symlink-alias overlap all refuse. The same planner, registry builder,
transaction owner, recovery, and verifier run in rehearsal and Phase 6; only
the validated layout differs.

### 6.1.2 Facade artifacts and result algebra

Preflight always receives an explicit resolution path. If that path is absent,
the facade atomically creates the canonical
`ProjectCatalogMigrationResolutionV1::empty(inventory_hash)` after inventory
capture. If it exists, the facade bounded-no-follow reads and strictly decodes
it. A present zero-byte file is invalid. This gives conflict-free migrations a
real persisted resolution artifact without asking P1-D to synthesize one.
After an operator edits a resolution, preflight is rerun and writes a new
report bound to those exact resolution bytes.

The report records the resolution artifact SHA-256. Preflight atomically writes
the report itself through a descriptor-bound no-follow parent, owner-only temp
file, file fsync, rename, and directory fsync. Apply bounded-no-follow reads the
report and mandatory resolution itself. It validates decoded semantics and
also binds the exact report and resolution byte hashes into the transaction
journal, migration marker, apply receipt, and verification receipt. It never
reruns preflight, remints an identity, normalizes an artifact behind the
operator's back, or substitutes regenerated bytes. Reapplying a completed plan
requires the same exact artifact hashes.

The serializable redacted receipts include version, domain status or outcome,
transaction id, inventory and plan hashes, exact report and resolution hashes,
expected and observed catalog/attachment/participant/immutable-asset hashes,
epoch, bounded role counts, backup hashes, checkout-action count, publisher
pin count, quarantine-root count, attached-project count, and omitted-catalog
count. Apply outcome is `Applied` or `AlreadyApplied`.

`ProjectCatalogMigrationVerifyResultV1` itself is not serializable. It exposes
the serializable redacted `MigrationVerificationReceiptV1` used by P1-D and a
separate host-local compatibility projection used by tests. The compatibility
projection contains paths and therefore can neither enter the CLI envelope nor
be written by the default report path.

### 6.2 Stable v1 inventory

Preflight acquires the shared lifetime lock and `projects.json.lock` while
capturing the project store. Other stores are captured through immutable
snapshots and exact byte hashes, all of which apply rechecks after acquiring
their role locks. It then builds one canonical
`V1ProjectCatalogInventory` containing:

- every `LegacyProjectRecordV1`, including missing paths;
- committed recorded authority when available through an explicit
  read-authorized probe;
- code-source activation, retained generation, descriptor, manifest, exact
  active/collision selectors, typed selector absence for ordinary retained
  rows, and quarantine summaries;
- exact `PublisherRefStore` source bytes and one row per pin with scope, full
  ref, candidate attachment ids, resolved commit, resolved scope, and
  observation ids;
- project ids and project-scoped refs found in Tantivy and vector metadata;
- every materialized commit namespace with a complete streamed commit-document
  count and canonical ordered commitment, plus the matching vector-key count
  and commitment, without embedding document bodies in inventory/report;
- edge workspace manifests and active selectors;
- Git metadata, every materialized commit namespace, and every legacy
  per-project `last_ingested_sha` cursor;
- checkout-id marker state once per canonical checkout root as valid,
  missing-or-empty, malformed, unreadable, or symlinked;
- project-scoped artifact and provenance targets;
- materialized aliases and registration timestamps; and
- one typed `LegacyPathObservation` per durable project-scoped coordination
  row, containing store kind, stable row id, and the bounded literal
  project/path selector needed for inventory-time deepest-root classification.
  Counts are derived from these observations.

Inventory adapters return typed observations. They do not mutate stores,
refresh indexes, infer authority from origin URLs, or dump private content into
the report. Reports use generic record ids, hashes, counts, and bounded
diagnostics. Public fixtures use only neutral synthetic names.

The facade, not its caller, opens and snapshots the required owners. The ten
`ImmutableInventoryLaneKindV1` lanes are a closed completeness set:

- Tantivy and vector owners enumerate every durable project-scoped entity ref;
- the edge-manifest owner supplies workspaces and active selectors;
- checkout/Git adapters supply canonical checkout identity, exact Git common
  directory and first-commit evidence, materialized namespaces, resolved refs,
  and legacy cursors;
- the legacy project owner and checkout evidence produce attachment candidates
  and already-materialized aliases;
- artifact and provenance owners enumerate exact project-scoped targets;
- the coordination-store owners enumerated by the closed
  `LegacyPathStoreKindV1` set enumerate every durable legacy path selector; and
- repository grouping and legacy namespace clusters are derived only from the
  preceding exact owner evidence.

`LegacyPathStoreKindV1` names actual owning surfaces, not entry subtypes:

```text
Knowledge | Gap | Thread | Note | Pin | Roadmap | Packet | Task | Proposal
| SlackBinding | Whiteboard | Artifact | Provenance | TranscriptEdge
```

Decision and memory are knowledge entry kinds and remain visible in the stable
row id/evidence, not fictional physical stores. There is no `Goal` owner. A new
owner surface cannot be silently folded into an existing kind; adding one
requires a versioned inventory change and parity update.

Every owner adapter captures under that owner's read/role lock, returns a
bounded immutable snapshot with source fingerprint and row-set commitment, and
releases the lock before cross-owner planning. Composite lanes additionally
contain a canonical nonempty `OwnerSubsourceEvidenceV1` list with owner kind,
stable source id, typed present/missing/corrupt state, byte/count bounds,
content fingerprint, and row-set commitment for every constituent store. The
lane aggregate is derived from that exact list. One aggregate lane hash can
never stand in for proof that each owner was visited.

Missing or corrupt owners return typed subsource/lane evidence and make the
report `Refused`; they never collapse into an empty complete lane. Apply
recaptures all lanes through the same owner APIs and requires the same
subsource list, fingerprints, and row commitments before plan validation.
Where an owner crate lacks a read-only, no-create strict snapshot API, P1-C
adds it to that owner; neither the facade nor P1-D parses the owner's private
files ad hoc. Root-only wire schemas move to a dependency-safe leaf before the
adapter is added. The hard-coded `owner_lane_unsupported` placeholder is a
fail-closed staging state, not an acceptable P1-C result.

The Tantivy/vector owner snapshots also produce
`LegacyCommitNamespaceInventoryV1` for every exact commit namespace. Each row
binds source schema/fingerprint, complete commit-document count and canonical
ordered row commitment, vector-key count and commitment, and the later proved,
ambiguous, or unclaimed attribution. The marker and verification receipt
retain these commitments as Phase 3 readiness evidence. Phase 1 does not copy
commit-document bodies, create `RepoHistoryGeneration`, or add history assets
to the migration participant registry. Phase 3's reviewed pre-replacement
materializer is the sole owner of those immutable generations and must
reproduce the Phase 1 commitments before replacing an index.

Literal legacy selectors are migration inputs, not canonical inventory or
default report fields.
`legacy_path_bindings` report rows contain observation id, store kind,
relationship/status, and a domain-separated path digest. The complete bounded
literal appears only in a non-serializable host-local runtime binding set and
the strict host-local attachment post-image. Each runtime binding is paired
one-to-one with the digest in canonical inventory. Apply recaptures both under
the same owner locks and verifies the pairing before constructing post-images.
When an operator must resolve an ambiguous row, the local CLI may display it
interactively or write an explicitly requested `--include-local-paths` review
report using no-follow creation, owner-only permissions, a
`local_paths_included: true` warning, and no stdout echo. Such a report is
host-local sensitive state, not canonical inventory, and must never be
committed. The default report, canonical inventory JSON, and every public
fixture remain path-redacted.

The inventory hash is SHA-256 over a versioned domain separator plus canonical
inventory JSON. File mtimes and directory enumeration order are excluded.
An active or retained generation with a missing or corrupt immutable descriptor
is refused with the exact bridge-v1 repair/retire instruction. Resolution
cannot invent its scope.

Code-source inventory is memory-bounded without imposing a lifetime
cardinality cap on a valid store. Under the owner store's lock, a streaming
radix walk orders validated `scope-hash/generation-id` keys lexically without
trusting filesystem enumeration order or writing scratch state. It feeds a
domain-separated sequential SHA-256 commitment and row count for the complete
legacy generation namespace. Per-row size and validation limits are checked
before allocation or replacement, and the inventory adapter receives the
owner store's configured `StoreLimits`; it never substitutes defaults.

The adapter retains full row evidence only for effective roots and generations
selected by that same owner policy as retained for catalog, activation, or
collision-lifecycle scopes. Historical and orphan rows remain represented by
the complete-set commitment and bounded counts but are non-surviving GC
candidates: they are never migration participants, selectors, or authority.
Apply and recovery rerun the same commitment and classifier under the lock.
Current v2 validation streams lifetime history and retirement state through the
same classifier. Mixed v1/v2 stores may contain only unprotected,
non-selectable v1 leftovers; a protected v1 row without exact strict-v2
project/scope ownership refuses startup. Maintenance and GC use this classifier
too, so they cannot reinterpret an omitted row as authority.

Preflight plans and persists every strong-random value that will enter a
predicted post-image:

- one migration transaction id;
- one repository-history id for each surviving history group;
- one attachment id for each retained attachment candidate;
- one legacy-path binding id for each retained ledger row;
- one local commit namespace for each history group that requires local
  authority and has no inventoried materialized namespace; and
- one checkout id per eligible missing-or-empty marker, shared by all monorepo
  attachments on that canonical root.

The catalog post-image uses the exact transaction id in
`CatalogOriginV2::MigratedV1`. Reported repository-history groups carry their
planned history id, primary namespace, and compatibility namespaces. Reported
attachment rows carry their planned attachment id. Reported legacy-path
binding rows carry their planned binding id. Apply never remints or substitutes
any of these values. The adapter cannot derive an opaque attachment id from a
path, digest, observation order, or checkout id. A `LegacyLocal` project with
no inventoried history evidence receives no repository-history record; when
such evidence requires a local-authority history and supplies no materialized
namespace, preflight uses the persisted planned local namespace.

Migrated `CorpusProject.created_at` and every corresponding
`CheckoutAttachment.attached_at` preserve that legacy project's exact
`registered_at` value. No wall-clock value may enter a predicted post-image;
any future timestamp-bearing migration participant must use a persisted planned
value or exact inventoried source value.

The planned identities, inventory, resolution, and all predicted post-images
form one canonical `plan_hash`. Generating a new report may generate a different
plan, but apply consumes one exact persisted report and never substitutes a new
id.

### 6.3 Evidence and grouping

Preflight groups records into one repository-history candidate only with one of
the governing proofs:

- identical committed recorded authority;
- shared canonical Git common directory plus matching full first commit; or
- immutable collected descriptor/activation authority agreement for a missing
  checkout.

Weak namespace equality, path hash, origin URL, alias, computed repo hint,
directory name, or request order never proves sameness.

Every group records the exact evidence class and source observation ids.
Monorepo projects with proved same-repo evidence share one history candidate but
retain distinct published scopes.

### 6.4 Preflight report

`ProjectCatalogMigrationReportV1` includes:

```text
version
transaction_id
inventory_hash
plan_hash
resolution_artifact_hash
source_store_hash
publisher_ref_source_hash
generated_at
status: clean | resolution_required | refused
projects
repo_history_groups
attachments
checkout_identity_actions
legacy_path_bindings
namespace_conflicts
scope_conflicts
alias_conflicts
activation_conflicts
publisher_bindings
publisher_binding_conflicts
predicted_g1_assets
predicted_accepted_pointer_hashes
missing_paths
unscoped_legacy_counts
required_resolutions
predicted_catalog_hash
predicted_attachment_hash
predicted_participant_hashes
```

`clean` means apply can produce the exact predicted post-images without operator
judgment. `resolution_required` means every conflict has a supported bounded
resolution shape. `refused` means the inventory is corrupt, incomplete, or
contains a conflict no resolution schema may override.

Preflight writes no project, checkout marker, attachment, activation, index,
vector, edge, knowledge, gap, or coordination state. Writing the explicit
report and, only when absent, the explicit resolution path are its only default
mutations and use no-follow atomic replacement. Apply always requires those
exact persisted artifact bytes, even for an empty or conflict-free inventory.

An explicitly requested sensitive review artifact is a separate third
possible preflight write. Its typed path must differ from report, resolution,
source, and every owner path. It contains
`local_paths_included: true`, a fixed warning, digest-paired legacy selector
rows, and digest-paired attachment/checkout rows. It is built only from the
opaque runtime bindings after complete one-to-one validation, created below a
no-follow owner-only directory, and atomically replaced at mode `0600`.
Preflight returns only its hash and row counts. No serializable default result,
canonical inventory, public fixture, or committed artifact contains its path
or literals.

### 6.5 Resolution file

`ProjectCatalogMigrationResolutionV1` contains:

```text
version
inventory_hash
selected_scope_owners
repo_history_group_merges
repo_history_group_splits
excluded_attachments
quarantine_collected
publisher_binding_dispositions
operator_notes
```

Preflight and apply both receive the resolution artifact path. On the first
preflight only, absence means "create and use the canonical empty resolution
for this freshly captured inventory"; it never means "no resolution artifact".
Once present, the file must be a nonempty strict v1 document. Apply refuses a
missing or zero-byte file and never synthesizes an empty resolution.

A resolution whose inventory hash is stale is never rewritten or
automatically carried forward. The operator retains or moves that local
artifact for comparison, selects a new absent resolution path, reruns
preflight to create the canonical empty document for the new inventory, and
re-enters only dispositions that the new report still names with the same
typed candidates. Preflight emits bounded old/new inventory hashes and stable
resolution ids to support that review, but no disposition crosses inventories
without explicit operator authorship.

Unknown resolution keys, unknown record ids, stale inventory hashes, duplicate
dispositions, incomplete dispositions, and attempts to remint/reassign a
preserved project id fail closed.

A publisher disposition is exactly one of:

```text
SeedG1 {
  project_id,
  attachment_id,
  expected_scope,
  full_ref,
  accepted_commit,
  generation_id,
  payload_hashes,
  pointer_hash
}
NoPublishedContentAcknowledged {
  project_id,
  expected_scope,
  full_ref,
  bounded_reason
}
```

A unique legacy publisher may generate `SeedG1` automatically. Ambiguous or
missing candidates require a resolution selecting one inventoried attachment
or explicitly acknowledging no published content. A resolution can also select
a survivor for duplicate published scope, split an ambiguous weak namespace,
exclude an unprovable attachment, or explicitly quarantine a losing collected
generation. It cannot:

- rewrite or redirect existing entity refs;
- turn a computed hint into recorded authority;
- relabel immutable collected bytes;
- accept an alias conflict without choosing one owner;
- invent a publisher attachment, ref, commit, or scope not proved by inventory;
- merge projects without stronger same-repo evidence; or
- suppress an inventory class.

Preflight is rerun with the resolution and must become `clean` before apply.

### 6.5.1 Migration participant codecs

`accepted_publication_store.rs` owns strict, capped, deny-unknown-fields
codecs for:

```text
AcceptedPublicationGenerationV1 {
  project_id,
  scope,
  full_ref,
  accepted_commit,
  knowledge_file_manifest,
  gap_file_manifest,
  normalized_knowledge,
  normalized_gaps,
  hashes,
  counts,
  total_encoded_bytes
}

AcceptedPublicationPointerV1 {
  project_id,
  attachment_id,
  full_ref,
  accepted_commit,
  accepted_scope,
  accepted_generation,
  generation_hash,
  prior_pointer?
}
```

The store derives immutable generation and mutable pointer paths from validated
project and generation ids. A canonical file manifest is keyed by normalized
repository-relative filename and contains the content hash and normalized entry
or tombstone input needed by later overlay and promotion logic. It contains no
checkout path and no Git ancestry. Phase 1 exposes strict verify and migration
write APIs only; publisher advance and query integration remain Phase 5.

`bbox-code-source-store` adds a strict
`CollisionRetirementLifecycleV1` document keyed by validated project id. It
contains a bounded canonical `BTreeMap<GenerationId,
CollisionRetirementEntryV1>`. Every entry contains former scope,
`selector_evidence`, snapshot and manifest hashes, inventory hash, plan hash,
and a typed `Pending`, `Queued`, or `Completed` state. `selector_evidence` is
exactly `ExactMaterialized(selector)` for an active loser or
`NoDurableSelector` for a retained-only loser.

The migration transaction atomically installs the complete entry map for every
active and owner-policy-retained generation of the losing project while its
complete manifest participant removes any losing active row. Entry evidence
and membership are immutable after install; only monotonic state transitions
are allowed. Each entry has a subordinate collision-retirement work row with a
code-derived id over project and generation. `Pending` publishes or verifies
that row before advancing to `Queued`; `Queued` requires or recreates the same
row; physical completion is invoked by project/generation identity and
atomically installs the durable `Completed` entry before removing the matching
work row. An active entry's exact selector is a deletion target. A
retained-only entry never acquires selector authority during cleanup. A
matching lagging work row beside `Completed` is tolerated and cleaned
idempotently; contradictory, duplicate, missing, or regressed state fails
closed. Only Pending/Queued entries keep their immutable generations as
journal/marker/lifecycle GC roots; Completed entries remain terminal receipts.

The same crate adds strict `ActivationRecordV2` and `StoredGenerationV2`
metadata with explicit `published_scope` and no serde default. Migration
backfills active scope only when the legacy immutable generation descriptor,
manifest, activation, exact effective materialized selector, and migrated
published catalog project agree. An ordinary retained generation without an
activation is instead bound through the exact owner-locked retention set,
descriptor, manifest, generation id, project, and scope, and records typed
`NoDurableSelector`; the adapter never invents a `:m<16hex>` suffix. Ambiguous
retained scope ownership yields a bounded resolution conflict carrying its
candidate set. Each rewritten metadata file is a code-owned migration
participant. A losing collision writes no active v2 record; its former scope
and per-generation typed selector evidence live only in
`CollisionRetirementLifecycleV1`. Active losers require an exact materialized
selector; every retained-only loser requires its own `NoDurableSelector` entry
and exact project/generation identity. First v2 startup rejects scopeless
metadata instead of inferring scope from project id.

### 6.6 Deterministic post-image construction

For each legacy record:

- preserve the exact `project_id`;
- preserve `registered_at` in `registered_at_compat`;
- preserve detected languages;
- migrate materialized aliases to accepted operator aliases;
- use committed recorded scope or a fully agreeing active collected descriptor
  for `Published`;
- otherwise create `LegacyLocal`;
- create an attachment only after canonical path, planned or existing checkout
  identity, and scope validation; and
- never drop a missing path.

Repository-history selection follows the governing primary-namespace rules.
Colliding unproved namespaces become typed ambiguity records and are excluded
from ordinary resolution.

No legacy per-project Git cursor seeds a consolidated repo-history cursor. The
plan inventories and backs up those SHA values, creates the repo-history record
without a cursor, and requires the later Git-overlay phase to publish one full
reachable-history generation before recording a new cursor.

The deterministic post-image input carries the report's exact migration
transaction id, planned repository-history assignments, planned legacy-path
binding ids, planned local namespaces, and checkout identity actions. The
builder rejects any catalog origin, transaction draft, history record, ledger
row, or local namespace whose id differs from that persisted value. It emits
one transaction plan containing:

- catalog and attachment post-images;
- the complete mapped, unscoped, and quarantined legacy path ledger inside the
  attachment post-image;
- the complete effective source-manifest post-image;
- strict scope-bearing activation and retained-generation metadata post-images
  for every surviving collected generation;
- one typed `CollisionRetirementLifecycleV1` document containing the complete
  Pending entry map and removal of any corresponding legacy activation;
- every accepted-publication pointer post-image;
- the migration marker;
- immutable G1 knowledge/gap generation assets with canonical relative-filename
  manifests and hashes; and
- every planned monotonic checkout-id action.

A `quarantine_collected` disposition removes the losing workspace selector
from the prospective manifest and preserves its immutable generation as a
journal and marker GC root. Phase 1 may defer physical Tantivy, vector, and
edge deletion, but it does not defer the authority cut. The loser is absent
from effective selection in the exact state the later v2 runtime opens.

Every legacy publisher pin yields one G1 pointer or one explicit
`NoPublishedContentAcknowledged` disposition. G1 stores accepted commit P and
canonical knowledge and gap file manifests, but no invented Git ancestry.
Exact `publisher-refs.json` bytes and checksum remain rollback-pinned, and v2
never consults them.

### 6.7 Apply engine

The engine:

1. requires the exclusive lifetime migration lock;
2. captures each mutable source under its short role lock, releases those
   locks, and reruns inventory from the exact bytes;
3. prepares Git/content-derived G1 assets through explicit read leases without
   holding a store lock;
4. requires the exact persisted report, migration transaction id, resolution,
   inventory hash, and `plan_hash`;
5. requires a clean report and one disposition for every publisher pin and
   collision;
6. builds and validates every post-image in memory;
7. acquires `projects.json.lock` and auxiliary role locks in declared order,
   then rechecks every captured byte hash and lease fingerprint;
8. gives the complete participant and asset plan to the P1-B transaction owner;
9. writes and fsyncs immutable assets, v1 and publisher-ref backups, mutable
   stages, checksum metadata, and the prepared journal;
10. installs missing markers with no-follow create-if-absent, accepting only the
   planned id and retaining successful markers on rollback;
11. installs and verifies every mutable participant, including the migration
    marker, before committing the journal; and
12. reopens and validates every v2 participant and pinned asset before success.

The marker contains the transaction id, plan hash, exact report and resolution
artifact hashes, source and inventory hashes, all participant post-image
hashes, G1 asset hashes, collision quarantine pins, complete legacy
commit-namespace document/vector commitments for the later history
materializer, epoch, and retained backup hashes. The engine is idempotent only
for the same completed marker, plan, and exact artifacts. Different source
bytes, report bytes, resolution bytes, planned checkout id, or predicted hashes
refuse. Pair-installed but marker-absent state recovers only through its
prepared journal; without one it fails
`error.project_catalog_migration_incomplete`.

Phase 1 CLI apply requires an explicit isolated rehearsal root different from
the configured live projects path. The guard compares canonical parent and
target paths, not a caller-supplied boolean. The engine itself is the same code
Phase 6 later activates for the configured path. Rehearsal redirects every
participant, legacy source, checkout fixture, and GC root to isolated copies;
it changes destination, not transaction semantics.

Facade verification is a fresh reopen, never an inspection of the in-memory
apply result. `verify` derives the fixed layout from the rehearsal root,
bounded-no-follow reads only enough journal and attachment evidence to recover
the observation-id-to-checkout-root registry, requires a unique root-contained
mapping, rebuilds the complete registry, and then invokes migration-aware
strict open. Provisional bytes grant no authority: success requires the
subsequent marker, journal, pair, participant, asset, and source verification
to authenticate them exactly.

The verifier checks:

- the strict catalog and attachment pair and epoch;
- a committed terminal journal and matching migration marker;
- transaction, plan, inventory, report, and resolution identities;
- every expected and observed mutable participant hash;
- every immutable G1 asset and accepted-publication pointer;
- publisher backups and retained rollback backup hashes;
- checkout markers and their exact planned ids;
- effective source manifest, scope-bearing activation and retained metadata,
  collision retirement state, and every pending GC root; and
- predicted versus installed catalog, attachment, participant, and asset
  hashes.

Its receipt distinguishes `Committed` from `AlreadyApplied`. Any ambiguous
journal-to-checkout correlation, missing registry role, incomplete recovery
set, or installed mismatch fails closed; P1-D never reopens internals to
manufacture a verification result.

### 6.8 Migration tests

Fixture and property tests cover:

- empty v1 store;
- an external-consumer integration test completing preflight, rehearsal apply,
  idempotent reapply, and fresh verify through only the three public facade
  operations;
- all ten owner lanes as complete, missing, corrupt, reordered, and changed
  snapshots, proving the staging `owner_lane_unsupported` path is gone;
- every composite lane with one omitted, duplicated, changed, missing, or
  corrupt owner subsource, proving an aggregate lane hash cannot launder
  incomplete capture;
- complete per-namespace commit-document and vector-key commitments for
  proved, ambiguous, and unclaimed legacy namespaces, including refusal on a
  changed/omitted row and proof that Phase 1 created no history-generation
  asset;
- exact report/resolution byte binding, noncanonical or zero-byte artifact
  refusal, no-follow atomic writes, and same-plan/different-bytes refusal;
- every rehearsal/live canonical ancestor, descendant, inode-alias, and
  symlink-alias overlap, plus proof that facade rehearsal never copies state;
- separately invoked preflight and apply reproducing identical post-image
  hashes for repository-history records, legacy-path binding rows, and a
  `LegacyLocal` project whose inventoried history requires a planned local
  namespace;
- Git, non-Git, monorepo, shallow, missing, and moved projects;
- duplicate ids, scopes, aliases, and weak namespaces;
- planned random attachment ids reproduced across separate preflight/apply,
  plus refusal of path-derived, order-derived, reminted, or missing ids;
- cross-project duplicate active `(checkout_id, project_root_relpath)`
  candidates represented as attachment conflicts and excluded before install;
- well-formed legacy aliases preserved exactly and invalid length,
  whitespace, separator, percent, or control-character aliases hard-refused
  with only a digest in the default report;
- same-repo and false-same-repo evidence;
- active and retained collected generations;
- complete generation history beyond reduced row and aggregate-byte limits,
  proving bounded streaming, ordered commitment stability, and that historical
  or orphan rows do not survive;
- refusal when a protected generation is omitted or lacks exact owner/scope
  evidence;
- ordinary retained generations and retained-only collision losers with typed
  selector absence, active and active-collision rows with exact materialized
  selectors, and refusal of any fabricated retained selector;
- duplicate-scope retained rows represented as resolution-required candidate
  sets rather than prematurely assigned or dropped;
- descriptor/activation/manifest disagreement;
- legacy scopeless active/retained metadata rewritten from exact descriptor
  agreement, plus every missing or ambiguous scope join refusal;
- publisher pins with unique, missing, and ambiguous candidates;
- G1 knowledge/gap seeding, no-content acknowledgement, and exact legacy
  publisher-ref backup;
- missing/corrupt index, vector, edge, artifact, and coordination inventory;
- accepted resolution, stale resolution, and unsupported override;
- losing collected state with and without quarantine disposition;
- collision lifecycle crash/retry at `Pending` to `Queued` and `Queued` to
  `Completed`, durable terminal-receipt validation, and idempotent cleanup of a
  matching lagging queue row;
- retained-only collision retirement keyed by exact project/generation
  identity without manufacturing selector authority;
- one losing project with an active generation and multiple retained
  generations, proving complete lifecycle membership, independent entry
  transitions, and immutable terminal receipts;
- production startup reconciliation of selector-backed and selectorless
  Pending/Queued entries through code-derived project/generation work ids;
- mixed v1/v2 maintenance and GC, proving unprotected leftovers remain
  non-selectable and protected scopeless legacy state refuses;
- lock overlap with a live bridge registry;
- contention with a bridge `StorePersister` on `projects.json.lock`;
- missing marker planning and materialization, a shared monorepo checkout root,
  existing matching and different ids, malformed/unreadable/symlinked markers,
  and crash/retry at every marker boundary;
- source mutation between preflight and apply;
- every P1-B participant and immutable-asset crash point;
- pair installed before marker, marker installed before committed journal, and
  incomplete forward/rollback sets;
- fresh-v2 open without a marker, migrated-v1 open with a matching marker, and
  migrated origin with a missing or mismatched marker;
- idempotent completed apply;
- backup and marker integrity;
- reports containing no absolute checkout path unless an explicitly local
  attachment diagnostic is requested;
- default legacy-binding rows exposing only path digests, plus an explicit
  `--include-local-paths` report with owner-only permissions and the sensitive
  marker;
- canonical inventory JSON containing no absolute path or literal legacy
  selector, with non-serializable runtime bindings recaptured and digest-joined
  under the same owner locks; and
- complete mapped/unscoped classification from typed literal observations
  without a second live-store read.

Compatibility fixtures obtain their path-bearing rows only from the facade's
host-local verification projection. For every attached migrated v1 project
they compare exact project id, canonical path, Git/non-Git classification,
languages, accepted aliases, and registration timestamp through the existing
cross-validated `ProjectRecord::from_catalog_attachment` join. Exactly one
attached base row is allowed per migrated legacy project. Zero attached rows
means remote/missing and increments the explicit omitted-catalog count; more
than one refuses. Nominated aliases never resolve. The serializable receipt is
scanned to prove none of these paths escaped.

The final report test scans all string fields for known fixture tokens that
represent credentials or private identifiers and fails if any leak.

### 6.9 P1-C gate

- run targeted migration and transaction nextest suites;
- from an integration test compiled as an external `bbox-indexing` consumer,
  run facade preflight on an operator-prepared neutral v1 rehearsal fixture
  containing a markerless checkout, a publisher pin, and a collected
  collision;
- run facade rehearsal apply with the exact report/resolution artifacts, then
  run a separately opened facade verify;
- verify predicted and installed hashes match;
- verify the exact planned checkout marker, G1 pointer, effective manifest,
  scope-bearing activation/retained metadata, retirement record, and marker
  hashes;
- rerun facade apply and prove `AlreadyApplied` with exact artifact identity;
- prove every configured-live overlap refuses and the live bundle is
  byte-unchanged;
- run the stable-signed bridge bootsmoke against fresh v1 state;
- confirm the live bridge state is unchanged by rehearsal; and
- commit and push P1-C, then cluster-verify that exact pushed ref.

The `blackbox` executable does not exist until P1-D. P1-C never claims a CLI
gate; P1-D repeats this exact facade rehearsal through the thin executable.

## 7. Milestone P1-D: offline CLI, compatibility proof, and later-phase handoff

### 7.1 `blackbox` executable

Add a root-package binary at `src/bin/blackbox.rs` and an explicit Cargo binary
entry. Use the root package's existing `clap` dependency.

Commands:

```text
blackbox project-catalog migrate --preflight --report <path> --resolution <path>
blackbox project-catalog migrate --apply --report <path> --resolution <path> --rehearsal-root <path>
blackbox project-catalog verify --root <path>
```

Common options include explicit projects path, state dir, report, and
resolution. Both preflight and apply require `--resolution`; first preflight
may create the canonical empty artifact at that explicit path, while apply
requires the existing exact report/resolution pair and refuses unless the
report is clean. Preflight alone accepts
`--include-local-paths <sensitive-report-path>`. Defaults use the same config
loader as the daemon, but help and version remain side-effect-free.
`--preflight` is read-only except for the explicit report, first-use
resolution, and optional sensitive report. `--apply` refuses without an
exclusive lifetime lock and an isolated rehearsal root in Phase 1. The exact
`blackbox` name is final: the package and library already own it, while
`blackboxd` remains daemon-only.

`--root` and `--rehearsal-root` both name the rehearsal state root, never a
`projects.json` file. The facade derives participant paths from that root.
An explicit `--state-dir` re-roots the complete conventional bundle; an
explicit `--projects-path` then overrides its projects member and therefore
wins when both are present. With no state override, non-project members remain
the exact shared-config paths. The CLI reports the resolved source and
destination roles without echoing private local paths in default JSON.

After parsing, the executable performs only:

```text
shared config load -> one typed layout constructor -> one facade operation
    -> redacted receipt envelope
```

It never opens an owner store, decodes a report or resolution, constructs an
observation or participant, maps a checkout action, computes a post-image, or
reopens migration state.

Update the binary inventory, getting-started and operations documentation,
release packaging, and Nix app definitions so the CLI is installed
deliberately. On macOS, resolve `which stablesign` and stable-sign the exact
built `blackbox` binary before any live CLI rehearsal, using the same operator
protocol as `blackboxd`.

After command parsing, CLI JSON uses one tagged v1 envelope with `version`,
`command`, and exactly one of `result` or `error { code, message }`. Human
diagnostics go to stderr and the envelope goes to stdout. A failure exits
nonzero and emits only the error-shaped envelope; help, version, and clap parser
diagnostics retain their conventional side-effect-free streams and never open
configuration or stores.

### 7.2 Compatibility proof

Use the facade verification result's non-serializable host-local compatibility
projection. It is built from a fully cross-validated migrated catalog and
attachment pair through `ProjectRecord::from_catalog_attachment`; P1-D does not
join the stores itself. Compare it with the v1 registry for every fixture:

- same attached project ids;
- same canonical paths;
- same Git/non-Git classification;
- same languages;
- same accepted alias behavior;
- same registration timestamp where exposed;
- no remote-only project fabricated into the attached list; and
- an explicit omitted-catalog count.

This is the contract Phase 2 must use when it replaces `ProjectRegistry` in the
isolated v2 runtime path. It is not wired into configured daemon state during
Phase 1. The serializable verification receipt exposes only attached and
omitted counts, never compatibility paths.

### 7.3 Later-phase readiness artifact

The migration marker and verification output must be sufficient for Phases 2
through 6 to prove:

- strict catalog pair and complete migration participants open and recover;
- every v1 id/ref/selector namespace has a typed v2 owner or quarantine;
- every active attachment can produce a compatibility row;
- every legacy publisher pin has a verified G1 pointer or explicit no-content
  disposition;
- every losing collected selector is absent from the effective manifest and
  pinned for rollback;
- remote-only projects require no path;
- no live activation would be silently selected under a mismatched scope; and
- the v1 backups required for rollback remain intact.

Do not add a release ledger to system memory. The readiness artifact is
machine-readable migration state plus this reviewed plan and tests.

### 7.4 P1-D gate and Phase 1 closeout

Run:

- CLI parser, help, side-effect, JSON-envelope, and exit-code tests;
- release inventory and Nix/install tests for the `blackbox` binary;
- compatibility fixture comparisons;
- all targeted Phase 1 nextest suites;
- `scripts/fmt.sh --check`;
- workspace nextest full profile on the cluster;
- workspace clippy and concurrency lint on the cluster;
- stable-signed isolated bridge bootsmoke;
- stable-signed isolated `blackbox` CLI help, preflight, apply, and verify
  rehearsal;
- isolated preflight/apply/verify rehearsal; and
- `git diff --check`.

Commit and push P1-D. Submit the exact pushed ref to full cluster verification.
Only after that passes, start a fresh broad Kimi implementation review using
the fixed baseline and complete diff. Repair, push, reverify, and resume the
same Kimi session until the final verdict is exactly `PASS`.

Phase 1 is complete only when:

- the plan review passed before implementation;
- P1-A through P1-D are committed and pushed separately;
- every milestone bootsmoke passed;
- the full exact-SHA cluster gate passed;
- the fresh implementation review passed;
- operator-state apply remains impossible until Phase 6; and
- the v1 bridge behavior remains at parity with the starting head.

## 8. Concurrency and security rules

- Never hold a Tokio worker across filesystem, Git, hashing, inventory, or
  transaction work. CLI operations are synchronous offline work; daemon-facing
  adapters use existing blocking actors or `spawn_blocking`.
- Lock order is lifetime migration lock, `projects.json.lock`, then auxiliary
  participant locks in deterministic role/path order. No inventory adapter may
  call back into catalog mutation.
- No lock is held while waiting for operator input.
- All durable file opens use no-follow semantics and reject symlinks or
  non-regular files.
- Staged, backup, journal, report, resolution, and marker filenames are
  owner-generated validated basenames.
- Default reports never include credentials, bearer tokens, producer labels,
  private repository identifiers, literal host paths, or unbounded file
  content. The explicit `--include-local-paths` review artifact may contain
  only bounded legacy/attachment paths, is marked sensitive, uses owner-only
  permissions, and is never a public fixture or commit candidate.
- Repository config can nominate aliases and expose recorded authority, but it
  cannot accept aliases, elect a publisher, select a survivor, or self-create a
  catalog project.
- Computed repository hints, origin URLs, paths, weak namespaces, and aliases
  never become scope authority.
- Phase 1 never mutates configured live code-source, index, vector, edge,
  knowledge, gap, or coordination stores. Rehearsal runs the exact G1,
  effective-manifest, quarantine, marker, and catalog transaction against
  isolated copied inputs. It performs the selector authority cut but defers
  physical index/vector/edge retirement.

## 9. Live bootsmoke protocol for every major milestone

Every P1 milestone uses the same live protocol after targeted tests and before
commit:

1. Build exact current `blackboxd` locally for macOS arm64.
2. Run `which stablesign` in the operator shell and require it to resolve.
3. Stable-sign the exact `target/debug/blackboxd` through the operator helper.
4. Confirm the selected isolated port has no listener.
5. Start `scripts/dev-isolated-daemon.sh` with a unique port and exact binary.
6. Require the log to reach the listening message with only throwaway paths.
7. Probe `/admin/runtime-metrics` and `/roster` for HTTP 200.
8. Perform MCP `initialize` and require a session id and successful result.
9. Perform the milestone-specific state assertion.
10. Send graceful interrupt, prove the listener is gone, and move any retained
    exact throwaway state directory to Trash.

Do not copy, replace, sign, restart, or signal the production or persistent dev
service. A later phase that explicitly needs the persistent dev instance must
first perform a read-only service scope check and obtain operator approval for
that named service.

Milestone-specific assertions:

- P1-A: bridge creates only v1-compatible state and the lifetime migration
  lock.
- P1-B: bridge startup while holding the shared lifetime lock makes an
  exclusive apply probe fail.
- P1-C: isolated CLI rehearsal produces and verifies v2 post-images while the
  separate bridge instance remains healthy and unchanged. The rehearsal verifies
  planned checkout ids, G1, effective-manifest quarantine, marker, and recovery
  roots.
- P1-D: stable-sign the exact `blackbox` binary, then require compatibility
  output from the rehearsed v2 state to match the v1 fixture while live-path
  apply remains refused.

## 10. Bookend protocol

### Before implementation

1. Finish this survey and plan.
2. Update the fixed Kimi plan-review lens to require this document and the
   complete governing plan.
3. Start a fresh Kimi plan-review session.
4. Treat every verdict other than exact `PASS` as `REVISE`.
5. Repair the documents and resume the same session with the fixed broad
   prompt.
6. Repeat until exact `PASS`.
7. Commit and push the clean plan milestone.

No implementation begins before step 6.

### After implementation

1. Finish all milestone commits, bootsmokes, and exact-ref cluster gates.
2. Start a fresh Kimi implementation-review session.
3. The fixed scope remains
   `monolith-decomposition-pre-attempt-2..HEAD`, not Phase 1 files.
4. Repair every finding, rerun relevant local and live gates, commit and push,
   rerun exact-ref cluster verification, and resume the same review session.
5. Repeat until exact `PASS`.

If Kimi is genuinely disrupted or unavailable, use a fresh GLM 5.2 bro with
the same read-only tool limits, fixed baseline, governing documents, complete
diff, required response, and no-narrowing resume text. A provider fallback
never narrows scope or resets review history. Resume that same GLM session for
all corrections.

## 11. Reviewer checklist

The plan reviewer must reject this plan unless it proves:

- Phase 1 is dependency-correct and does not smuggle later live behavior into a
  partial live cut;
- the v2 model cannot serialize host paths in the catalog;
- strict catalog and attachment corruption fails before runtime publication;
- census and authority remain separate;
- typed values cannot be forged through serde or string-shape inference;
- journal recovery cannot expose a mixed participant state;
- all crash boundaries have executable fault tests;
- preflight inventory is complete enough to detect unsafe identity joins;
- resolution authority is bounded and hash-bound;
- apply cannot race a bridge daemon or install into live state during Phase 1;
- every publisher pin has a verified G1 or explicit no-content disposition;
- markerless legacy roots normalize without path-derived authoritative ids;
- the migration marker, G1, and source quarantine share the one transaction
  decision and remain GC-pinned;
- catalog origin makes committed marker loss detectable without rejecting a
  fresh v2 store;
- surviving collected records acquire explicit scope from immutable descriptor
  agreement before first v2 bind;
- scope-migration records are path-free catalog data while legacy path bindings
  are host-local attachment data, so the regular pair transaction owns both;
- every attachment-proved scope transition has exactly one host-local proof,
  promotion has a typed durable epoch-bearing record, and operator-attested
  transitions have no fabricated proof;
- typed literal legacy-path observations are complete enough to build the
  ledger while default reports remain path-redacted;
- publisher detach preserves published reads without fabricating overlay Git
  ancestry;
- legacy ids and namespaces remain stable or explicitly quarantined;
- reports and fixtures are safe for a public repository;
- stable-signed live bootsmokes are required at every milestone;
- milestone commits and cluster gates are explicit;
- the implementation review is fresh and complete-baseline; and
- the final six-phase parity proof remains open rather than being inferred from
  Phase 1 success.
