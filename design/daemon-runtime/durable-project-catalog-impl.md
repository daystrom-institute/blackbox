---
title: "Durable corpus project catalog implementation plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, project-identity, catalog, checkout-attachments, indexing, migration]
brief: "Split durable corpus project identity from host-local checkout attachments, make collected code work without a local path, and gate every remaining checkout access behind an observable capability."
---

# Durable corpus project catalog implementation plan

Date: 2026-07-22

Companion designs:

- [`locality-first-decomposition.md`](locality-first-decomposition.md)
- [`distributed-code-source-collector-impl.md`](distributed-code-source-collector-impl.md)
- [`project-taxonomy-standardization.md`](../corpus/agentic-corpus/project-taxonomy-standardization.md)
- [`checkout-identity-and-provisional-knowledge.md`](../corpus/knowledge/checkout-identity-and-provisional-knowledge.md)
- [`checkout-provenance-export-impl.md`](checkout-provenance-export-impl.md)

## 1. Outcome and bounded scope

This slice removes the local `ProjectRecord` prerequisite from the distributed
code-source collector. It replaces the path-bearing project registry with two
different records:

1. a durable, path-free corpus project catalog; and
2. an authoritative host-local attachment store for concrete checkouts.

At the end of this slice, a catalog project with an active collected code
generation and no checkout on the corpus host survives restart, incremental
indexing, full rebuild, lexical and vector search, graph inspection, and
garbage collection. Project selection, entity refs, aliases, response paths,
and source activation use the catalog identity. Operations that genuinely need
a checkout acquire an explicitly selected attachment or return a typed
`attachment_required` result.

The implementation also lands two prerequisites before changing identity:

- response-level `built_from` tables for published and provisional knowledge
  and gap views; and
- checkout-access instrumentation enforced through validated leases.

Those prerequisites make mixed-view results and hidden filesystem fallbacks
observable during migration. They do not authorize the identity cut by
themselves.

This is still an overlap slice. Local project walking, Git history, blame,
rendering, repo-owned knowledge publication, provenance note import, artifact
watching, and mutation tools remain available through attachments. Their
continued existence does not prevent a remote-only catalog project from being
searchable. Moving those producers to typed checkout-to-corpus transports is a
later decomposition slice.

## 2. Decisions fixed for this implementation

The following policy choices are part of the plan and are not delegated to an
implementer:

1. Every migrated `project_id` is preserved byte-for-byte. New projects receive
   a server-minted opaque 128-bit random id. No new id is derived from a path,
   repository URL, alias, or string hash.
2. A remote-only project is created by an explicit operator catalog add/import
   action. A producer upload and a configuration reload never create a project
   implicitly, and a request never supplies the corpus `project_id`.
3. Durable file identity is `(project_id, relative_path)`. The stable machine
   rendering is `bbox://project/<project_id>/<encoded-relative-path>`. A corpus
   host never fabricates an absolute path for collected content.
4. When a project has no selected Git attachment, immutable commit documents
   may remain available with a stale/unrefreshable stamp, but the
   generation-bound `COMMIT_TOUCHED_FILE` overlay is absent. It never points at
   a different active code generation.
5. Removing a producer assignment revokes its credential immediately. If no
   eligible local attachment exists, the last collected generation remains
   effective in a persisted `cutback_pending` state. Reload succeeds and no
   retry loop spins.
6. Non-Git projects and Git projects without committed recorded authority stay
   in a typed `legacy_local` lane. A computed repository hint never becomes a
   `PublishedScope` by inference.
7. Publisher authority is operator-owned. Repository config may nominate
   aliases and defaults, but cannot elect its checkout, tracked ref, or accepted
   publication commit.
8. Catalog deletion is distinct from attachment detachment. It refuses while
   attachments, active or retained generations, entity references, or durable
   project-scoped state remain, unless a separate explicit destructive-retire
   workflow has discharged them.
9. The unregistered-cwd substring lane is a permanent search compatibility
   feature. It carries no catalog identity and grants no filesystem authority.
10. Promotion from `LegacyLocal` to `Published` is an explicit audited operator
    action that preserves the existing project id. Register detects the need
    and refuses with the promotion command instead of creating a second project.
11. Structural cutback blockers are event-driven. Transient staging failures
    use bounded persisted backoff and startup re-drive.
12. Catalog retire refuses while any configured producer assignment targets
    the project. The assignment is removed first.
13. Committed aliases remain nominations requiring operator acceptance. This
    is an intentional authority change; migrated materialized aliases stay
    active so existing selectors do not break during the transition.
14. The legacy `bbox_project_list` remains attached-project-only and reports an
    additive omitted-catalog count. The new catalog list/get surfaces are the
    complete inventory, including remote-only projects.
15. A repo-history primary namespace never changes after it is safely
    established. Import uses an already-materialized unambiguous legacy
    namespace when one exists; recorded authority is primary only when no safe
    namespace exists. Promotion preserves the primary.
16. A weak legacy namespace never proves that two records are the same repo.
    Cross-repo namespace collision is a hard preflight refusal until an
    operator supplies a split; ambiguous old refs are quarantined, not guessed.
17. Moving a project's monorepo relpath or recorded repo authority never uses
    register to mint a replacement id. An explicit audited scope migration
    preserves the project id and stores the old scope only as non-authoritative
    provenance. Producer grants accept only the new catalog scope.

## 3. Non-goals

This slice does not:

- transport Git objects, Git history, blame execution, or Git-note import from
  another host;
- transport repo-owned knowledge or gaps from a checkout host;
- expose a model-facing arbitrary JSON, filesystem, or blob endpoint;
- make provisional `all` visibility cross-host;
- implement session workspace mount maps that do not exist yet;
- silently merge two old project ids that claim one durable scope;
- rewrite existing project, symbol, commit, provenance, artifact, or edge refs
  to a newly minted identity;
- declare the whole daemon checkout-free or move the corpus off-host; or
- reopen multi-fleet routing, broad contradiction policy, or the optional
  blackops process split.

## 4. Current coupling and why an additive field is insufficient

`ProjectRecord` currently combines five concerns:

- the path-derived eight-hex `project_id`;
- `canonical_path` on this host;
- a weak computed `repo_id` hint;
- durable-scope aliases loaded from a checkout; and
- derived Git/language metadata.

That record is loaded directly below the daemon by the code index, Git-history
index, tool-edge parser, hybrid search, and source activation. Collected staging
still receives a `ProjectRecord`, stamps its path into documents, and opens the
path for Git-current edges. Full rebuild and active-selector registration
enumerate `projects.json`, so a collected generation with no local record is
either skipped, hidden from readers, or deleted by a later purge.

The same conflation appears above the index. Publisher election scans checkout
paths. Knowledge and gap views hydrate a durable project with
`canonical_path`. Blame, render, file providers, refactor tools, artifact
watchers, and provenance import accept a logical project and then assume that
it names one local directory. Several selectors accept a raw project id only
when it looks like eight hexadecimal characters.

Adding `canonical_path: Option<_>` to this record would spread an optional-path
state through every consumer without defining authority, selection, or failure
behavior. The implementation instead gives path-free and path-requiring code
different input types.

## 5. Core data model

### 5.1 Typed project identity

Add pure types to `bbox-corpus-core`:

```text
ProjectId(String)
RecordedRepoAuthority(String)
RepoBootstrapHint(String)
CommitNamespace(String)
RepoHistoryId(String)
ScopeMigrationId(String)

CorpusProject {
    project_id: ProjectId,
    scope: ProjectScope,
    operator_aliases: Vec<String>,
    nominated_aliases: Vec<String>,
    display_name: String,
    created_at: String,
    registered_at_compat: Option<String>,
    repo_history: Option<RepoHistoryId>,
    languages: Vec<Language>,
}

ProjectScope = Published(PublishedScope) | LegacyLocal

RepoHistoryRecord {
    repo_history_id: RepoHistoryId,
    authority: RepoHistoryAuthority,
    primary_namespace: CommitNamespace,
    compatibility_namespaces: Vec<CommitNamespace>,
}

RepoHistoryAuthority =
    Recorded(RecordedRepoAuthority)
  | LocalProject(ProjectId)
  | LegacyNamespace(CommitNamespace)

AmbiguousNamespaceRecord {
    namespace: CommitNamespace,
    candidate_repo_history_ids: Vec<RepoHistoryId>,
    status: Quarantined,
}

CatalogOriginV2 =
    FreshV2
  | MigratedV1 { transaction_id }

CatalogSnapshotV2 {
    version: 2,
    epoch,
    origin: CatalogOriginV2,
    projects,
    repo_histories,
    ambiguous_namespaces,
    scope_migrations: BTreeMap<ScopeMigrationId, ScopeMigrationRecord>,
}
```

`ScopeMigrationRecord` is the path-free record defined in section 7.2. It is
catalog data, not a sidecar, so the logical scope change and its compatibility
bridge share the regular catalog/attachment pair transaction. A fresh v2
initializer writes `FreshV2`. The v1 importer writes
`MigratedV1 { transaction_id }`; strict open then requires a committed
migration marker with the same transaction id. The complete plan hash remains
in the marker and journal, avoiding a hash cycle through the catalog
post-image. Marker loss is therefore distinguishable from a store that was
born at v2.

`ProjectId::parse` accepts the already-persisted legacy ids plus new ids under
one bounded, path-safe contract: 1 through 96 ASCII alphanumeric, `_`, `-`, or
`.` characters, excluding `.` and `..`. It rejects colon, slash, backslash,
whitespace, control characters, percent escapes, and empty input. New ids use
`p_` followed by 32 lowercase hexadecimal characters from the operating
system random source. Creation collision-checks the catalog and retries with a
bounded failure result.

Callers never decide that a string is an id from its shape. Selector resolution
first parses it and then requires exact catalog membership. Existing eight-hex
special cases in hybrid search, static transcript search, Slack binding help,
tool descriptions, and tests are removed.

`PublishedScope.repo_id`, a computed bootstrap hint, an `aka_repo_id`, and the
legacy commit namespace are separate typed values. No code distinguishes them
by length or formatting. The migrated weak `ProjectRecord.repo_id` becomes
the primary or compatibility namespace in a `RepoHistoryRecord` so existing
`commit:<namespace>:<sha>` refs, clean snapshot ids, metadata filenames, and
joins remain stable. `CorpusProject.repo_history` references the shared record;
it does not own a namespace. `RepoHistoryId` is a server-minted opaque id, so a
record can exist before recorded repo authority. The catalog enforces unique
`RecordedRepoAuthority` among records that have recorded authority. The
catalog's authoritative scope comes only from
committed recorded authority, an operator override, or an explicit operator
catalog import.

Commit namespaces are owned by a repo-history record, not independently by
each project. Primary selection is one rule across creation, import, and
promotion: if the inventory proves an already-materialized namespace belongs
unambiguously to that repo, preserve it as primary forever, whether recorded
authority is already present or appears later. If no materialized namespace
exists, a published repo uses its full `RecordedRepoAuthority` as primary. A
promotion changes authority but never changes a safely established primary.
Every monorepo project for the proved repo shares the record.

Other already-materialized namespaces become typed compatibility namespaces
only after the same-repo proof below. Existing refs are not rewritten. Lookup
during the compatibility epoch searches primary plus compatibility namespaces,
while all new commit materialization uses the primary. The catalog and this
mapping move together, so another host never re-derives the choice. A
`LegacyLocal` project created under v2 receives a server-minted local history
record whose authority is `LocalProject(project_id)` and whose primary
namespace is an independent random `CommitNamespace`. It may ingest
attachment-backed local Git history, preserving the existing registered-Git
capability, but that record cannot authorize publishing, producer grants,
cross-host repository identity, or another project. Historical commit
documents imported for an old LegacyLocal record remain queryable through its
legacy-authority record. Promotion records proved repository authority while
preserving any safely materialized local or legacy namespace as the primary or
a compatibility namespace under the same rules.

A colliding namespace that cannot be attributed safely is not an established
primary for either repo. It lives in `AmbiguousNamespaceRecord`, is excluded
from commit-ref resolution and GC ownership, and returns
`ambiguous_commit_namespace` instead of selecting a candidate. After an
operator split, each repo with recorded authority uses that authority as its
new primary; a repo without authority has no refreshable history record until
promotion. The ambiguous historical documents remain quarantined and are not
rewritten or deleted by ordinary project GC.

Quarantine is not necessarily permanent. A later explicit
`bbox_repo_history_namespace_resolve` operation may attribute individual commit
documents only after the selected repo object store proves each commit SHA and
the operator accepts the inventory. It writes new unambiguous generation
ownership without changing the old entity ref, then retires the quarantine only
after coverage is complete. Until that proof, ambiguity remains fail-closed.

The catalog enforces uniqueness of `ProjectId`, `PublishedScope`, and accepted
operator alias. A `LegacyLocal` record has no wire identity and cannot be used
for a producer grant or remote publisher.

### 5.2 Authoritative host-local attachments

Add a separate fail-closed attachment store next to the configured projects
path:

```text
CheckoutAttachment {
    attachment_id: String,
    project_id: ProjectId,
    checkout_id: String,
    checkout_dir: PathBuf,
    checkout_project_dir: PathBuf,
    project_root_relpath: String,
    kind: Base | Worktree | ManagedClone,
    validated_scope: Option<PublishedScope>,
    computed_repo_hint: Option<RepoBootstrapHint>,
    branch_ref: Option<String>,
    capabilities: AttachmentCapabilities,
    status: Attached | Detached,
    attached_at: String,
}

AttachmentSnapshotV1 {
    version: 1,
    epoch,
    attachments,
    scope_migration_proofs:
        BTreeMap<ScopeMigrationId, ScopeMigrationAttachmentProof>,
    legacy_path_bindings:
        BTreeMap<LegacyPathBindingId, LegacyPathLedgerEntry>,
}
```

One checkout may carry multiple monorepo attachments. The key is
`attachment_id`; active uniqueness is `(project_id, checkout_id,
project_root_relpath)`. Paths are canonicalized and are host-local routing
data, never serialized into catalog records, code documents, entity refs,
collector descriptors, or durable response stamps.

Capabilities include local code source, Git history, blame, repo knowledge,
repo mutation, render output, provenance note I/O, and artifact watching. A
capability is not inferred merely because a directory exists. Acquisition
revalidates the conservative read or write gate, checkout id, recorded scope,
catalog match, and operation-specific conditions.

The existing `CheckoutRegistry` remains the recoverable provisional-overlay
discovery census. Its corrupt-file behavior may still degrade to an empty
index. It may gain `project_id` and `attachment_id` references, but it is not
the authoritative attachment store and cannot authorize filesystem access.

The path-bearing compatibility ledger from section 7.3 lives in this host-local
strict snapshot, not the path-free catalog. A `LegacyPathLedgerEntry` is mapped,
unscoped, or quarantined and retains its bounded historical path,
source-store/row identity, relationship, and inventory epoch. Attachment
relocation appends a binding in the same pair transaction that updates the
attachment and any path-free catalog `ScopeMigrationRecord`.
An attachment-proved scope migration also stores its attachment id, checkout
id, revalidated old/new scope evidence, and proof timestamp here as
`ScopeMigrationAttachmentProof`. The catalog record stores only the logical
provenance class and never a host-local attachment id.

### 5.3 Compatibility views

Keep `ProjectRecord` temporarily as a joined compatibility view constructed
only from a `CorpusProject` and one validated attachment. It is never the
input to catalog-only code. A remote project cannot be coerced into a fake
record.

Introduce explicit resolver outputs:

```text
CatalogProjectContext { project: CorpusProject }

AttachedProjectContext {
    project: CorpusProject,
    attachment: ValidatedCheckoutAttachment,
    checkout: Option<CheckoutContext>,
}
```

Corpus queries stop at `CatalogProjectContext`. Filesystem operations require
`AttachedProjectContext`. Compatibility adapters are deleted surface by
surface after callers move to one of those types.

## 6. Persistence and crash-safe migration

### 6.1 File placement and transaction owner

The version-2 catalog remains at the existing configured
`BLACKBOX_PROJECTS_PATH`. This preserves parent-derived edge, Git metadata,
activation, and sidecar locations. The attachment file and a project-store
transaction journal live beside it. `bbox-indexing::projects` remains the sole
mutation owner. Pure versioned decoding and validation live in
`bbox-corpus-core` so lower corpus crates can read a catalog without depending
on `bbox-indexing`.

The catalog and attachment store are strict state. Unsupported versions,
corrupt JSON, invalid ids, duplicate scope or alias, dangling attachment, and
scope mismatch fail closed at open, before server routes bind. Only the separate
provisional checkout census is recoverable-by-recompute.

The short catalog mutation lock is the existing canonical
`projects.json.lock` derived by `with_store_lock(projects_path)`. The bridge
`StorePersister`, v2 pair owner, preflight capture, and migration coordinator
all honor that one lock. No second lock file guards the same logical store.
The process-lifetime migration lock is acquired first. Auxiliary participant
locks follow `projects.json.lock` in deterministic role/path order. Immutable
inventory snapshots plus apply-time byte-hash revalidation protect stores that
cannot share this lock.

### 6.2 Two-file transaction protocol

Registration and any operation that changes both stores use one locked,
journaled transaction:

1. Acquire the existing project-store exclusive lock.
2. Load and validate both current snapshots and recover any prior journal.
3. Build complete post-images in memory and validate all cross references.
4. Write checksum-named staged post-images, fsync both files and their parent.
5. Atomically write and fsync a journal containing transaction id, version,
   old hashes, new hashes, and staged filenames.
6. Rename both post-images into place, fsyncing the parent after each rename.
7. Verify both installed hashes, atomically mark the journal committed, then
   remove staged and backup files only under the retention policy.
8. Publish the new in-memory snapshot only after the installed pair verifies.

Startup completes an uncommitted transaction forward when both staged
post-images verify. Otherwise it restores both old snapshots from verified
transaction backups. It never chooses one new file and one old file. Fault
injection covers every write, fsync, rename, verify, and journal transition.

Catalog-only actions use the same machinery with an unchanged attachment
post-image. Attachment-only actions use an unchanged catalog post-image. This
keeps one recovery protocol and one visible epoch.
Every regular transaction preserves `CatalogOriginV2` byte-for-byte. Only the
fresh initializer or v1 importer sets it, so later administration cannot erase
the marker requirement.

Migration uses the same transaction owner with a closed, role-bounded
participant plan rather than invoking an independently committing pair
transaction. Catalog and attachment post-images are mandatory participants.
The migration plan additionally includes the complete effective
source-manifest post-image, versioned scope-bearing activation and retained
generation metadata post-images, typed collision-retirement records and losing
legacy activation removals, accepted-publication pointer post-images, and the
migration marker. Every mutable participant records:

```text
role
old: Absent | { hash, backup_name }
new: Absent | { hash, stage_name }
```

Roles derive their targets in code; the journal never accepts arbitrary target
paths. Immutable G1 knowledge/gap generations and quarantined collected
generations are transaction assets named by hash. A prepared journal and a
committed marker pin every asset and backup against GC.

The migration owner stages every asset and mutable post-image, fsyncs all
backups and stages, writes one prepared journal, installs every participant,
verifies every hash and cross-store invariant, then marks that journal
committed. Recovery classifies every participant together as old, new, other,
or missing. It completes forward only when every new image is installed or
available from a verified stage. It rolls back only when every old image is
installed or available from a verified backup. `old = Absent` rollback removes
only an exact matching transaction-created image. Any incomplete set fails
closed without mixing epochs.

### 6.3 Version-1 import and rollout command

Version-1 import is an explicit offline operation, not a side effect of first
v2 daemon startup. Add these non-daemon command modes:

```text
blackbox project-catalog migrate --preflight [--report <path>]
blackbox project-catalog migrate --apply [--resolution <path>]
```

Preflight takes a shared/read lock, reads live v1 state, writes no project
state, and emits a complete machine-readable report plus its v1 inventory hash.
It can run while the v1 daemon remains available.

Phase 0 first ships a v1-store-compatible daemon that holds a shared
`project-catalog-migration.lock` for its entire process lifetime. All project
store writers in that bridge release honor the same lock. Apply requires the
exclusive lifetime lock, so runbook ordering is enforced rather than trusted;
it cannot race any compatible daemon write. It also requires the same
inventory hash, reruns preflight after acquiring exclusivity, performs the
journaled transaction, and emits the resulting catalog epoch. The final v2
daemon holds the lifetime lock too. A v2 daemon that sees v1 bytes fails closed
with the exact preflight/apply command; it never attempts an implicit import.

Before import, take a read-only inventory of:

- every old `ProjectRecord` and its current or missing path;
- committed recorded scope when safely readable;
- active and retained code-source activations and immutable descriptors;
- every `PublisherRefStore` pin, its exact source bytes, full ref, current
  unique publisher candidates, resolved commit, and scope proof;
- Tantivy/vector project ids and entity refs;
- edge manifests and workspace selectors;
- Git metadata and legacy commit namespaces;
- artifacts and provenance targets;
- one typed observation for every durable project-scoped store row, including
  store kind, stable row id, and the bounded literal project/path selector
  required for inventory-time deepest-root classification; and
- materialized aliases and registration timestamps.

Literal legacy selectors remain inside the immutable inventory and strict
host-local attachment post-image. Default reports expose only observation id,
store kind, relationship/status, and a domain-separated path digest. The local
CLI may display a selected ambiguous row or write an explicit
`--include-local-paths` review artifact with owner-only permissions and a
sensitive marker. That artifact is never a public fixture or commit candidate.
Apply reruns the typed inventory and never performs a second untracked live
store read to build the ledger.

Preflight also inventories `.bbox/local/checkout-id` once per canonical
checkout root as valid, missing-or-empty, malformed, unreadable, or symlinked.
For each eligible missing-or-empty root the persisted preflight report contains
one planned strong-random checkout id. The report, resolution, planned identity
actions, and all predicted post-images are bound by one plan hash. Apply writes
that plan to the prepared migration journal before installing a missing marker
through no-follow create-if-absent semantics. An already matching id resumes;
any different id or other marker transition refuses. A successfully installed
marker is monotonic v1-compatible host identity and is not deleted on rollback.
Malformed, unreadable, or symlinked markers require attachment exclusion or
keep migration refused. Synthetic path-derived `v1-root` ids never enter the
authoritative attachment snapshot.

Preflight groups old records into one repo-history record only with same-repo
evidence independent of the weak namespace: identical committed recorded repo
authority; or a shared canonical Git common directory whose first commit also
matches; or an immutable collected descriptor/activation authority match when
the checkout is missing. Origin URL, path hash, abbreviated namespace, alias,
or namespace equality alone is insufficient. The report records which evidence
authorized every group and the fallback provenance that originally produced
each weak namespace.

If one namespace is claimed by groups with different recorded authority,
different proved first commits, or otherwise no same-repo proof, preflight
hard-refuses. A resolution may confirm stronger same-repo evidence and merge
the groups, or split them into separate `RepoHistoryId` records. Split writes an
`AmbiguousNamespaceRecord`; it never installs the colliding namespace as either
record's primary or compatibility namespace. Projects with recorded authority
then use that authority as primary. Authority-less projects lose refreshable
history until promotion, while their ambiguous historical docs remain
quarantined. The resolution report states this availability loss explicitly.

The importer deterministically creates one `CorpusProject` with the exact old
`project_id` and exact legacy commit namespace for every old record. A readable
authoritative committed scope becomes `Published`; otherwise the project is
`LegacyLocal`. Missing paths are not dropped. If an active collected descriptor
contains a matching scope and project id, it may recover that project's scope
without opening the missing path. Descriptor, activation, and manifest must
all agree.

An existing path becomes an attachment only after canonicalization and scope
validation. Old aliases become `operator_aliases`, preserving behavior without
letting a future branch edit rewrite authority. Derived languages and
`registered_at` are retained.

The importer refuses the whole cutover when it finds:

- one id associated with different logical projects;
- multiple old ids claiming one `PublishedScope`;
- one weak commit namespace claimed by distinct repos or by groups lacking
  same-repo evidence;
- duplicate accepted aliases;
- disagreement among an activation, descriptor, manifest, and old record;
- an attachment whose validated scope belongs to another catalog project; or
- a malformed id or commit namespace that cannot round-trip safely.

It does not merge or pick a winner. The optional resolution file is a
versioned, operator-authored mapping bound to the preflight inventory hash. For
duplicate scope claims it selects the one old id that owns the published scope;
the other ids remain `LegacyLocal` with all their state and an explicit
collision diagnostic. It does not rewrite or redirect their entity refs. For
an id/path collision it may exclude the conflicting attachment, but may not
remint or reassign the preserved id. Unresolvable conflicts keep apply blocked.
The operator reruns preflight with the mapping and must obtain a clean report
before apply.

A losing duplicate-scope id with active or retained collected state requires an
explicit `quarantine_collected` disposition in the resolution file. Apply moves
its activation and selector to
`CollisionRetirementLifecycle::Pending { former_scope, generation }`, preserves
immutable bytes for rollback, and removes it from effective/read-view selection
before v2 can bind. It does not relabel the generation as LegacyLocal and does
not serve the former scope. Startup scope-agreement validation accepts this one
typed quarantine state, then resumes the normal journaled
selector/vector/edge retirement. The losing project remains
`Unavailable::ScopeCollision` until an attachment-backed local generation
succeeds or the project is explicitly retired. Without the disposition, apply
refuses. The migration report lists the exact search, graph, vector, and source
generations made unavailable.

Collision quarantine has one project-scoped durable lifecycle record. Its
typed states are `Pending`, `Queued`, and `Completed`, and every state preserves
the losing project, former scope, generation, selector, and migration evidence.
The ordinary retirement queue is subordinate execution state, not the terminal
proof. A completed lifecycle record remains as the durable receipt after
physical retirement; a matching lagging queue row is tolerated and removed
idempotently. Startup and GC reject missing, regressed, or cross-bound lifecycle
evidence instead of requiring an ephemeral queue row to survive forever.

The staged complete active-manifest post-image removes every losing workspace
row in the same migration transaction that installs the catalog. This is the
authority cut: no search selector or graph snapshot for the loser remains
effective at first v2 bind. Physical Tantivy documents, vector partitions, and
edge snapshots may remain pinned for idempotent retirement, but deferring
physical deletion never defers the selection cut.

For every surviving active or retained collected generation, migration writes
strict version-2 metadata with an explicit `PublishedScope`. Legacy
`StoredGeneration.descriptor.scope` and the immutable manifest are the only
backfill evidence: descriptor, manifest, legacy activation, effective
selector, and the migrated catalog project must agree exactly. Any missing or
ambiguous join refuses. `ActivationRecordV2.published_scope` and
`StoredGenerationV2.published_scope` have no serde default and must equal the
immutable descriptor scope. Losing collision records preserve their former
scope under the typed quarantine instead of producing an active v2 record.
Thus first v2 startup validates already-rewritten scope-bearing metadata; it
never guesses scope from project id.
An active or retained legacy generation whose descriptor is missing or corrupt
keeps preflight refused. No resolution may invent its scope; the operator must
repair or retire it with the bridge-compatible v1 tooling and rerun preflight.

Migration streams the complete legacy generation namespace into a canonical,
ordered SHA-256 commitment and row count. Only effective roots and generations
retained by the code-source store's actual owner policy for catalog,
activation, or collision-lifecycle scopes survive into catalog state,
selectors, or migration participants. Other historical and orphan rows remain
covered by the complete-set proof but are inert, non-selectable GC candidates;
they never become v2 authority merely because their immutable bytes exist.
Current-state validation and GC use the same classifier. A protected legacy row
whose project/scope ownership cannot be proved from strict v2 state refuses
startup rather than being treated as an orphan.

Every legacy publisher pin has exactly one migration disposition. A uniquely
resolved old publisher produces `SeedG1` with project id, attachment id,
expected scope, full ref, accepted commit, generation id, canonical knowledge
and gap file manifests, payload hashes, and pointer hash. Ambiguous or missing
candidates require a hash-bound resolution that selects one inventoried
attachment or records `NoPublishedContentAcknowledged` with a bounded reason.
A resolution cannot invent a ref, scope, commit, or attachment. Apply writes
and verifies immutable G1 assets and installs accepted pointers through the
same migration transaction. Exact `publisher-refs.json` bytes and checksum are
retained as rollback input, named by the journal and marker, and never
consulted by the v2 runtime.

The original version-1 bytes, `publisher-refs.json`, and their checksums are
retained until the catalog, attachments, index materialization,
accepted-publication state, and durable-store backfills have passed the final
parity and rollback closeout gate. The migration marker is a staged mutable
participant, not an after-commit receipt. It names the transaction id and plan
hash, complete inventory, every mutable post-image, every immutable G1 and
quarantine asset, retained backups, and schema epoch. Startup runs migration
recovery before opening any participant. A v2 catalog with neither a valid
marker nor a recoverable prepared journal fails with
`error.project_catalog_migration_incomplete`; it never infers completion from
valid catalog bytes alone. External storage GC excludes all prepared-journal,
marker, stage, backup, G1, and quarantine roots.

Phase 1 through Phase 5 run this exact protocol only against isolated copied
state. The supported live rollout occurs in Phase 6: deploy the v1-compatible
bridge under the normal shared-service approval process, run preflight, resolve
every refusal, stop that daemon, run apply while holding the exclusive lifetime
lock, then start the complete v2 runtime. Tests cover a live bridge preventing
apply, refused preflight, hash-bound resolution, successful retry, every
auxiliary participant crash boundary, and v2 startup refusal when apply was
skipped.

The configured live service remains on the last deployed v1-compatible bridge
binary throughout Phases 2 through 5. Those phase binaries are exercised only
against isolated v2 state and are not installed over the live bridge. Phase 6
is the one approved service replacement: apply completes under the stopped
bridge's exclusive lock, then the complete v2 binary starts and refuses any
remaining v1 bytes.

## 7. Project selection, administration, and path-bound state

### 7.1 One resolver with two stopping points

Replace parallel selector implementations with one resolver. Its order is:

1. exact catalog `ProjectId` membership;
2. exact unique accepted catalog alias;
3. exact `PublishedScope` for operator/internal APIs that explicitly accept a
   scope;
4. exact or deepest-contained active attachment path;
5. linked-worktree or managed-clone mapping through an attachment; and
6. session cwd only as a legacy fallback through the session attachment.

Equal-depth, alias, scope, or attachment ambiguity fails closed. Unknown
absolute paths never manufacture a catalog identity. Corpus-only tools stop
after the project. Path tools require a session-pinned attachment, an explicit
attachment selector, or exactly one operator-selected default with the needed
capability. They never choose the first clone.

Route hybrid/discover search, transcript search, knowledge/gaps, threads,
notes, pins, roadmap, packets, whiteboards, bindings, provenance planning,
tool-edge stamping, and storage surfaces through this resolver. Preserve the
current asymmetric read and write gates when resolving attachments.

### 7.2 Administrative semantics

Add explicit operator surfaces after list/get deduplication:

- catalog add/import by authoritative `PublishedScope` or explicit
  `legacy_local` kind;
- catalog list/get;
- promote one attached `LegacyLocal` project to its newly committed scope;
- migrate one published project to a new monorepo relpath or recorded repo
  authority while preserving its id;
- attach and detach;
- choose a default local-source attachment;
- bind or advance a publisher attachment and full ref;
- accept or reject nominated aliases; and
- catalog retire/delete with a full reference and generation inventory.

Producer traffic cannot invoke these surfaces.

The daemon does not authenticate a human operator separately from an agent MCP
client. Tool-surface filtering is visibility control, not operator
authentication. Attachment-backed operations with live repository proof may
remain MCP-callable only with the expected catalog epoch, required explicit
authority acknowledgement, and a bounded audit reason. Agents may pass
operator-supplied acknowledgements but may not default or infer them.
Proofless authority operations, including unattached import or scope migration,
conflict-resolution apply, and whole-store migration apply, are local
`blackbox project-catalog` CLI operations protected by the lifetime lock.
Adding authenticated operator roles is a repository-wide transport change, not
an authority property this catalog design pretends already exists.

`bbox_project_register` remains a compatibility composite: resolve committed
scope, find or create the catalog project according to operator-local register
authority, then attach the path in one transaction. For a new published scope,
registration may create the project because the operator explicitly invoked
registration on that checkout. For an unrecorded or non-Git checkout it creates
a `LegacyLocal` project with a random id. Idempotency is by validated scope and
active attachment, not a path hash.

If register observes newly committed authority on an attachment already bound
to `LegacyLocal`, it returns `scope_promotion_required` with the exact project
id and proposed scope. It does not create a second project or move the
attachment. `bbox_project_promote` requires that project id, revalidates the
explicit promotion attachment and committed authority, proves no published
project already owns the scope, then changes the same catalog record to
`Published(scope)` in the journaled transaction. It inserts the typed
`promotion` `ScopeMigrationRecord` plus matching attachment proof in the same
pair transaction; that record stores old kind, new scope, operator invocation,
and catalog epoch. If another project already owns the scope, promotion refuses
and points to the offline compatibility resolution workflow; it never merges
automatically.

When the project has multiple active attachments, promotion validates every
one with readable committed authority and requires the exact same proposed
scope. An unreadable, unrecorded, or mismatched attachment makes promotion
ambiguous and must be detached or repaired first; the designated attachment is
not allowed to overrule siblings.

Promotion preserves every project-id-keyed store and active local
materialization. It creates a recorded repo-history record by section 5.1 if
none exists. If the project references a `LegacyNamespace` or `LocalProject`
history record, promotion changes that record's authority to `Recorded`
without changing its stable `RepoHistoryId`, primary namespace, compatibility
namespaces, or commit refs. Every sibling project referencing the same id
continues to reference the same record. The transaction verifies that all
siblings with readable authority name the same repo; a conflicting sibling
blocks promotion and requires explicit history-record split/resolution. A
sibling that remains LegacyLocal may read retained history but cannot publish
or refresh through the promoted sibling's authority. In-flight local staging
pins the old catalog epoch, loses its publish compare-and-swap after promotion,
and retries against the published identity. Init and eject report promotion as
the required next operator action after the new authority is committed.

`bbox_project_rename` becomes attachment relocation. It verifies the same
checkout and same `PublishedScope` after the move before committing the new
path. Moving a monorepo project to a different `bbox_root_relpath` or rebinding
to another repo is an explicit scope migration, not rename.

Published attachments prove sameness with checkout id plus revalidated scope.
For markerless `LegacyLocal`, rename refuses. The operator can first create a
checkout identity through the explicit checkout-init surface, or detach and
reattach with the existing project id and an explicit identity confirmation.
Rename never treats path existence or inode reuse as proof of sameness.

The shared scope-migration request requires project id, exact expected old
scope, exact new scope, kind `relpath_move` or `repo_authority_change`, authority
mode, and supports dry-run. It has two deliberately different invocation
channels.

`bbox_project_scope_migrate` is the MCP operator surface and accepts only
`attachment { attachment_id }` authority. The selected attachment must have the
same checkout identity and committed config must resolve to the new scope, and
every sibling attachment must agree or be detached.

`blackbox project-catalog scope-migrate --operator-attested` is a separate
offline CLI surface for a project with zero active attachments. It is absent
from the MCP catalog and producer HTTP routes, requires the daemon to be stopped
and the exclusive process-lifetime migration lock, and requires explicit
`--acknowledge-unattached-scope-migration` plus a bounded operator reason. Its
audit provenance is `operator_attested`. This is the same operator authority as
an explicit remote catalog import, not an inference from producer traffic. The
CLI refuses if any active attachment exists. A repo-authority change through
either channel additionally requires an explicit
`acknowledge_repo_authority_change` operator flag; agents never default or infer
it. The target scope must be unowned. If it is already owned, the command
refuses and points to the offline survivor/compatibility workflow.

The regular catalog/attachment pair transaction atomically changes the catalog
scope, revalidates and updates the selected attachment when present, appends
applicable host-local path bindings inside `AttachmentSnapshotV1`, and inserts
the path-free migration record into `CatalogSnapshotV2.scope_migrations`:

```text
ScopeMigrationRecord {
    scope_migration_id,
    project_id,
    catalog_epoch,
    authority_provenance,
    operator_invocation,
    operator_reason?,
    old_scope: ProjectScope,
    new_scope: ProjectScope,
    kind: promotion | relpath_move | repo_authority_change,
    migrated_at,
    code_bridge_generation?,
    publication_bridge_generation?,
    pending_capabilities,
}
```

For attachment-proved migration, the pair transaction also inserts the matching
`ScopeMigrationAttachmentProof` in `AttachmentSnapshotV1`. Operator-attested
unattached migration has no attachment proof row. The catalog never serializes
a host-local attachment or path merely to retain audit evidence.
Strict cross-validation is bidirectional: every attachment-proved record has
exactly one matching proof that revalidates its old/new scopes and attachment,
and every proof has one record. Operator-attested records have no proof row.

Promotion is the `LegacyLocal -> Published` transition kind in this same
path-free audit chain. Relpath moves and recorded-authority changes are
`Published -> Published`. Other shape/kind combinations refuse. The explicit
`catalog_epoch` is the new epoch committed by the pair transaction, and
`operator_invocation` is the bounded audit source. This is the durable home for
the promotion audit; it is not journal-only.

A published old scope is a historical alias for provenance and exact legacy
selector diagnostics only. `LegacyLocal` in a promotion record carries no wire
scope. Historical values are excluded from catalog uniqueness, publisher
election, producer grants, config resolution, upload authentication, and new
writes. `build_snapshot` resolves only `CorpusProject.scope`, never historical
aliases.

An already-active collected generation remains selected by project id through
`code_bridge_generation`, but its response/source metadata truthfully retains
the old published scope and health reports `scope_migration_refresh_required`.
The exact migration record is the only allowed catalog/activation scope
disagreement at startup. The first successfully activated generation uploaded
under the new scope clears the code bridge and retires the old generation
normally. The accepted publication pointer similarly remains readable with its
old-scope `built_from` stamp and stale health until an operator rebind/advance
under the new scope clears the publication bridge. No immutable descriptor or
accepted snapshot is relabeled.

A relpath move keeps the existing repo-history record. A recorded-authority
change preserves the safely established primary namespace, records the former
authority as a non-authoritative compatibility note, clears the current-file
Git overlay, and marks history refresh-required. New history generation and
commit proof use the new recorded authority. The old authority never authorizes
scope, producer traffic, or publisher selection.

Register checks checkout id and historical attachment/path bindings before
find-or-create. When an existing project now validates to a different scope, it
returns `scope_migration_required` with the exact dry-run command and never
creates a second project. The online attachment-proven command is sufficient
for an unowned target because the journal and migration record preserve
rollback. Unattached attestation is offline because no checkout proof exists.
Only target collision uses the separate offline resolution ceremony.

`bbox_project_unregister` becomes detach and leaves logical state intact.
Reattaching a `LegacyLocal` checkout requires an explicit project id when no
authoritative scope can prove identity. `bbox_project_eject`, init, render,
artifact discovery, and repository mutation require an attachment capability.

The existing `bbox_project_list` continues returning attached compatibility
`ProjectRecord` rows and adds `omitted_catalog_projects` plus a pointer to
`bbox_project_catalog_list`. It never invents a path for remote projects. The
new catalog list/get response is the canonical complete inventory.

### 7.3 Path-keyed durable-store migration

Before attachment relocation stops rewriting state, inventory every store that
currently treats a canonical path as logical project identity. Add a stable
`project_id` field and dual-read old rows for a bounded schema epoch. The
logical backfill includes knowledge, gaps, threads, notes, pins, roadmap,
packets, Slack bindings, project-scoped artifacts, proposals, and whiteboards.

Do not blanket-convert execution targets. Team `project_dir`, poller and cron
working directories, workflow ambient cwd, render output locations, and
watcher roots remain attachment/workspace data. They gain a project or
attachment reference only where needed for resolution while preserving their
execution path.

A persisted `LegacyPathBinding` ledger inside the strict host-local
`AttachmentSnapshotV1` records the v1 inventory's historical path, mapped
project id, relationship (`root` or `contained_subdirectory`), source
store/row id, and inventory epoch. Mapping uses the registered-root snapshot
captured at inventory time with deepest-containing-root semantics, not the
current attachment path and not a freshly computed path hash. This explicitly
covers the known legacy quirk where state was written under a plain
subdirectory rather than the registered root. Ambiguous rows are quarantined
with an operator mapping action. The path-free `ScopeMigrationRecord` lives in
the catalog; the path-bearing binding never does.

Rows whose literal project/path never belonged to any inventory-time
registered root are `UnscopedLegacyPath`, not catalog migration failures. They
retain the bounded raw string in a typed non-catalog lane, grant no attachment
or corpus authority, and remain queryable only through exact/raw legacy store
semantics. Doctor and the ledger report bounded counts by store, never path
labels. The operator may explicitly map or delete them later. They do not block
the cut of the mappable path fallback because the new typed lane no longer
consults catalog path resolution. New unregistered coordination rows, where a
surface still permits them, are written directly in that lane.

Bindings are append-only through the compatibility epoch. Attachment relocation
adds the new path but retains old bindings, so old rows continue to resolve
after rename. New writes use project ids immediately. The path fallback is
removed only after the mappable ledger is complete, its ambiguity quarantine
is empty, every unmappable row is classified `UnscopedLegacyPath`, and
checkout-access observations show no compatibility reads for the required
window.

## 8. Built-from response contract

Land this phase before the identity cut.

Add a response-level table whose entries are stable within one assembled view:

```text
BuiltFromStamp = Published {
    published_scope,
    published_ref,
    publisher_commit,
} | CheckoutOverlay {
    published_scope,
    checkout_id,
    publisher_commit,
    checkout_head,
    merge_base,
    working_fingerprint,
}
```

Each stamp receives a response-local id. Knowledge and gap rows point to that
id; a response emits each distinct stamp once. The internal
`KnowledgeStore.built_from` map is not reused as the wire contract. A content
row with no provable stamp is returned only in the explicitly labeled legacy
compatibility lane and carries a diagnostic; it is never mislabeled as
published.

The response schema addition is additive. Existing textual renderers append a
compact `built_from` section without repeating stamps per row. Structured
store helpers include `built_from_ref` and the table. Search/indexed responses
continue to use their existing clean snapshot and head fields, but expose them
through the same response wrapper where those surfaces already return
knowledge or gap rows.

Tests prove that a response merging published, own-checkout, and peer-checkout
rows has three distinct stamps, stable row references, a working fingerprint
for dirty bytes, and no accidental reuse after a publisher commit advances.

## 9. Checkout-access authority and instrumentation

### 9.1 Validated leases

Introduce a `CheckoutAccessBroker` in the daemon/indexing boundary. A caller
requests `(project_id, attachment selector, access kind, read/write intent)`.
The broker revalidates attachment status, checkout identity, scope, capability,
and the conservative path gate, then returns a `ValidatedCheckoutLease`.

Access kinds are closed and code-owned:

- local project walking;
- Git history;
- publisher/config tree read;
- knowledge or gap overlay read;
- blame;
- render/file provider;
- provenance note I/O;
- artifact watch/discovery; and
- repository mutation.

The lease owns the validated roots and safe relative-path join helpers. Path
consumers stop accepting raw `canonical_path` or `ProjectRecord`. Collected
staging, catalog lookup, corpus search, graph reads, and collected rebuild have
no lease parameter and therefore no filesystem authority.

### 9.2 Observability and proof

Every successful lease acquisition records operation kind, outcome, source
lane, and a monotonic counter before returning a path. Metrics contain no
absolute path and no unbounded path-derived label. Structured debug logs may
include bounded project and attachment ids, subject to normal log controls.

Expose current counters, last-success time per operation kind, denied counts,
and active compatibility lanes through doctor/health. Persist a small
roll-forward observation snapshot so restart does not erase the evidence used
to retire adapters. It is operational evidence, not durable corpus identity.

Tests inject `DenyCheckoutAccess` and assert zero acquisitions for remote-only
upload, activation, restart, full rebuild, search, graph, and GC. An acceptance
check also rejects new `canonical_path`, direct project-root `std::fs`, or Git
process use inside the collected-source adapter. The static check supplements,
but does not replace, lease-level runtime tests.

## 10. Path-free code indexing and read views

### 10.1 Source-neutral project input

Replace `StageCollectedGeneration(ProjectRecord, ...)` with:

```text
CodeProjectIdentity {
    project_id: ProjectId,
    scope: ProjectScope,
    display_name: String,
    repo_history: Option<RepoHistoryRecord>,
}
```

Local staging receives this identity plus a validated local-source lease.
Collected staging receives the identity plus an immutable manifest and blob
resolver, and rejects `LegacyLocal` before accepting a generation. It cannot
receive a checkout path.

Published Git local projects retain the versioned clean-snapshot derivation
bound to project id, recorded scope, commit namespace, `HEAD`, and working-tree
fingerprint. A v2-created attached Git `LegacyLocal` project has the random
local-only commit namespace from section 5.1; a non-Git `LegacyLocal` project
has no commit documents. Neither kind uses a head-bound clean snapshot for code
identity. Its local code snapshot id comes from
`legacy_local_snapshot_id(project_id, manifest_digest)`: a domain-separated
SHA-256 folded to the existing lowercase colon-free snapshot-id shape.
`manifest_digest` hashes the sorted normalized relative path, content hash, and
supported-file metadata for the complete local generation. The digest never
embeds project id or delimiters directly in an entity-ref component. This is
the same complete manifest used by purge, so incremental and full rebuild
converge.

The local freshness cache retains normalized path, size/mtime fingerprint, and
last content hash. An incremental event re-reads and re-hashes only changed or
uncertain files, then folds the cached complete manifest into the new digest;
full rebuild validates every entry. An attached Git `LegacyLocal` project may
refresh history through its validated Git-history lease under its local or
imported legacy namespace. That refresh grants no published-scope or cross-host
authority. Detach preserves the last history generation as stale and blocks
refresh. Non-Git local walking remains a first-class lease-backed source lane.

All new writers emit the strict scope-bearing activation and
retained-generation metadata introduced by the Phase 1 migration substrate.
Startup validates exact agreement among catalog project, activation,
descriptor, manifest, and source assignment before selecting a generation.
Legacy scopeless records are accepted only as offline migration input and are
rewritten transactionally before the first v2 bind. Recovery never scans
checkout paths or guesses a scope from a project id.

### 10.2 Index schema and response paths

Bump the project-file materialization/schema version and store:

- typed `project_id`;
- normalized project-relative path;
- stable logical `source_uri`;
- source kind and active generation selector;
- content and chunk hashes;
- recorded published scope where available; and
- existing clean snapshot/head fields needed by indexed responses.

No new document stores a checkout absolute path as project or file identity.
Relative paths are normalized component-wise, reject traversal and platform
prefixes, and use slash separators. Embedding text may include the stable
display name and relative path, never a host root.

At the response boundary, return structured `project_id`, `relative_path`, and
`source_uri`. Render `display_path` in this order:

1. session workspace mapping, when one exists later;
2. explicitly selected operator attachment for local UI output; or
3. accepted project alias/display name plus relative path.

The machine `source_uri` never changes when aliases or attachments change. Its
encoding is normative: normalize and validate the UTF-8 relative path first,
split only on `/`, then percent-encode each segment's UTF-8 bytes except ASCII
`ALPHA / DIGIT / "-" / "." / "_" / "~"`, using uppercase hexadecimal. Slash
separators are not encoded. Apply no Unicode normalization. Decode exactly
once, require canonical re-encoding equality, and reject empty, `.`, `..`,
encoded slash/backslash, NUL, control, or platform-prefix segments. Round-trip
tests include spaces, `%`, `#`, `?`, and non-ASCII names.

Absolute input filters are accepted only through an attachment resolver. The
raw substring lane for unregistered cwd/project selectors is permanent. New
transcript and tool-call documents whose cwd resolves to no attachment keep
the bounded literal cwd/project field and have no catalog/base-project id.
Search with an unregistered absolute selector performs the existing literal
substring match over that field and never obtains a lease. Catalog project
selection uses exact id plus relative path. The legacy absolute fields in
project-file documents remain overlap-only, but removing them does not remove
the transcript substring lane.

### 10.3 Full, incremental, purge, and readers

Source planning iterates a pinned catalog snapshot, not attachments. For each
project it chooses the persisted effective source:

- `collected`: rematerialize the active immutable generation without a lease;
- `local`: require the selected validated local-source attachment;
- `cutback_pending`: continue using the active collected generation;
- `unavailable`: preserve the last-good committed view or omit a project with
  an explicit health state, never purge it as an empty local root.

Full rebuild inventories every active collected generation before replacing
the index. A missing or quarantined blob aborts the replacement and preserves
the complete last-good index. Incremental purge keys by project id, source
kind, generation, and relative document identity, not canonical path.

Seed code selectors, vector selectors, edge registered-project sets, search
filter validation, and `CodeReadView` from the catalog. A request pins one
catalog epoch, active code selector, vector selector, and edge snapshot. A
remote project cannot be indexed yet hidden because it lacks an attachment.

## 11. Git history becomes an optional overlay

Remove Git-current staging from the collected code activation transaction.
Code activation commits code documents, code edges, vectors, and its selector
without opening Git. Git failure cannot roll back a valid collected generation.

Git history becomes an attachment-backed immutable overlay with identity:

```text
GitOverlaySelector {
    project_id,
    code_generation,
    repo_history_generation,
    attachment_id,
    repo_head,
    commit_namespace,
    overlay_generation,
}
```

`CodeReadView` selects the active code snapshot plus either a matching Git
overlay or `None`. Activating a new code generation without a usable attachment
atomically clears the selected current-file overlay. Old
`COMMIT_TOUCHED_FILE` edges cannot target the new snapshot. Commit documents
remain immutable historical facts but report stale/unrefreshable health when
the source attachment is absent.

For a Git repository with multiple catalog projects, ingest repo-level commit
documents once per authoritative repo and preserved commit namespace. Map
changed repo-relative paths into each project's `bbox_root_relpath`, then emit
project-specific file edges only for paths inside that project. This removes
the current duplicate monorepo ingestion and last-writer-wins commit record.

That consolidation applies only after recorded authority or an explicit
same-repository migration proof gives the projects one shared history record.
Unpromoted `LegacyLocal` monorepo siblings keep separate project-bound history
records and may perform duplicate local walks. The design accepts that bounded
cost rather than merging authority from path proximity; promotion or explicit
history resolution can consolidate them later without rewriting existing refs.

The first consolidated repo-history generation never seeds from one legacy
per-project `last_ingested_sha`: those values are commit identities, not an
ordered cursor, and monorepo siblings may disagree. Migration inventories and
backs up all old cursor files for diagnostics, initializes the new
repo-history record without a cursor, performs one complete reachable-history
walk through a validated attachment, publishes the complete generation, and
only then records its new cursor. Divergent sibling cursors therefore trade one
bounded rewalk for proof that no commit interval was skipped.

Repo-level commit documents and vectors have their own immutable
`RepoHistoryGeneration`, keyed by stable `RepoHistoryId` plus the catalog's
primary commit namespace. Per-project Git overlays reference that generation;
they do not own its documents. A repo-history manifest lists all catalog
project and retained-overlay references. Detach releases no repo-history
reference. Project retirement removes only that project's overlay reference.
Commit documents and their vectors are eligible for tombstone/GC only after no
catalog project, active or retained overlay, pinned read view, or in-flight
history build references the repo-history generation. Vector tombstoning is
driven by the repo-history generation, never by one project's code selector.

Compatibility namespaces from section 5.1 remain queryable through the
repo-history record. They are retired only by an explicit namespace migration
that proves no durable refs or retained generations remain; ordinary project
detach or retire cannot delete them.

The repo-history reference manifest is a derived GC acceleration index, not
authority. Persisted catalog records plus active/retained Git overlay selectors
are its durable inputs. Startup and every crash-recovery path rebuild and
checksum it from those inputs before enabling history GC. A mismatch disables
GC and reports doctor failure but does not hide otherwise-valid history reads.
In-memory pinned read views and in-flight builders are added to the rebuilt
durable reference set while the process runs. Therefore a crash between an
overlay selector swap and manifest refresh cannot prematurely free a history
generation.

A later matching attachment can build an overlay against the active code
generation and swap it atomically into new read views. Attachment removal does
not corrupt code search. History health distinguishes current, lagging,
unavailable-no-attachment, invalid-scope, and failed-last-refresh.

## 12. Collector authority and cutback state machine

### 12.1 Catalog-based grants

`src/server/code_source.rs::build_snapshot` receives a catalog snapshot. Each
configured `PublishedScope` must resolve to exactly one existing published
catalog project. Zero, duplicate, or legacy-local matches fail the candidate
configuration. It performs no checkout or committed-config read.

The server still binds a token to producer id and allowed scopes. Its internal
grant maps scope to the catalog-selected project id. The request carries only
scope. An upload cannot create, rename, attach, select, or delete a project.

Config reload first validates tokens, duplicate producer/scope assignments,
catalog references, limits, and the complete candidate auth table. Failure
retains the prior table. Success swaps auth immediately, independently of
source transition work. Catalog mutations are never a side effect of reload.

Cold open matches the collector design's fail-closed policy. If collection is
enabled and any configured scope has no exact published catalog project, token
validation fails, or assignments conflict, startup fails before binding HTTP.
Deployment ordering is catalog add/import first, then producer assignment.
Destructive catalog retire refuses while any current configured assignment
references the project; the operator removes the assignment and observes the
resulting cutback/retirement state first.

Scope migration uses a fixed live sequence so catalog and grants never disagree
silently:

1. Remove the old-scope producer assignment and reload. This revokes its token
   authority; the collected generation remains effective or cutback-pending.
2. For an attached project, run `bbox_project_scope_migrate --dry-run`, then the
   attachment-proven MCP migration. For a project with zero attachments, stop
   the daemon under shared-service approval and run the offline
   `blackbox project-catalog scope-migrate --operator-attested` dry-run/apply
   while holding the exclusive lifetime lock, then restart. Either path leaves
   the bridge readable under truthful old provenance while the catalog accepts
   only the new scope.
3. Update the checkout's producer config/token allowlist to the new scope and
   reload. Cold open or reload fails closed if this is attempted before step 2.
4. Publish and activate a complete new-scope code generation, then rebind and
   advance publication as applicable. Each success clears its bridge
   capability; old generations retire through normal journals.

Restart after any step is defined: no old-scope request authenticates after
step 1; the migration record restores bridge selection after step 2; new-scope
auth is valid only after step 3; activation remains atomic after step 4.

### 12.2 Desired versus effective source

Persist source state by project id:

```text
desired = Collected(producer) | Local

effective =
    Local { attachment_id, generation }
  | WarmingCollected { retained_local, staged_generation }
  | Collected { generation }
  | CutbackPending { generation, reason }
  | Unavailable { reason, retained_generation? }
```

An assignment addition changes desired authority, then warms and activates as
the existing collector design requires. An assignment removal changes desired
authority to local and revokes the former token before cutback.

Cutback requires one explicitly selected, scope-matching local-source
attachment and a successful complete local generation. If none exists, persist
`CutbackPending::NoLocalAttachment`, retain collected results, and wait for an
attachment/config event. `NoLocalAttachment`, `AmbiguousAttachment`, and
`ScopeMismatch` are structural reasons and never poll on a timer. A matching
attachment event attempts cutback. Re-adding the assignment cancels pending
cutback and retains or refreshes collected authority.

Failures after a valid attachment is selected are transient or terminal.
Transient writer contention, I/O pressure, deadline, and index-commit failures
persist attempt count, last error class, and `retry_not_before`, then use capped
exponential backoff with jitter. The bound is configuration-owned and tested;
after the cap, state becomes `ManualRetryRequired` and an explicit operator
retry or config event is needed. Validation/security failures are terminal and
never retry automatically.

Startup re-drives `WarmingCollected` from its durable upload journal, validates
`Collected`, re-evaluates every structural `CutbackPending` once against the
current attachment epoch, and schedules any due transient retry. SIGHUP does
the same after auth swap. `Local` validates its attachment and preserves the
last-good local selector if unavailable. `Unavailable` remains so until its
named event or explicit retry. No state spins on restart or loses the active
collected generation.

Only an explicit destructive project-retire action can remove the last
collected generation without a replacement. It inventories references and
states exactly which search, graph, vector, and history materializations will
be removed.

## 13. Knowledge, aliases, and publisher authority

### 13.1 Published and provisional reads

Key published knowledge/gap caches by catalog project, `PublishedScope`, full
publisher ref, and accepted commit. Hydrate logical project metadata with
`project_id`, not `canonical_path`. A remote-only project can query the last
accepted published snapshot already in corpus state. Absence of a new
publisher attachment degrades publication freshness only; it does not remove
the catalog project or collected code.

The durable source of that promise is a strict `AcceptedPublicationStore`
beside the catalog state, not the current in-memory TTL cache. One immutable
publication generation contains normalized knowledge entries, normalized gap
entries, canonical relative-filename manifests for both lanes, scope, full ref,
accepted commit, per-file/content hashes, counts, and total encoded bytes. The
manifests preserve the filename presence and equality inputs needed by overlay
and promotion logic. One atomically replaced
`AcceptedPublicationPointer` contains the complete publisher binding and the
selected current generation plus the bounded prior pointer used for rollback.
There is no separately committed binding/manifest pair.

Publisher advance validates both lanes, writes and fsyncs their bounded
immutable payloads, then compare-and-swaps the single pointer. A partial or
invalid knowledge/gap generation never advances either lane. A crash before
the pointer swap leaves the old binding and generation; a crash after it finds
the complete new immutable generation. Startup requires the pointer's scope,
ref, commit, generation id, and hashes to agree with the payload. Disagreement
fails the published capability closed to the last retained pointer whose full
generation verifies; it never combines fields across epochs.

Loads enforce the existing per-entry limits plus configured maximum entry
count and total encoded bytes for each lane. Startup verifies the selected
manifest and hashes before populating the in-memory cache. Corruption fails the
published capability closed while leaving code search available and surfacing
doctor failure. GC retains the selected generation, a bounded rollback
generation, pinned read views, and in-flight publication; project retire
inventories and explicitly removes these generations only after all knowledge
and gap refs are discharged. Restart with no attachment serves the selected
accepted generation.

Provisional overlays remain keyed by `(PublishedScope, checkout_id)` and stay
host-local. `own` uses the authoritative session attachment. `all` means all
valid overlays on this corpus host, not overlays on remote checkout hosts. The
new response stamps make that distinction explicit.

After publisher detach, the verified accepted generation remains published
truth and names accepted commit P plus the canonical published file manifests.
It does not fabricate Git ancestry, a merge base, or Git objects. Each
attachment may recompute its overlay only when its own object database contains
P and can prove and read `B = merge_base(H, P)`. It never silently borrows
another attachment. A previously valid overlay remains eligible only while the
accepted pointer still names the same P and its attachment lease, checkout
HEAD, and working-tree fingerprint all revalidate unchanged. Otherwise it is
`overlay_baseline_unavailable`.

`published` continues from the accepted generation. An explicit or default
`own` request for an authoritative checkout whose baseline is unavailable
returns `provisional_overlay_unavailable`. `all` serves published plus every
valid peer, omits unavailable peers, and lists them in structured
`degraded.overlays`. Health reports accepted-publication integrity,
publisher-advance availability, and per-checkout overlay-baseline availability
separately. No live publisher blocks advance and alternate-object assistance,
not accepted published reads.

### 13.2 Publisher binding

Replace path-scan election with an operator-owned binding:

```text
PublisherBinding {
    project_id,
    attachment_id,
    full_ref,
    accepted_commit,
    accepted_scope,
    accepted_generation,
}
```

This logical binding is serialized inside the single
`AcceptedPublicationPointer` keyed by the catalog project.
The selected attachment must revalidate to the same scope. The full ref may
advance only after the new commit is resolved and that commit declares the
same scope; the accepted commit and derived snapshot swap atomically. Changing
checkout `HEAD`, branch, config, or adding another clone cannot elect a new
publisher. Detaching the bound attachment leaves the last accepted snapshot
and reports publisher-unavailable until an operator rebinds.

Migrate existing `PublisherRefStore` pins by resolving their current unique
publisher under the old rules during preflight. Ambiguous or missing matches
block automatic migration and require an explicit binding to one inventoried
attachment, full ref, expected scope, and resolved commit. Every old pin maps
to one seeded binding or one explicit no-content acknowledgement. The exact
legacy store bytes and checksum remain rollback input, and v2 never consults
that store.

Apply seeds publication generation G1 for every migrated binding before the
catalog epoch commits. It resolves the pinned full ref, validates the exact
commit and scope, reads knowledge and gaps through the migration command's
explicit publisher/config-read lease, writes the immutable accepted generation,
and installs its pointer. G1 payloads are immutable transaction assets and
pointers are mutable migration participants. The prepared journal inventories
every payload and pointer hash; the committed marker retains them as GC roots.
Forward recovery requires them all. Rollback leaves immutable assets
unreachable from v1 and pinned for bounded orphan GC. A binding that cannot be
seeded is a preflight
refusal unless the resolution file explicitly records
`no_published_content_acknowledged`; that choice creates no binding and reports
the published capability unavailable. It never enables a committed-tree
fallback. The completion gate requires every project expected to publish to
have a seeded generation. Restart immediately after apply, before any advance,
must serve G1 with no attachment.

Committed `.bbox/config.toml` aliases become nominations read from the exact
accepted publisher commit. They cannot overwrite operator aliases or become
active on conflict. Existing materialized aliases migrate as operator aliases
to preserve selectors. Accept/reject is an explicit catalog action, and alias
uniqueness is checked against both ids and active aliases. Registration,
publisher advance, and reload report pending nominations and the exact
epoch-checked accept command. A missing or changed committed declaration never
silently revokes an accepted alias, and a remote-only project preserves its
accepted aliases without opening a checkout.

## 14. Remaining checkout-side adapters

The following may remain after the identity cut only as explicit lease
consumers:

| Surface | Corpus identity | Checkout behavior without attachment |
|---|---|---|
| Local source walker | `project_id` + generation | source unavailable or retained last-good view |
| Repo knowledge/gap publisher | project + scope + accepted commit | retain last accepted snapshot, report unavailable |
| Git history | project/repo + code generation | no current-file overlay, stale commit docs labeled |
| Blame | project + relative path + requested commit | `attachment_required` or commit mismatch |
| Render/file provider | project + relative refs | `attachment_required` |
| Provenance note import/export | stable project refs | `attachment_required` for Git note I/O |
| Init/eject/mutation/refactor | stable project selection | `attachment_required` or write-gate denial |
| Artifacts/watchers | project-stamped artifacts | no watcher; retain durable catalog metadata |
| Tool/transcript edges | catalog project id + relative anchor | unresolvable path event is diagnosed, never re-id'd |

Blame must verify that the selected attachment contains the requested commit or
snapshot; it cannot blame arbitrary attachment `HEAD`. Render filters by
project id immediately even while output remains attachment-side. Provenance
plan generation stays corpus-only; only legacy Git-note I/O acquires a lease.

Every adapter returns a bounded structured error or degraded health state. No
adapter silently selects another checkout, falls back from collected to local,
or reconstructs a host path from catalog data.

## 15. Dependency order and implementation phases

### Phase 0: provenance and access prerequisites

1. Add response `built_from` types and response-local table/reference wiring.
2. Wire published and overlay knowledge/gap assembly to exact stamps.
3. Add checkout access kinds, broker, counters, doctor output, and test probes.
4. Migrate current root reads to leases without changing project identity.
5. Add the process-lifetime migration lock while retaining version-1 store
   compatibility, so this phase can be deployed as the bridge release.

Exit gate: merged-view stamp tests pass, every named checkout surface is
counted, and a remote-shaped test path can deny access deterministically.

### Phase 1: pure model, strict stores, and migration preflight

1. Add typed ids, catalog, attachment, scope-authority, bootstrap-hint, and
   commit-namespace types to `bbox-corpus-core`.
2. Add versioned pure decoders and full-store validation.
3. Implement the journaled catalog/attachment transaction owner and its
   role-bounded multi-participant migration mode in `bbox-indexing`.
4. Add migration-only accepted-publication persistence, G1 seeding, source
   selection quarantine, scope-bearing activation/retained metadata rewrites,
   and the journaled migration marker.
5. Implement the offline v1 preflight/apply command, hash-bound resolution
   file, fault-injected isolated rehearsal, backups, and collision
   refusal/retry.
6. Preserve `ProjectRecord` only as an attached compatibility view.

Exit gate: every existing id/ref/selector namespace round-trips, no catalog
serialization contains a path, every old publisher pin has a G1 or explicit
no-content disposition, and every torn participant transaction recovers one
coherent state.

### Phase 2: resolver and administration cut

1. Implement the shared catalog/attachment resolver and route all selector
   surfaces through it.
2. Add catalog, attachment, publisher, alias, detach, and retire administration.
3. Add audited LegacyLocal promotion and scope migration, then convert register,
   rename, unregister, init, and eject semantics.
4. Add project-id fields and the bounded compatibility ledger to path-keyed
   logical stores, without rewriting execution targets.

Exit gate: id/alias/scope corpus queries work with no attachment; path
operations select exactly one valid attachment; ambiguity and unknown paths
fail closed. This phase proves the v2 runtime path against isolated migrated
state; it does not apply v2 bytes to configured operator state.

### Phase 3: path-free index and optional Git overlay

1. Add source-neutral project input and the relative-path schema.
2. Remove path and Git access from collected staging.
3. Iterate catalog projects for full/incremental rebuild, purge, selectors,
   vectors, edge registration, and read views.
4. Split repo-owned commit generations and project current-file edges into
   reference-counted matching optional overlays.
5. Add source URI and boundary display rendering.

Exit gate: a remote-only fixture activates, rebuilds, searches, and exposes
graph data under `DenyCheckoutAccess`; new docs contain no host path.

### Phase 4: catalog-based collector and state transitions

1. Resolve grants from catalog scope only and make every live activation writer
   emit the strict scope-bearing v2 record.
2. Separate auth swap from desired/effective source changes.
3. Implement persisted no-attachment cutback pending and event-driven resume.
4. Validate catalog/activation/descriptor/manifest agreement at startup.

Exit gate: token revocation succeeds while collected results remain pending;
reattach, reassign, restart, and explicit retirement converge exactly once.

### Phase 5: publisher, views, and remaining adapters

1. Wire the migration-seeded publisher bindings and accepted generations into
   live views, rebind, and advance.
2. Key accepted knowledge/gap views by catalog identity and stamps, including
   per-checkout overlay-baseline degradation after publisher detach.
3. Move blame, render, provenance notes, file providers, artifact watchers,
   refactor/mutation, and tool-edge path resolution to leases.
4. Surface capability-specific health and typed attachment errors.

Exit gate: no corpus-only request requires `ProjectRecord`; every remaining
checkout open is lease-counted and remote-only projects degrade per capability.

### Phase 6: overlap proof and cut

1. Delete direct `load_project_records` consumers and eight-hex selector
   assumptions, then prove the complete v2 binary against isolated migrated
   state.
2. Observe compatibility-path and direct-checkout counters through the agreed
   bridge closeout window and exercise cutback.
3. Run the complete isolated multi-participant migration rehearsal, durable
   backfills, new-index rebuild, quarantine handling, and exact post-image
   verification against a final copied inventory.
4. Through the shared-service approval runbook, stop the bridge and apply to
   configured state under the exclusive lifetime lock.
5. Before any v2 route binds, run the offline durable-store backfills and
   path-free index rebuild against the applied catalog, preserving active
   collected generations, then require the complete startup validation gate.
6. Start the v2 daemon and run the catalog-only, cutback, and adapter live
   checks.
7. Retain the version-1 backup and compatibility reader until a later explicit
   retirement after rollback proof.

Exit gate: the daemon can start with an empty attachment store and serve all
catalog-only collected code; the remaining nonzero checkout operations match
the explicit adapter table.

## 16. Concurrency, recovery, and security invariants

- Catalog, attachment, source authority, code selector, vector selector, Git
  overlay, and edge snapshots are immutable request inputs once a read view is
  pinned.
- No lock is held across filesystem walking, Git, embedding, or index commit.
  Mutation prepares off-lock and compare-and-swaps against the catalog/source
  epoch before publication.
- Lock order is the process-lifetime migration lock, the existing
  `projects.json.lock`, then auxiliary participant locks in deterministic
  role/path order. Preflight and the bridge persister contend on that same
  project-store lock.
- A project detach racing local indexing invalidates the lease before commit;
  the last-good read view remains selected.
- A collected activation racing detach is unaffected because it has no lease.
- Authentication happens before bounded request parsing, and scope membership
  is checked before any durable upload mutation, as in the collector design.
- The catalog id contract is safe in filenames and URIs. Relative paths are
  normalized and checked again at every blob/materialization boundary.
- Tokens, checkout paths, producer labels, aliases, and Git refs are never
  incorporated into random id generation.
- Catalog scope authority never comes from `aka_repo_ids`, repository URL,
  computed hash, or a request body.
- Historical scope aliases and former recorded authorities are provenance only;
  no auth, publisher, attachment, or write resolver consults them as authority.
- Unattached scope migration is operator authority equivalent to explicit
  catalog import, requires zero active attachments plus acknowledgement and
  audit reason, and is never reachable from producer or model-facing routes.
- Same-user MCP administration is delegated automation, not authenticated
  human identity. Attachment proof, expected epochs, acknowledgement
  pass-through, and audit records constrain it. Proofless authority remains
  local CLI-only.
- Corrupt catalog, attachment, accepted-publication pointer/generation,
  migration journal, or source selector fails closed for its capability. A
  repo-history reference-manifest mismatch disables GC until deterministic
  rebuild. A missing optional attachment degrades only its capability.
- Accepted publication never substitutes content for Git ancestry. A missing
  overlay baseline degrades only that checkout's provisional view while the
  verified published generation remains readable.
- GC pins active read views, active/retained code generations, the selected Git
  overlay, in-flight rebuild inventories, prepared migration journals,
  committed migration markers, G1 assets, quarantined generations, and
  migration backups. General storage sweeps exclude transaction stage and
  backup roots.

## 17. Verification matrix

### Identity and migration

- A version-1 singleton import preserves project id, aliases, timestamps,
  commit namespace, every project-file/symbol/commit/provenance ref, activation,
  vector selector, edge manifest, artifact key, and sidecar location.
- Missing-path, non-Git, shallow, and uncommitted-authority records survive as
  `legacy_local` without manufactured scope.
- Duplicate scope, id collision, alias collision, mismatched active descriptor,
  scope-changing rename, corrupt catalog, dangling attachment, and malformed id
  all refuse before replacing v1 state.
- A refused preflight emits an inventory-hash-bound report; a valid survivor
  mapping leaves losing ids LegacyLocal; apply succeeds on retry; changed v1
  state invalidates the mapping; v2 startup before apply gives the offline
  command and does not bind.
- The v1-compatible bridge holds the lifetime migration lock, a concurrent
  apply refuses, and apply succeeds only after bridge shutdown releases it.
- A duplicate-scope loser with active collected state requires explicit
  quarantine, is absent from v2 read views at first bind, and completes normal
  retirement without serving the winner's scope.
- Fault injection at every regular pair and migration-participant boundary
  proves recovery to one complete old or new state. A pair installed without a
  marker is recovered through the prepared journal or fails closed.
- Preflight and a bridge `StorePersister` contend on the same
  `projects.json.lock`; apply-time source hashing detects every auxiliary input
  change.
- Every old publisher pin produces exactly one verified G1 pointer or one
  explicit hash-bound no-content acknowledgement. V2 never consults the legacy
  publisher-ref store, whose exact bytes remain rollback-pinned.
- Missing checkout markers receive their persisted planned random id
  idempotently. Shared monorepo roots receive one id; a different, malformed,
  unreadable, or symlinked marker refuses without overwrite.
- Every surviving legacy collected activation and retained-generation metadata
  gains explicit scope only from exact descriptor/manifest/catalog agreement.
  First v2 startup selects it without a compatibility guess; any ambiguous join
  refuses preflight.
- Typed legacy-path observations classify every durable row from the captured
  literal selector. Default reports contain only path digests; an explicit
  owner-only local review artifact is marked sensitive.
- A fresh-v2 catalog opens without a migration marker. A migrated-v1 catalog
  records its transaction id in `CatalogOriginV2`, opens only with the matching
  committed marker, and detects marker loss after commit. The marker and
  journal retain the complete plan hash.
- New ids use the fixed random format, collision-check, and resolve by exact
  membership rather than string shape.
- New published repos use recorded authority as one shared commit namespace;
  migrated primary and compatibility namespaces round-trip without rewriting
  refs and remain stable after moving the catalog to another host.
- A migrated published repo with an unambiguous materialized legacy namespace
  and the same repo imported LegacyLocal then promoted both keep that legacy
  primary. A new repo with no materialized namespace uses recorded authority.
- Two distinct repos forced to share one weak namespace fail preflight. An
  operator split creates separate history records, quarantines the ambiguous
  old namespace, prevents unqualified commit resolution, and cannot merge their
  history or GC ownership.
- Two migrated LegacyLocal monorepo projects sharing one legacy history record
  retain reads when one promotes: the stable history id remains, authority
  becomes recorded, both references remain valid, and conflicting sibling
  authority blocks the transaction.
- A newly registered attached Git `LegacyLocal` project ingests and queries
  history under a random local-only namespace, survives detach as stale
  history, refreshes after reattach, and preserves its refs through promotion
  without acquiring publisher or producer authority early.

### Resolution and administration

- Id, alias, scope, base path, nested monorepo path, linked worktree, and
  managed clone converge on one catalog id where authoritative.
- Same-scope multiple attachments leave corpus reads unambiguous but require a
  session or operator selection for path work.
- Equal-depth and alias conflicts fail closed. Unknown absolute paths never
  create a project.
- Detach preserves the project, entities, source generations, knowledge, and
  project-scoped state. Catalog delete refuses live references.
- Rename cannot change scope. Reattaching legacy-local state without scope
  requires an explicit id.
- Explicit LegacyLocal promotion preserves project id and state, invalidates a
  racing old-epoch generation, and refuses rather than forking when the scope
  already has an owner.
- Markerless LegacyLocal rename refuses; checkout-init or explicit detach and
  reattach supplies the operator proof instead of path/inode inference.
- A monorepo relpath move makes register return `scope_migration_required`;
  audited migration preserves project id, attachments, logical stores,
  collected generation, accepted publication, path bindings, and repo-history
  record. Old results retain old-scope provenance until new-scope refresh.
- Scope migration to an already-owned target refuses without modifying either
  project. Fault injection yields the complete old or new catalog/attachment
  epoch plus its matching migration record.
- Scope migration stores its path-free record inside the catalog and its
  path-bearing historical bindings inside the attachment snapshot. Fault
  injection cannot publish one without the other.
- A remote-only collected project with zero attachments uses the offline
  `operator_attested` CLI after explicit acknowledgement, preserves
  project id, bridge, active collected generation, and durable state through
  the four-step producer re-scope, then clears the bridge on new activation.
- Unattached migration refuses when any active attachment exists, when the operator
  acknowledgement/reason is absent, or when the target scope is already owned.
- A recorded-authority change requires the explicit acknowledgement, preserves
  the established commit namespace, records the former authority only as
  non-authoritative history, clears current-file Git edges, and rebuilds under
  the new authority.
- Historical path bindings keep root and contained-subdirectory legacy rows
  readable after attachment relocation, and an ambiguous row has a tested
  resolution path.
- Never-registered raw-path rows become counted `UnscopedLegacyPath` values,
  remain exact-queryable without authority, and do not block the mappable-lane
  cut.
- The attached-only legacy list reports omitted remote projects, while catalog
  list/get returns the complete inventory without fake paths.

### Response provenance

- Published, own, and peer rows each reference the correct deduplicated
  response stamp.
- Dirty overlays carry a working fingerprint and do not claim checkout `HEAD`
  as complete provenance.
- Publisher advance creates a new stamp; an in-flight response retains the old
  pinned table.
- Text and structured knowledge/gap paths expose the same stamps.

### Remote-only code path

- Explicitly add a catalog project, configure a scope grant, upload, stage,
  and activate with zero attachments.
- Restart, incremental indexing, full rebuild, lexical search, vector-only
  hybrid search, inspect expansion, graph discovery, and GC complete with
  `DenyCheckoutAccess` at zero.
- Active selectors and edge registered-project sets include the catalog-only
  id.
- Missing or quarantined active blobs abort replacement and retain the full
  last-good read view.
- Catalog, activation, descriptor, and manifest scope mismatch fails before
  selector or index commit.
- New documents and vectors contain relative paths/source URIs and no producer
  or corpus-host absolute path.
- Source URIs round-trip the normative byte encoding for spaces, `%`, `#`, `?`,
  and non-ASCII segments and reject non-canonical or traversal encodings.
- A non-Git LegacyLocal project survives local incremental/full rebuild, purge,
  and search with a colon-free content-manifest snapshot and no commit
  documents; its `ProjectFileV2` ref round-trips through the exact parser.
- An incremental LegacyLocal edit re-hashes only the changed/uncertain file,
  folds the cached complete manifest, and produces the same generation as a
  subsequent full rebuild.
- An unregistered-cwd transcript remains searchable through the permanent raw
  substring lane after all project-file compatibility documents are rebuilt.

### Git and cutback

- Code activation succeeds when Git is absent or fails.
- A matching attachment builds a Git overlay for the exact active code
  generation; a new code generation or attachment removal clears current-file
  edges atomically.
- Commit history ingests once per repo while two monorepo projects receive only
  their own file edges.
- Divergent legacy monorepo cursor SHAs seed no consolidated cursor. One full
  reachable-history walk publishes the initial repo-history generation before
  its new cursor is recorded.
- Retiring one monorepo project cannot tombstone shared commit documents or
  vectors while another project/overlay/read view references the repo-history
  generation.
- Crash between overlay selector swap and derived history-manifest refresh
  rebuilds references before GC and cannot free a live history generation.
- Assignment removal revokes the token and persists structural
  `cutback_pending` without a retry spin while collected search remains visible.
- Transient cutback failure backs off to its cap, resumes when due after
  restart, and then requires explicit retry; terminal validation failures never
  retry.
- Matching reattach completes cutback; reassign cancels it; mismatched or
  ambiguous attachment remains pending; restart preserves every state.
- Cold open fails before bind for an unresolved configured scope, and catalog
  retire refuses until the assignment is removed.
- The four-step producer re-scope sequence survives restart after every step:
  old auth remains revoked, bridge generations stay truthfully stamped, new
  auth resolves only after migration, and new activation clears the bridge.

### Checkout adapters

- Remote-only blame, render, file, eject, mutation, artifact-watch, and Git-note
  operations return typed capability errors without filesystem access.
- Publisher binding cannot change through branch switch, checkout `HEAD`, or a
  config edit. Detach retains the last accepted snapshot.
- Accepted knowledge and gap generations survive restart with no attachment;
  a partial/corrupt candidate cannot advance the manifest or accepted commit,
  and bounded GC preserves pinned/rollback generations.
- After publisher detach, a peer containing accepted commit P recomputes its
  overlay from its own Git database. A peer lacking P reports
  `overlay_baseline_unavailable`: `published` remains available, `own` returns
  the typed provisional error, and `all` omits only that peer with structured
  degradation. Restart preserves the same outcomes without inventing ancestry.
- Fault injection before/after the accepted-publication pointer swap yields a
  complete old or new binding/content epoch, never mixed provenance.
- Migration seeds G1 before catalog commit; restart before the first publisher
  advance serves G1 without an attachment, while an acknowledged no-content
  project has no binding and a clear unavailable state.
- Config aliases remain inactive nominations until explicit acceptance, while
  all migrated materialized aliases continue resolving.
- Every successful checkout access increments exactly one closed operation
  kind, and no metric contains a path label.
- The path-keyed compatibility ledger covers every logical store while
  execution targets remain attachment/workspace-bound.

## 18. Repository gates and bookend review

Use the project-owned formatter and workspace-wide nextest gates:

```text
scripts/fmt.sh --check
cargo check --workspace --all-targets
cargo nextest run --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --profile full
```

Run heavy verification through the project lane workflow against the pushed
ref. Do not use a cold dispatched worktree for the workspace gates.

Before implementation, a new Kimi plan-review session reads this complete
document, governing designs, baseline, and current code. Correct every finding
and resume that same session until its verdict is `PASS`.

After implementation, format and run proportionate local/single-crate checks,
commit and push, then run the full cluster verification on the pushed ref. A
separate new Kimi implementation-review session inspects the full fixed scope.
Correct findings, push and rerun the full gate, and resume the same review
session until it returns `PASS`.

## 19. Completion and next decomposition gate

This slice is complete when collected code intelligence for a durable catalog
project has no local-registration or local-path prerequisite, all corpus
selectors use stable catalog identity, every remaining checkout access is an
explicit observable lease, and detach cannot delete durable project state.

The daemon is not yet checkout-free. The next slices use the same scope-bound
producer credential infrastructure for typed Git-history/provenance and
published knowledge transports. Each replaces one adapter in section 14. Only
after checkout-access observations are zero outside intentional local mutation
and session-bound operations, plus cutback has been proven, may the legacy
adapters be retired and the corpus process moved off-host.
