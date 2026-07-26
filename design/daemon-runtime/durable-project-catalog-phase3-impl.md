---
title: "Durable project catalog Phase 3 implementation plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, project-identity, catalog, path-free-index, repo-history, git-overlay, migration]
brief: "Make code indexing iterate the catalog instead of attachments, cut the project-file schema to relative paths and source URIs, materialize immutable repo-history generations before any destructive index replacement, and split Git history into an optional attachment-backed overlay."
---

# Durable project catalog Phase 3 implementation plan

Date: 2026-07-25

Governing design: [`durable-project-catalog-impl.md`](durable-project-catalog-impl.md)
sections 10, 11, 15 (Phase 3), 16, 17. Companion:
[`distributed-code-source-collector-impl.md`](distributed-code-source-collector-impl.md)
sections 9.1 through 9.4;
[`durable-project-catalog-phase1-impl.md`](durable-project-catalog-phase1-impl.md)
section 6.2 (inventory), 7.3 (readiness artifact);
[`durable-project-catalog-phase2-impl.md`](durable-project-catalog-phase2-impl.md)
sections 3 (deferrals), 4.3 (parity discipline precedent).

## 1. Required outcome

At the Phase 3 exit gate, proved against isolated migrated v2 state and the
bridge parity harness:

1. A remote-only catalog fixture project with an active collected generation
   and zero attachments activates, incrementally rebuilds, fully rebuilds,
   serves lexical and hybrid search, exposes graph data, and survives GC with
   `DenyCheckoutAccess` asserted and zero lease acquisitions on the collected
   paths.
2. New project-file documents contain no corpus-host absolute path. They carry
   typed `project_id`, a normalized `relative_path`, a stable `source_uri`
   whose encoding is normative and round-trip tested, source kind, and the
   active generation selector. The response boundary returns the structured
   triple and a `display_path` rendered by the fixed fallback order.
3. Before the first destructive index replacement under the new schema, the
   pre-replacement materializer creates complete immutable
   `RepoHistoryGeneration` records for every proved namespace and
   `RepoHistoryQuarantineGeneration` records for every ambiguous or unclaimed
   namespace observed in the legacy index, verifies each against the persisted
   Phase 1 namespace commitments, and binds all of them in a durable
   `RepoHistoryRebuildManifestV1` with prepared and committed states. A
   namespace that cannot be proved keeps the replacement refused and the
   last-good lexical and vector views intact.
4. An attachment-less project's stale commit documents and vectors survive
   full index replacement, rematerialized from their authenticated history
   generation without any checkout access.
5. Collected code activation commits without opening Git. Git failure can no
   longer fail or roll back a valid collected generation. Git current-file
   edges become a post-activation overlay; in catalog mode the overlay is
   selected by a typed `GitOverlaySelector` and cleared atomically when a new
   code generation activates without a usable attachment.
6. Source planning, full and incremental rebuild, purge keying, selector
   seeding, edge registered-project sets, and background storage GC iterate a
   pinned catalog snapshot. The three empty-root purge hazards and the two
   registered-set divergences enumerated in section 2.4 are closed with tests.

Everything is still bounded by D-002: no v2 bytes are applied to configured
operator state, and the bridge daemon keeps behavioral parity except for the
enumerated changes in section 4.3.

## 2. Survey of the current tree

Code anchors verified at `f6c4747a`. This section is inventory, not intent.

### 2.1 Landed substrate Phase 3 builds on

- Collected staging already has the immutable-source seam the governing
  section 10.1 asks for: `stage_collected_project_generation`
  (`crates/bbox-corpus-index/src/index/project_files.rs`) takes
  `entries: &[ManifestEntry]`, a `GenerationDescriptor`, and an
  `open_bytes: FnMut(&ManifestEntry) -> Result<Vec<u8>>` closure; both call
  sites close over `CodeSourceStore::verified_blob_file`. Formalizing
  `CodeProjectIdentity` replaces the `ProjectRecord` parameter, not the blob
  plumbing.
- `ProjectRecordsSnapshot { records, corpus_project_ids, omitted_catalog_count,
  authority_epoch }` and `ProjectRecordsProvider` (P2-B) already deliver the
  complete catalog id set and an epoch. Five surfaces consume
  `corpus_project_ids` today (selector seeding, startup edge set, three
  storage tools); everything else still iterates the attached-only `records`.
- `collect_preserved_collected_documents`
  (`crates/bbox-corpus-index/src/index/project_files.rs`) is the existing
  strict verify-then-abort preservation gate: activation/manifest/generation
  agreement, live document count, and an entity-inventory SHA-256 recomputed
  and compared, failing closed with `preservation_failed` health before
  `delete_all_documents()`. The materializer verification vocabulary extends
  this shape instead of inventing a second one.
- Phase 1 captures per-namespace history evidence:
  `CorpusIndexMigrationSnapshotV1.commit_namespaces` (per-namespace commit
  document count plus canonical ordered commitment,
  `crates/bbox-corpus-index/src/index/migration_inventory.rs`) and
  `VectorMigrationSnapshotV1.commit_namespaces` (vector-key count plus
  commitment, `crates/bbox-vectors/src/migration_inventory.rs`), normalized
  into `LegacyCommitNamespaceInventoryV1` with
  `Proved | Ambiguous | Unclaimed` attribution
  (`crates/bbox-indexing/src/project_catalog_inventory.rs`).
- The migration facade already owns an immutable-asset mechanism:
  `MigrationImmutableAssetDraftV1` materialized under
  `project-catalog-migration-assets/`, hash-bound end to end through
  `predicted_immutable_asset_hashes` and the verification receipt
  (`crates/bbox-indexing/src/project_catalog_migration.rs`,
  `project_catalog_store.rs`).
- Crash-recovery precedent for a prepared/committed replacement journal:
  `PendingLocalActivationJournal` and
  `recover_pending_local_snapshot_activations`
  (`crates/bbox-edge-sidecar/src/snapshot.rs`,
  `crates/bbox-corpus-index/src/index/project_files.rs`).
- Content-addressed id precedent: `bbox_code_source::generation_id` (domain
  separator plus length-prefixed fields); store-record self-verification
  precedent: `StoredGenerationV2::validate` re-deriving its own id.
- `CodeReadView { active_selectors, searcher, edge_index }`
  (`src/server/state.rs`) is already an immutable per-request pin with four
  runtime republish writers, one startup initial construction in
  `src/server/open.rs`, and three test constructors (one test-gated in
  `src/tools/graph.rs`).
- `gc_blobs_for_scopes(catalog_scopes)` exists on the code-source store with
  zero production callers; the hourly maintenance thread calls the no-arg
  `gc_blobs()`.
- `bbox_code_source::validate_relative_path` already implements the
  section 10.2 normalization contract minus percent-encoding, with
  fail-closed tests.

### 2.2 Path-bearing reality of the code plane

- Exactly three tantivy fields carry a checkout absolute path on a
  project-file document: `file_path` (the joined display path),
  `path_tokens` (same string tokenized, so host-root components are BM25
  tokens), and `project` (`canonical_path`). All three are produced by one
  writer, `build_project_file_doc_for_source`, fed by
  `compatibility_display_path(display_root, relative_path)`; collected
  staging passes `display_root = Some(project.canonical_path)` even though no
  byte is read from the checkout.
- Commit documents carry `project = canonical_path` and
  `file_path = git:<project_id>`; both are excluded from the Phase 1
  namespace commitment (`hash_commit_rows` folds only namespace, entity ref,
  commit sha, content hash), so path-free re-emission preserves commitment
  equality by construction. That property is load-bearing and must not be
  "fixed".
- The freshness cache `FileMeta` map is keyed by the absolute path string
  (`HashMap<String, FileMeta>`, persisted at `config.meta_path`); the purge
  phase deletes stale non-project rows by an absolute-path term
  (`reindex.rs`), with a second identically shaped legacy purge loop in
  `bbox-corpus-index/src/index/search.rs` that must move in lockstep.
- The resolved-id filter lane (`push_project_filter_clause`) targets
  `base_project_id`, a field project-file documents never carry; project-file
  documents are reachable through a project filter only via the literal
  substring lane over their absolute `project` value. `project_id` is already
  stamped and indexed on project-file documents.
- Embedding text is already path-free (`chunk.content` only). Vector keys are
  already path-free (`entity_id` plus `content_hash`). Edge endpoints
  (`ProjectFile`/`ProjectFileV2`) are already path-free.
  `EntityRef::File { path }` remains a caller-supplied free-form variant with
  no normalization; it is a read-side resolver input, not indexed identity.
- `WorkspaceManifest` in the edge sidecar persists `canonical_path`,
  `git_common_dir`, and `git_worktree_dir` per project.
- `ref_snapshot_id` under `BBOX_PROJECT_REFS_V2=1` (default off) hashes the
  absolute root into `nongit-` snapshot ids: a latent leak if enabled.

### 2.3 History reality

Nothing of governing section 11's model exists in code yet:

- `RepoHistoryRecord` and `AmbiguousNamespaceRecord` shipped in Phase 1
  without the `materialization` field the governing section 5.1 describes;
  `RepoHistoryGenerationId`, `RepoHistoryQuarantineGenerationId`, the
  generation types, `RepoHistoryRebuildManifestV1`, and `GitOverlaySelector`
  do not exist. `CatalogSnapshotV2` and every nested record deserialize with
  `deny_unknown_fields`, so the field addition is a catalog format change.
- `V1ProjectCatalogInventory` is never persisted; only its aggregate
  `inventory_hash` survives in the marker and receipts. The materializer has
  nothing durable to prove namespace equality against today.
- Commit ingestion walks per project (`index_git_history_for_project`), keyed
  `commit:<namespace>:<sha>` with delete-then-add semantics: monorepo
  siblings duplicate the walk, overwrite each other's document fields, and
  advance divergent per-project `last_ingested_sha` cursor files under
  `git_meta/`. Nothing backs those cursor files up.
- `COMMIT_TOUCHED_FILE` edges embed `snapshot_id` and `chunk_hash` in their
  target refs, so they dangle on every generation swap; they are materialized
  per project per snapshot in `git-current.jsonl` and loaded only for the
  active snapshot. They are derived and disposable; only repo-level commit
  documents and their vectors need immutable generations.
- Commit vectors are enqueued per walk (`emit_git_message`) with
  `project_id: None` (already repo-scoped); there is no commit tombstone path
  and no generation-driven vector lifecycle.
- The destructive replacement trigger is live:
  `reset_index_on_schema_mismatch` performs `fs::remove_dir_all(index_path)`
  guarded only by `verify_collected_schema_migration_sources` (collected code
  blobs). Commit documents are dropped and re-derived from checkouts; for an
  attachment-less project that is permanent loss. The crate convention "the
  reindex IS the backfill; never dual-read old docs" is the rule this phase
  carves one reviewed exception into.
- `validate_catalog` runs on every strict read and every transact post-image
  and enforces: global namespace uniqueness across primary and compatibility
  namespaces, owned-xor-quarantined exclusivity, and at least two existing
  candidates per ambiguous record. Consequence: an `Unclaimed` namespace
  cannot be represented in the catalog at all; the rebuild manifest is the
  sole durable owner of unclaimed-generation identity.
- Retire (`project_catalog_admin.rs`) deletes a `LocalProject`-authority
  history record when its last referent retires, with no materialization
  guard: a GC-root deletion once `Ready` exists.

### 2.4 Live defects Phase 3 must close

Enumerated so the milestones can claim them explicitly:

- F1: remote-only catalog projects have no `ProjectRecord` row
  (`CatalogProjectRecordsProvider` emits attached-only rows), so
  `activate_desired_loop` and `cutback_to_local` fail with "registered
  project disappeared", and reindex planning (`acquire_project_leases`) never
  visits them.
- F2 (purge hazards): H1, a detached project's local documents are deleted by
  full rebuild with no preservation arm; H2, the same deletion fires on any
  incremental tick via the stale-path purge; H3, an attached-but-empty
  readable root is indistinguishable from a deleted project and purges
  itself.
- F3: the runtime edge rebuild (`src/server/routes.rs`) seeds
  `registered_project_ids` from attached records while startup
  (`src/server/open.rs`) uses `corpus_project_ids`: a remote-only project's
  edges are silently dropped on the first runtime rebuild, including the
  rebuild triggered by its own activation.
- F4: the background storage GC pass (`src/server/storage_gc.rs`) seeds
  liveness from attached records while the MCP tool uses
  `corpus_project_ids`, and runs destructively (`prune_orphans: true`), so a
  remote-only project's sidecars are deleted after the 30-day orphan fuse.
- F5: a Git error mid-walk during collected staging fails the whole
  activation and loops on backoff, while a Git lease denial degrades
  gracefully: same dependency, two policies, and the error path rolls back a
  valid collected generation.
- F6: `stage_collected_project_generation` embeds the checkout absolute path
  into every collected document via `display_root`.
- F7: filter-lane gap: resolved project selectors cannot reach project-file
  documents through an id lane (`base_project_id` absent on them).
- F8: the hourly `gc_blobs()` call passes empty catalog scopes, bypassing the
  catalog-scope root parameter that already exists.

### 2.5 Fixed baseline

Unchanged: `monolith-decomposition-pre-attempt-2` = 254cabf0. Phase 2 closed
at ee84986b with an exact Kimi PASS; Phase 3 work starts from f6c4747a.

## 3. Non-goals and phase boundary

Phase 3 does not:

- apply v2 bytes to configured operator state or weaken the
  isolated-rehearsal-only apply guard (D-002);
- convert collector grants to catalog scope, separate auth swap from source
  changes, implement persisted no-attachment cutback pending with
  event-driven resume, or make live activation writers emit the strict
  scope-bearing v2 store records (Phase 4). Section 6 item 6 defines the one
  bounded interaction Phase 3 has with the grant table;
- wire accepted-publication generations or catalog-keyed knowledge/gap views
  into live views, or move blame/render/provenance/file providers to their
  final adapter shapes (Phase 5). Blame's indexed-path handling changes only
  as far as section 9 item 4 requires for the new document fields;
- delete v1 compatibility lanes, the literal filter lane, eight-hex bridge
  compat, or `load_project_records` consumers (Phase 6);
- implement `bbox_repo_history_namespace_resolve`, ambiguous-namespace
  attribution, or the destructive-retire discharge workflow;
- change vector storage layout or routes (`SlabEntry` stays; commit vector
  keys are stable across this phase);
- touch the `ref_snapshot_id` path embedding under `BBOX_PROJECT_REFS_V2=1`:
  the flag is default-off and those snapshot ids are sidecar metadata, not
  tantivy document fields, so the section 1 no-host-path assertion is
  unaffected; flagged in section 2.2 and left for the flag's own retirement
  decision;
- touch dispatch/orchestration execution targets.

Session workspace mapping (display tier 1) is defined as a rendering seam
that returns `None` in this phase; only tiers 2 and 3 are implemented.

## 4. Runtime model decisions fixed by this plan

These are plan-level decisions the implementer does not relitigate. Where a
decision reconciles the governing document with shipped reality, the
governing document receives the surgical amendment noted here in the same
commit as the implementing milestone, and the decision is recorded in
`DECISION_LEDGER.md` (D-034 onward) if any further material choice arises
during implementation.

### 4.1 Catalog history-materialization fields ship now, not retroactively

`RepoHistoryRecord.materialization` and
`AmbiguousNamespaceRecord.materialization`
(`NotBuilt | Ready { generation_id }`) are added in Phase 3 with
`#[serde(default)] NotBuilt` so existing v2 bytes decode unchanged;
`deny_unknown_fields` stays. The v1 importer post-image is amended to emit
the field explicitly. Consequences accepted: previously generated rehearsal
reports/resolutions/receipts predict pre-field catalog hashes and are
regenerated, not grandfathered; the facade refuses a resolution artifact
whose plan hash predates the field by the existing hash-binding, which is the
correct fail-closed behavior. The governing section 5.1 sentence claiming
Phase 1 writes `NotBuilt` is amended to name Phase 3 as the field's owner.
`validate_catalog` gains clauses: `Ready` generation ids must parse
(`rhg_`/`rhq_` plus 64 lowercase hex, section 5.2), and a `Ready` ambiguous
record must still satisfy the existing candidate rules. Validation stays
pure; it never reaches the generation store.

### 4.2 The persisted namespace-inventory asset closes the proof gap

Phase 1 computed but never persisted the per-namespace commitments. Phase 3
adds `LegacyCommitNamespaceInventoryAssetV1`: a versioned, canonical-JSON
immutable asset carrying exactly the `LegacyCommitNamespaceInventoryV1` rows
(namespace, commit document count and commitment, vector-key count and
commitment, attribution) plus the source index fingerprint and the parent
`inventory_hash`. The migration facade writes it through the existing
immutable-asset mechanism, so its hash is bound in
`predicted_immutable_asset_hashes` and verified by the receipt. The
materializer proves observed namespace sets against this asset. On a
`MigratedV1` store whose marker predates the asset, materialization of
legacy namespaces refuses (typed `history_inventory_missing`) and the
replacement stays refused; rehearsal roots are regenerated with current
code, and the Phase 6 operator cut will always run with the asset present.
Cursor files gain the backup the governing section 11 promises: the facade
copies `git_meta/` into the existing backup root and binds the copy in
`backup_hashes`.

### 4.3 Bridge-window behavior contract

Bridge-mode live behavior changes are limited to this list; everything else
observable stays at parity, enforced by the parity harness:

1. Collected activation no longer opens Git and no longer fails on Git
   errors. Current-file Git edges for a collected generation are staged by a
   post-activation best-effort step (section 6 item 3); a Git failure there
   records health and leaves the activation intact. This implements the
   governing section 11 sentence "Git failure cannot roll back a valid
   collected generation" and supersedes the F5 split-policy behavior.
2. Collected documents stop carrying the checkout display path, in two
   enumerated steps. At P3-B, new collected documents' `project` and
   `file_path` fields carry the intermediate path-free fallback the doc
   builder already implements for an absent display root (the project id
   and the relative path); existing documents are untouched until the bump.
   At the P3-E schema cut, `project` becomes the display name and
   `file_path` the normalized relative path for all newly built
   project-file documents, local staging included. Both transitions are
   tested by the parity harness at their owning milestones; neither is a
   silent drift. Two deliberate search-behavior consequences at the P3-E
   cut are part of this enumeration and get their own parity rows: the
   permanent literal substring lane stops matching project-file documents
   by unregistered absolute-path fragments (the lane itself is untouched,
   and transcript documents keep their literal cwd fields; governing
   decision 9's permanence concerns the transcript lane), and BM25 queries
   containing host-root components stop matching project-file
   `path_tokens`.
3. The `INDEX_SCHEMA_VERSION` bump triggers one full index rebuild at the
   first daemon open after deploy, as every prior schema bump has. New in
   this phase: commit documents are carried over across the reset by the
   bridge spill lane (section 9 item 2) instead of being dropped and re-walked,
   so bridge projects with unavailable checkouts no longer lose history at
   a schema reset. This is additionally the first INDEXER_VERSION bump
   since the collected lane shipped: active collected projects migrate
   their materialization selectors in place during that rebuild (section 9
   item 2, D-035) instead of wedging boot, which the original wording here
   silently assumed could not happen. The same deploy also performs a one-time full re-embed
   of project-file vectors through the embedding-envelope version bump
   (section 9 item 5): behavior-neutral for results, operationally heavy on
   large corpora, enumerated here for the same visibility as the rebuild.
4. The resolved-id filter lane additionally matches `project_id` on
   project-file documents (F7). For resolved selectors and for the
   transcript lanes, result sets can only grow; the project-file
   literal-lane narrowing at the P3-E cut is the separate enumerated change
   in item 2, not covered by this sentence.
5. The background storage GC pass and runtime edge rebuild seed from
   `corpus_project_ids` (F3/F4). In bridge mode the two sets are identical
   by construction (every registered project is attached), so this is a
   no-op there; the parity harness asserts exactly that.
6. Defect repair discovered by the P3-B bridge bootsmoke: the first
   collected activation of a previously locally indexed project was wedged
   at the baseline (`validate_retirement_record` applied the strict
   migration snapshot-id shape to the OUTGOING snapshot id, refusing the
   `head-`/`nongit-` local snapshot every local-to-collected transition
   retires; introduced by a Phase 1-era repair commit and never reachable
   in any prior test or smoke). The retirement validator now uses the
   general snapshot-id shape; the strict migration shape is unchanged for
   its migration and collision consumers. This converts a permanent
   activation backoff loop into the designed behavior; regression tests
   cover the accepted outgoing shapes and the unsafe-id rejections.

### 4.4 Unclaimed namespaces: importer refusal stays, materializer handles drift

The v1 importer's `unsupported_legacy_namespace` refusal is unchanged:
migration still refuses unclaimed or unclustered-ambiguous namespaces, so a
migrated store starts with full attribution. The materializer independently
classifies namespaces observed in the live index at replacement time. A
namespace with no owning catalog record and no ambiguous record (possible
through post-migration drift, retired projects, or a fresh v2 store's legacy
residue) is materialized as
`RepoHistoryQuarantineGeneration { disposition: Unclaimed { inventory_diagnostic } }`,
named only in the rebuild manifest per the validate-catalog exclusivity
rules, and never resolves through ordinary queries. Both statements are true
at once and the plan states them together so neither reads as contradicting
the other.

### 4.5 Read-view pin

`CodeReadView` gains `catalog_epoch: u64` (from
`ProjectRecordsSnapshot.authority_epoch`) and, in catalog mode,
`git_overlays: BTreeMap<String, GitOverlaySelector>` (section 10). The
pinned `active_selectors` map is the selector snapshot for BOTH the lexical
gate and vector-hit filtering: the per-hit vector probe
(`is_active_code_entity_for`) is re-pointed at the pinned view's searcher and
selector map instead of live state, which satisfies the governing
section 10.3 pin without introducing a parallel vector-store snapshot
structure. All construction sites are updated: the four runtime republish
writers, the startup initial construction in `src/server/open.rs` (which
must seed `catalog_epoch` and `git_overlays` from the boot snapshot), and
the three test constructors; the searcher-only writer
(`publish_code_read_searcher`) must clone the new fields through exactly as
it clones `edge_index` today, with a regression test for the drop-on-commit
bug class.

### 4.6 Identity and keying decisions

- `rel_path_hash` stays 8 hex characters. Entity-ref stability across the
  bump outweighs the collision margin, the new `relative_path` field removes
  pressure on the hash as identity, and the collision class is unchanged
  from today. Documented, not silently inherited.
- `FileMeta` rekeys to a typed composite key rendered as
  `pf\0<project_id>\0<source_kind>\0<relative_path>` for project files while
  transcript and adapter rows keep absolute-path keys; the persisted meta
  file gains a version marker and old-format rows are discarded at load
  after the schema bump (the bump drops the index they described anyway).
  The purge phase's absolute-path delete arm survives only for the
  non-project lanes; both purge loops (reindex and legacy `build_index`)
  change together.
- `legacy_local_snapshot_id(project_id, manifest_digest)` is implemented as
  the governing section 10.1 defines, emitting the shape
  `legacylocal-<hex16>`; `validate_migration_snapshot_id` and
  `validate_collected_materialization_selector`-adjacent validators widen to
  accept the new prefix explicitly. The selection rule is computed from
  catalog record shape alone, never from a creation-lane notion the catalog
  does not store: a LegacyLocal project whose history authority is
  `LocalProject` with a `local_`-prefixed primary namespace, or a non-Git
  LegacyLocal project, uses `legacy_local_snapshot_id`; a migrated Git
  LegacyLocal project whose history record carries an imported legacy
  namespace (authority `LegacyNamespace`, or a non-`local_` primary) keeps
  the existing head-bound clean-snapshot derivation under that namespace,
  preserving its established ref shape and commit joins. Bridge local
  staging keeps head-bound clean snapshots unchanged.
- `source_uri` is a stored field and response value, not an `EntityRef`
  variant; the codec lives in `bbox-code-source` beside
  `validate_relative_path`. `source_entry_key` remains the delete-term key.

### 4.7 Effective source is derived, single-authority

No new persisted effective-source store. The edge-sidecar `ManifestIndex`
workspace entry remains the single durable authority for the live selector;
`cutback_pending` remains on the activation record. Phase 3 introduces the
typed derivation `EffectiveSource { Collected { generation }, Local,
CutbackPending, Warming, Unavailable { reason } }` computed per project from
the pinned catalog snapshot plus manifest plus activation record plus the
assignment table at planning time, with `Unavailable` gaining a durable
health record instead of a per-pass warning. `Warming` classifies a project
with a configured producer assignment and no active collected generation yet
(first upload in flight). Warming preserves local freshness, matching
today's planner (`code_local_enabled` stays true until a collected source is
active) and the collector design's section 10 contract: when the project has
a valid local-source attachment or bridge record, a `Warming` project plans
exactly as `Local` (identity plus the normal lease bundle, local walking
active); only when no local source exists (the remote-only warming case)
does it degrade to a pass-level no-op with no lease and no durable health
record, surfaced only as informational doctor output. Neither arm can
trigger the F1 lease-attempt class or collide with Phase 4's warming state
machine. The governing section 10.3 planning outcomes are implemented over
this derivation.

## 5. Milestone P3-A: substrate types, codec, and catalog fields

Ownership: `bbox-corpus-core`, `bbox-code-source`, `bbox-indexing`
(importer/admin), no daemon behavior change.

1. `CodeProjectIdentity { project_id: ProjectId, scope: ProjectScope,
   display_name: String, repo_history: Option<RepoHistoryRecord> }` in
   `bbox-corpus-core` (new module `code_project_identity.rs`), with
   constructors from a catalog project plus its history record and from a
   bridge `ProjectRecord` (v1 arm derives `scope` from the record's published
   scope when present; otherwise the identity keeps a placeholder
   `LegacyLocal` scope and the typed marker is realized as an
   `IdentityOrigin { Catalog, Bridge }` provenance field on the identity
   itself, so the P3-B collected-staging refusal keys on origin `Catalog`
   plus scope `LegacyLocal` while bridge identities proceed on lease
   authority; it never fabricates a `PublishedScope` (D-034)). The v1 arm's
   `display_name` is the record's first alias when one exists, else the
   project id, and never a path component: `ProjectRecord` has no
   display-name field, and a basename fallback would reintroduce a
   host-path fragment into the field P3-E cleans.
2. `source_uri` codec in `bbox-code-source`: percent-encode each validated
   relative-path segment's UTF-8 bytes except ASCII alphanumerics and
   `- . _ ~`, uppercase hex, slashes unencoded, no Unicode normalization;
   decode exactly once and require canonical re-encoding equality; reject
   empty, `.`, `..`, encoded slash or backslash, NUL, control, and platform
   prefixes. Rendering:
   `bbox://project/<project_id>/<encoded-relative-path>`. Round-trip tests
   include spaces, `%`, `#`, `?`, and non-ASCII names, plus rejection tests
   for non-canonical and traversal encodings.
3. Catalog fields per section 4.1: `materialization` on both record types,
   `RepoHistoryGenerationId` (`rhg_` + 64 lowercase hex) and
   `RepoHistoryQuarantineGenerationId` (`rhq_` + 64 lowercase hex) parsed
   types, `validate_catalog` clauses, importer post-image emission, admin
   guards: retire refuses to delete a history record whose materialization is
   `Ready` (typed `history_generation_referenced`), and the retire blocking
   inventory names it.
4. `legacy_local_snapshot_id` and validator widening per section 4.6.
5. `LegacyCommitNamespaceInventoryAssetV1` type plus facade emission and
   receipt binding per section 4.2, and the `git_meta/` backup copy.

Tests and gate: codec round-trip and fail-closed suites; catalog decode
compatibility (old bytes without the field), validation clause matrix,
importer round trip on the facade fixture (asset present, hash-bound, backup
recorded); admin retire refusal. Workspace nextest, clippy, concurrency lint
lane-side; commit, push, cluster verify.

## 6. Milestone P3-B: source-neutral staging, Git out of the collected transaction

Ownership: `bbox-corpus-index/project_files.rs`, `bbox-indexing/writer_actor.rs`,
`src/server/code_source.rs` call sites.

1. `IndexWriteOp::StageCollectedGeneration` and `StageLocalGeneration` take
   `Box<CodeProjectIdentity>`; actor handles and both pure staging functions
   convert. Collected staging rejects a `LegacyLocal` identity with a typed
   refusal before any writer work. Local staging takes identity plus the
   validated local-source lease it already holds; scope comes from the
   identity.
2. Collected staging drops the `GitHistory` lease acquisition,
   `stage_git_current_edges`, and the post-stage `revalidate` restage cycle
   entirely. The activation transaction commits code documents, code edges,
   vectors enqueue, and the selector without opening Git (governing
   section 11). `activate_collected_snapshot_with` keeps its signature this
   milestone; the `head_commit`/`repo_id` arguments become advisory metadata.
3. Git current-file edges for collected activations move to a
   post-activation, best-effort step owned by the daemon side
   (`code_source.rs`): acquire the Git lease, walk, stage the `git-current`
   snapshot member, republish the read view; on denial or error record the
   existing `git_history_unavailable` health and leave the activation
   intact. This is the minimal overlay semantics; the typed selector arrives
   in P3-F. The local-cutback lane keeps its current in-transaction Git
   behavior this milestone (`run_local_stage`, whose only production caller
   is `cutback_to_local`, IS the cutback path; the original wording here
   mislabeled it as a non-cutback lane and contradicted the first sentence,
   amended per the P3-B cell review) to bound the diff and preserve the
   local-staging parity gate; it converts to the overlay step in P3-F with
   the overlay machinery.
4. `activate_desired_loop` and `cutback_to_local` resolve the identity from
   the catalog snapshot in catalog mode (fixing F1 for activation) and from
   the records provider in bridge mode; the "registered project disappeared"
   failure survives only for bridge local cutback, which genuinely needs an
   attachment.
5. Collected staging passes `display_root: None`; until the P3-E schema cut
   the compatibility fields fall back to the existing
   `project_id`-as-display behavior that `build_project_file_doc_for_source`
   already implements for a `None` root. Bridge parity harness pins the
   enumerated field change (section 4.3 item 2 starts here for collected).
6. Bounded grant-table catalog arm, the single Phase 4 pull-forward,
   mirroring the P2-B seeding precedent: in catalog mode, `build_snapshot`
   resolves each configured producer scope to its catalog project by exact
   scope equality against the pinned catalog snapshot, acquiring no leases;
   unknown scopes and multi-project scope collisions fail closed with
   today's error shapes. Bridge mode keeps the lease-derived resolution
   unchanged, including its hard failure on any lease error. Auth-swap
   separation, cutback transitions, persisted no-attachment cutback, and v2
   activation-record emission remain Phase 4. Without this arm the
   governing Phase 3 exit gate ("a remote-only fixture activates") is
   unsatisfiable, because the v1 grant table requires a publisher-config
   lease on every registered project.

Tests and gate: staging unit tests for identity threading and LegacyLocal
refusal; an activation test where the Git walk errors and the generation
still activates with health recorded (F5 closed); collected staging under a
deny-all broker acquires zero leases; bridge parity fixtures for local
staging unchanged. Bootsmoke per section 13: bridge parity plus catalog-mode
activation of the facade fixture's remote-only project, which after this
milestone must succeed end to end for activation and lexical/hybrid search;
graph and edge completeness for the remote-only project arrives with the
P3-C registered-set fix (F3) and is asserted there. Commit, push, cluster
verify.

## 7. Milestone P3-C: catalog-driven planning, purge exemptions, read-view pin

Ownership: `bbox-indexing` (reindex, writer_actor), `src/server/*`,
`bbox-corpus-index` (filter clause, selector gating).

1. Planning iterates the pinned catalog snapshot: `acquire_project_leases`
   becomes `plan_project_sources`, which walks `corpus_project_ids`, computes
   `EffectiveSource` per section 4.7, and yields typed per-project plans:
   `Collected` plans carry the identity and acquire no code-walk lease for
   the collected indexing lane (the active immutable generation is
   rematerialized without walking a checkout); the non-code lease bundle
   (`PublisherConfigTreeRead`, `GitHistory`, `KnowledgeGapOverlayRead`) is
   keyed on ATTACHMENT exactly as today, so an attached collected project
   keeps the lease set its git-history and repo-owned-knowledge lanes depend
   on, while a project with no compatibility record acquires nothing at all.
   Governing section 10.3's "without a lease" clause is scoped to the
   code-source staging lane; section 4.3 enumerates no change to the
   git-history or repo-owned-knowledge lanes for an attached collected
   project, which is what makes attachment-keying the parity-correct and
   phase-correct reading (the overlay that would replace checkout-based git
   history does not exist until P3-F). Amended per the P3-C cell review.
   `Local` plans carry the identity plus the full lease bundle acquired
   exactly as today for attached projects; `Unavailable` plans carry the
   reason and a durable health write;
   `Warming` plans as `Local` whenever a local source exists (section 4.7)
   and otherwise as a pass-level no-op; `CutbackPending` plans are
   pass-level no-ops; the no-op arms acquire nothing and participate in
   purge only through their exemptions. Bridge mode reproduces today's
   lease set per attached record through the same shape, warming included.
2. Purge and preservation close F2: the stale-path purge exempts every
   project whose plan is not `Local`-scanned this pass (collected,
   unavailable, cutback-pending, detached); full rebuild preserves documents
   for every non-`Local` plan through the existing preservation collectors
   extended with a detached/no-attachment arm. The verification authority
   for that arm is the project's own freshness rows: the per-project
   `FileMeta` set (rekeyed in P3-E; until then the absolute-key rows
   filtered by project) enumerates the expected documents, and preservation
   verifies the live per-project document count against that inventory,
   recording `preservation_failed` and aborting the rebuild before
   `delete_all_documents()` on mismatch, exactly like the collected arm.
   The state converges through the same operator surfaces as the empty-root
   case below (acknowledged purge, detach, or retire); it cannot silently
   downgrade to unverified preservation. H3 gains the empty-scan refusal: a
   `Local` scan yielding zero entries while the prior pass's meta for that
   project was non-empty refuses the project's purge with
   `empty_root_refused` health instead of deleting. The operator escape is
   explicit and acknowledgement-shaped: `bbox_reindex` gains an
   `accept_empty_projects` list parameter (operator authority, passed
   through never defaulted, RX-V1 discipline) whose named projects purge
   normally on that pass and clear the health record; detach/unregister and
   retire also clear the state by removing the project from `Local`
   planning. Both escapes carry test rows.
3. F3 and F4: the runtime edge rebuild and the background storage GC pass
   seed from `corpus_project_ids`. F8 is a catalog-mode-only change: in
   catalog mode the hourly GC calls `gc_blobs_for_scopes` with the catalog
   scope set (LegacyLocal projects contribute their activation/anchor roots
   as today; they have no `PublishedScope` and add no scope entry). In
   bridge mode the hourly call keeps the empty-scope `gc_blobs()` exactly as
   today: every bridge activation and generation is a v1 record, a
   non-empty scope set flips `protected_generation_ids` off the legacy
   classifier arm (`catalog_scopes.is_empty()` is part of its guard), and
   the mixed classifier hard-fails on v1 rows ("protected legacy generation
   lacks strict v2 ownership"), which would permanently wedge bridge blob
   GC. A bridge parity test proves the protected set and reclaim behavior
   are byte-unchanged; a catalog-mode test proves retained-only generations
   gain protection through the scope roots. The `file:` provider's
   empty-projects error distinguishes "no attached projects" from "no
   projects".
4. F7: `push_project_filter_clause` gains the `project_id` term lane.
5. Read-view pin per section 4.5: `catalog_epoch` on `CodeReadView`, per-hit
   vector filtering re-pointed at the pinned view.

Tests and gate: planning matrix (remote-only, detached, attached, empty-root,
warming in both arms: attached-warming keeps local walking and lease
acquisition, remote-only warming is the no-op, and cutback) against a
fixture catalog, with an attached-warming row in the bridge parity harness
proving freshness during the warming window is unchanged; purge exemption
and empty-root
refusal tests on both purge loops, plus the acknowledgement round trip
(`accept_empty_projects` purges and clears the health record; detach clears
it too); detached-local preservation verification against the per-project
freshness inventory, including the mismatch-aborts-before-delete arm;
edge-set and GC-set equality tests between startup, runtime rebuild, tool,
and background pass; bridge blob-GC parity (empty-scope call preserved,
protected set and reclaim behavior byte-unchanged) and catalog-mode
retained-only scope protection; filter lane test proving a resolved id
reaches project-file docs with the literal lane removed from the fixture;
read-view pin regression test (searcher-only republish preserves epoch and
overlays). Bootsmoke: catalog-mode remote-only project survives an
incremental tick and a forced full rebuild with zero leases and identical
result counts. Commit, push, cluster verify.

## 8. Milestone P3-D: history generations, materializer, rebuild manifest

Ownership: new `bbox-corpus-index/src/index/history_generations.rs` (store
format and verification), `bbox-indexing/src/index/history_materializer.rs`
(orchestration), catalog transact integration. No index behavior change yet:
this milestone builds and proves the machinery against fixtures; the wiring
into the replacement boundary is P3-E.

1. Generation store: immutable, self-contained snapshots under the index
   family root (`<state>/history-generations/<generation_id>/`), a SIBLING
   of `index_path` and never inside it, because the destructive reset
   removes `index_path` recursively and must never be able to touch
   generations; each holding
   canonical JSONL commit documents (full stored-field set minus the two
   path-bearing fields, which are re-derived at re-emission), the vector
   input rows (entity id, content hash, message text), a manifest with
   ordered document-set commitment, counts, content hashes, source
   schema/fingerprint evidence, and the typed owner/disposition. Generation
   ids are content-addressed SHA-256 over a versioned domain separator,
   namespace, typed owner/disposition, and the canonical bytes of the
   body's CONTENT-BEARING preimage view, using the `put_field`
   length-prefix convention from `bbox_code_source::generation_id`.
   Amended per closing-review round 2 (D-039): the three source evidence
   fields are provenance, recorded in the body but excluded from the id
   preimage, so identical carried content re-derives the same id across
   schema bumps and across the scan and live-refresh construction sites;
   when identical content is re-created under different evidence the first
   writer's evidence is retained. This is what makes re-materialization
   idempotent and unable to remint identity ACROSS schema generations, not
   only within one; with the whole body in the preimage, the second schema
   replacement re-derived a new id for identical content and the strict
   no-remint advance wedged the open path permanently. Transition posture:
   generations minted under the pre-D-039 whole-body preimage fail
   validation loudly (no silent acceptance, no compatibility shim), which
   is acceptable because the format has never shipped as live authority;
   only disposable catalog-mode test and bootsmoke roots can hold such
   state, and they rebuild from scratch.
2. Materializer: streams the legacy index's commit documents by exact
   namespace (reusing the Phase 1 capture's row shape and commitment
   function), classifies each namespace against the pinned catalog snapshot
   (owned via record primary namespaces, owned-compatibility via record
   compatibility namespaces, ambiguous via ambiguous records, else
   unclaimed per section 4.4), proves against the persisted
   namespace-inventory asset for `MigratedV1` stores in TWO MODES gated by
   a recomputed source fingerprint (amended per the live P3-E smoke,
   D-036): Equality mode, when the index is unchanged since migration,
   keeps exact per-namespace count and commitment equality; Drift mode, in
   every other case, constrains only that recorded namespaces survive with
   at least their recorded counts (an ordered fold cannot prove subset
   containment, so commitments are not compared under drift), while
   namespaces absent from the asset classify normally; a cross-namespace
   survival check runs in both modes, the proof mode plus both
   fingerprints are recorded in the outcome and the rebuild manifest, and
   the Phase 6 offline rebuild must require Equality mode (typed refusals
   `history_inventory_missing` / `history_commitment_mismatch` in both
   modes). The materializer creates and verifies the generation, then
   advances `materialization: NotBuilt -> Ready` through one regular
   catalog transact per proved namespace set, where Ready names the
   PRIMARY namespace's generation only: compatibility-namespace
   generations mint owned ids but are durably owned by the rebuild
   manifest's dedicated compatibility bucket, like unclaimed ones (D-037),
   and the double-advancement refusal is keyed to the primary namespace.
   Fresh v2 stores with no legacy residue produce no generations and stay
   `NotBuilt` legally.
3. `RepoHistoryRebuildManifestV1`: durable prepared/committed manifest under
   the same family root; prepared binds source index fingerprint, complete
   namespace inventory, catalog epoch, every owned/ambiguous/unclaimed
   generation id, and planned target lexical/vector generation labels;
   committed additionally binds the verified replacement views and resulting
   catalog epoch. Recovery on open runs before any read view binds and is
   classified by position relative to the destructive drop: a prepared
   manifest observed with the old index still intact rolls back (delete the
   manifest, last-good views stay selected); a prepared manifest observed
   after the drop can only resume, re-executing the replacement from its
   pinned generations, because no last-good index exists to roll back to.
   Both states pin their named generations for GC. Crash-recovery tests use
   the fault-point seam pattern from Phase 1, and each matrix row names
   which arm (rollback or resume) its crash point takes.
4. GC surface: history generations become GC roots exactly as governing
   section 16 lists; the derived reference-manifest acceleration index is
   deferred to P3-F where overlay references exist to accelerate.

Tests and gate: materializer fixture matrix (proved, ambiguous, unclaimed,
commitment mismatch, missing asset, empty index, idempotent re-run yielding
byte-identical generation ids); transact advancement and validation-clause
integration; manifest recovery matrix (crash before prepared, between
prepared and committed, after committed); vector-input completeness against
the vector-side commitment. Commit, push, cluster verify. No bootsmoke
change (machinery is not yet wired).

## 9. Milestone P3-E: the path-free schema cut

Ownership: `bbox-corpus-index` (schema, docs, purge, responses),
`bbox-indexing` (reindex, meta), `bbox-mcp-tools` and `bbox-providers`
(response shaping), `src/embed_runtime.rs`, `src/tools/graph.rs`.

This is the single milestone that bumps `INDEX_SCHEMA_VERSION` and
`INDEXER_VERSION` (paired, one commit) and therefore the only milestone that
can change stored document identity.

1. Schema: add `relative_path`, `source_uri`, `source_kind` stored fields;
   project-file docs stop storing absolute values: `file_path` carries the
   normalized relative path, `path_tokens` tokenizes the relative path plus
   symbol, `project` carries the catalog display name (catalog mode) or the
   bridge fallback fixed in P3-A item 1 (first alias, else project id).
   Commit documents drop `project = canonical_path` in favor of the same
   display value and keep their namespace/sha identity untouched.
2. Replacement boundary, restructured to inventory-materialize-verify-replace
   with the crate direction respected: `bbox-corpus-index` cannot depend on
   `bbox-indexing`, so `reset_index_on_schema_mismatch` never calls the
   materializer directly. Instead, `open_or_create` /
   `open_or_create_with_code_source_store_path` gain an optional
   pre-replacement guard callback, injected by both production callers
   (`src/server/open.rs` and the writer-actor open in
   `crates/bbox-indexing/src/index/writer_actor.rs`). On a detected schema
   mismatch the open path invokes the guard BEFORE any destructive step. In
   catalog mode the injected guard is the P3-D materializer orchestration
   (living in `bbox-indexing`, which owns the catalog transact): it drives
   every namespace to `Ready`, writes the prepared rebuild manifest, and
   only its success authorizes the reset; any refusal aborts the reset and
   keeps the last-good index and views readable. The generation store
   scanning/creation primitives stay in `bbox-corpus-index`
   (`history_generations.rs`) so the guard closure composes them with the
   catalog transact across the boundary. Re-emission of commit documents
   from generations happens inside the schema rebuild pass before checkout
   walks, and the committed manifest must name the resulting views.
   Re-emission also verifies vector coverage from each generation's
   vector-input rows: any (entity id, content hash) pair without an active
   vector is re-enqueued from the generation's stored message text, and the
   committed manifest records that verified-or-enqueued inventory, so a
   replacement never commits a lexical-only history view whose vector view
   was promised (governing section 10.3). In bridge mode the injected guard
   is the spill lane, with an explicit crash lifecycle because the
   schema-mismatch trigger fires only once: the spill file lives durably
   beside the index family root (`<state>/commit-spill/`, outside
   `index_path`), is written and fsynced before the drop, is consumed at
   EVERY daemon open whenever present, independent of any schema-mismatch
   trigger (re-add is delete-term-then-add idempotent), and is deleted only
   after the re-add commits. Leftover-spill consumption completes BEFORE
   read views bind and readiness is reported, mirroring the ordering the
   P3-D manifest recovery pins for the catalog lane, so the carried-over
   population is never served incomplete. A crash after the drop, during
   the rebuild, or
   after the rebuild but before the re-add commits therefore recovers on
   the next open instead of reopening the history-loss window; vectors are
   untouched throughout because commit entity ids and content hashes are
   stable. An absent guard refuses the reset outright: after this milestone
   no open path can reach the destructive drop without an injected guard,
   which converts today's unconditional drop into fail-closed behavior for
   every caller. The bump also owns its own survivability, added per the
   P3-E cell's stop-and-report (the plan originally missed both defects
   below): a version bump changes every collected materialization selector
   and snapshot id by construction, so the full-rebuild collected path
   gains a materialization-migration arm. A persisted selector that is
   shape-valid for the same project and generation with a different
   materialization suffix, whose activation record agrees, re-stages from
   store blobs under the current version with zero leases, saves the new
   activation record preserving cutback state verbatim, flips the manifest
   entry under the coordinator, and enqueues the outgoing selector's
   retirement; every other mismatch shape keeps its fail-closed bail, and
   incremental passes preserve rather than migrate (D-035). Separately,
   the first post-reset rebuild runs against an empty index, which
   surfaced a missing zero guard on the unavailable-git document collector
   (a zero-limit top-docs query panics); the guard now matches its
   siblings. A pre-marker index (no schema marker file, directory
   non-empty) is authorized WITH CARRY, not authorized-empty, per the
   closing review: the scan proceeds without a marker recording a reserved
   sentinel in the generation body's source evidence (provenance only,
   outside the id preimage per D-039), a structurally-non-index
   directory (no tantivy meta file) is nothing-to-carry while a corrupt
   index stays fail-closed, recovery classification resumes
   unconditionally on sentinel manifests, and both guards carry any
   documents found (catalog as drift-mode generations, bridge through the
   spill). The corrupt arm is deliberately fail-closed even for a
   marker-less directory (confirmed at closing-review round 2): a tantivy
   meta file that is present but unreadable means an index is there whose
   history cannot be carried, and refusing to open beats the old
   self-heal-by-rebuild, which silently destroyed whatever that index
   held; the posture matches the marked-index corrupt arm. The rebuild pass itself carries a typed cause: the strict
   collected-preservation collectors verify live counts and are
   load-bearing on an ordinary full pass, but meaningless on a freshly
   reset index, so the schema-migration rebuild (and only it, threaded
   explicitly from the open path, never inferred from observed emptiness)
   skips exactly those two collectors and relies on re-staging from
   verified store blobs; every document class the ordinary collectors
   protect is either re-materialized from durable sources post-reset or
   was already lost at every prior schema bump by the pre-existing
   contract.
3. Keying: `FileMeta` composite rekey and meta version marker per
   section 4.6; both purge loops split their delete arms (project rows by
   entry key, non-project rows by absolute path); edge purge reads the
   relative key directly.
4. Response boundary: `properties_from_doc` emits `relative_path`,
   `source_uri`, and `display_path` (tier 2: selected attachment root join,
   rendered but never opened; tier 3: display name plus relative path);
   `hybrid_title`, `project_file_label`, and the reranker candidate document
   consume the relative path; `HybridResult` gains structured `project_id`,
   `relative_path`, `source_uri`; `acquire_project_file` consumes the stored
   relative path and retires the absolute-strip fallback to a tagged compat
   arm for pre-bump refs; `chunk_from_embedding_doc` rehydrates the relative
   path.
5. Embedding text prepends the display name and relative path per governing
   section 10.2, with a named re-embed mechanism, because the enqueue dedup
   would otherwise silently skip it: `should_embed` keys on
   `(entity_id, content_hash)` and project-file enqueues pass the raw
   `chunk_hash` today, so an unchanged chunk never re-embeds (and folding
   the prepend into `chunk.content` would change `chunk_hash` and therefore
   `ProjectFileV2` ref identity, which section 4.6 forbids). The mechanism:
   the value passed as the embed queue's `content_hash` for project-file
   rows becomes a versioned envelope hash,
   `sha256(EMBED_TEXT_VERSION || chunk_hash)`, while document `chunk_hash`
   and every entity-ref component stay untouched. Bumping
   `EMBED_TEXT_VERSION` at this milestone misses the dedup for every
   project-file row exactly once; the vector store's single-active-entry
   upsert per entity id replaces the old vector, so no duplicate hits can
   surface. The envelope crosses every boundary that compares project-file
   vector hashes, or coverage accounting breaks: the Code/Docs arm of
   `record_index_doc_coverage` applies the same envelope to the document's
   `chunk_hash` before comparing against `active_entity_hashes`, so
   coverage converges to full after the bump instead of reading a permanent
   phantom zero that would mask real embedding outages and turn every
   residue sweep into full-corpus churn. The visual lane is OUTSIDE the
   envelope: its embedding input carries no text prepend and is unchanged
   by this milestone, so the enqueue helper splits lanes explicitly (text
   lanes enveloped, visual lane keeps the raw `chunk_hash`) and the
   coverage visual arm keeps raw comparison. Test rows: dedup miss on
   version bump, dedup hit within one version, replaced-not-duplicated
   active vector, ref bytes unchanged across the bump, and Code/Docs
   coverage converging to full after the bump with zero phantom residue
   while the visual lane neither re-embeds nor loses coverage. The one-time
   full re-embed is an operational event on large corpora; the milestone
   notes it in the deploy story beside the one-time index rebuild, and
   section 4.3 enumerates it as a bridge-window event.

Tests and gate: no-host-path assertion sweep over every emitted document
kind (grep-shaped test over stored fields for fixture roots and the
governing section 17 rows); source-uri response round trips; preserved
count/content equality across the guarded replacement on a migrated fixture
(commit docs, collected docs, LegacyLocal docs); refusal matrix (missing
generation, corrupt manifest, commitment mismatch all keep the old index
readable); bridge spill-lane equality test plus its fault-injection rows
(crash after the drop before the rebuild, during the rebuild, and after the
rebuild before the re-add commits, each recovering the complete commit set
at next open; leftover spill consumed on an open with no schema mismatch);
filter reachability with the id lane; incremental-equals-full convergence
for a LegacyLocal fixture per governing section 17. Bootsmoke: catalog-mode remote-only fixture searched
before and after a forced schema replacement with identical commit-document
counts and zero leases; bridge smoke proves the one-time rebuild and
carryover. Commit, push, cluster verify.

## 10. Milestone P3-F: Git overlay, consolidated ingestion, history GC

Ownership: `bbox-corpus-index/git_history.rs`, `bbox-edge-sidecar`,
`bbox-edge-index`, `src/server/code_source.rs`, catalog admin guards, doctor.

1. `GitOverlaySelector { project_id, code_generation,
   repo_history_generation, attachment_id, repo_head, commit_namespace,
   overlay_generation }` in `bbox-corpus-core`; `CodeReadView.git_overlays`
   per section 4.5; the workspace manifest entry gains additive overlay
   fields (non-strict serde, cheap); `active_paths_for_loader` gates the
   `git-current.jsonl` member on the overlay selection; activating a new
   code generation without a usable attachment atomically clears the
   project's overlay inside the existing manifest-coordinator after-hook.
2. Consolidated ingestion in catalog mode: one walk per repo-history record
   per refresh, keyed by primary namespace, executed through a validated
   attachment selected deterministically across member projects (operator
   default attachment first, then a `Base` attachment, then the lowest
   attachment id, the D-033.3 ladder shape), so the same catalog state
   always picks the same walk source; changed repo-relative paths map into
   each member project's `bbox_root_relpath` (generalizing
   `git_targets_for_scope`) and emit per-project file edges only inside that
   project. The first consolidated generation ignores every legacy
   per-project cursor (they are inventoried and backed up, never seeded from,
   per governing section 11), performs one complete reachable-history walk,
   publishes the generation, and only then records the new repo-history
   cursor on the history record's runtime state. Bridge mode keeps
   per-project walks unchanged. `run_local_stage` converts to the overlay
   step here, completing the P3-B deferral.
3. Live history refresh creates a superseding immutable generation
   (generations are immutable; nothing appends in place) through the same
   content-addressed creation path as the materializer:
   one shared creation path whose only callers are the materializer and the
   live refresh, with no other code constructing generations (the governing
   section 11 wording is amended to this formulation in the same commit as
   this plan). `Ready` generation ids advance through transact; commit
   vectors enqueue once per repo, not per project. Amended per
   closing-review round 2: the refresh's constant source marker is
   provenance only (D-039 keeps evidence out of the id preimage, so the
   scan and refresh sites converge on one id for one content), and the
   refresh path carries its own test coverage (supersede-and-retain,
   no-change no-op, cursor-after-publication, foreign-namespace refusal,
   and the refresh-then-replacement identity composition), closing the
   round-2 N2 gap where the sole production `Ready` advancement path was
   test-free.
4. History GC: commit documents and vectors become eligible for tombstone
   only when no catalog record, active or retained overlay, pinned read
   view, in-flight build, or prepared/committed rebuild manifest references
   the generation; the derived reference manifest is rebuilt and checksummed
   at startup and before every GC pass. Divergence semantics, amended per
   the closing review (D-038): the manifest is derived, not authority, so a
   persisted copy that decodes but disagrees with the rebuild is replaced
   by it, the divergence is logged with both checksums and surfaces as an
   informational doctor finding, and GC stays enabled; Disabled survives
   only for an unreachable or undecodable persisted file. The destructive
   sweep and vector tombstoning deliberately gain no production caller
   this phase (enablement evaluation and machinery only; a later,
   separately authorized wiring owns the pass). Vector tombstoning for
   retired history generations iterates the generation's own vector-input
   inventory (`delete_entity_all_routes` per entity), never a project code
   selector.
5. Health: the five-state history health model (current, lagging,
   unavailable-no-attachment, invalid-scope, failed-last-refresh) lands as
   typed health records surfaced in doctor beside the existing code-source
   section; the P3-C `unavailable` and `empty_root_refused` records join the
   same section.

Tests and gate: overlay swap/clear matrix under the manifest coordinator
(including the searcher-republish preservation regression); two-project
monorepo fixture proving single ingestion, per-project edge fan-out, and
divergent-cursor no-seed behavior; retire/detach reference-counting matrix
(retiring one sibling never tombstones shared history; detach releases no
history reference); crash between overlay swap and manifest refresh cannot
free a live generation; health-state matrix. Bootsmoke per section 13, full
catalog-mode assertion set. Commit, push, cluster verify.

## 11. Exit-gate proof

Extend the facade external-consumer acceptance test and the ignored producer
test (`crates/bbox-indexing/tests/project_catalog_migration_facade.rs`) into
the Phase 3 acceptance block, executed in CI and live:

1. The migrated fixture gains materialized legacy commit namespaces (proved,
   ambiguous via a two-candidate cluster, and one drift-unclaimed namespace
   injected post-migration), an attachment-less published project with an
   active collected generation and stale history, and a non-Git LegacyLocal
   project.
2. Remote-only assertions: activation, incremental, full rebuild, lexical
   and hybrid search, inspect expansion, graph discovery, and GC complete
   under `DenyCheckoutAccess` with zero leases (observation counters
   asserted); active selectors and edge sets include the catalog-only id.
3. Replacement assertions: forced schema replacement rematerializes the
   complete stale commit set from generations; the committed
   `RepoHistoryRebuildManifestV1` reproduces the exact catalog and
   quarantine links; each refusal arm (missing asset, corrupt generation,
   count mismatch) preserves the last-good views.
4. Document assertions: no new document or vector input contains a producer
   or corpus-host absolute path; source URIs round-trip the normative
   encoding for the section 17 character set; `ProjectFileV2` refs
   round-trip the exact parser; LegacyLocal incremental edit converges with
   full rebuild on the same generation.
5. Overlay assertions: matching attachment builds an overlay for the exact
   active code generation; activation without the attachment clears it
   atomically; monorepo single-ingestion and per-project edges; retiring one
   sibling preserves shared history.
6. The bridge daemon at the same commit passes the full parity harness plus
   the enumerated 4.3 changes.

## 12. Concurrency and security rules

- No lock is held across Git walking, blob reads, embedding, or index
  commit. The materializer prepares generations entirely off-lock and
  advances catalog state through the regular transact CAS; the rebuild
  manifest is written before, and verified after, the destructive step it
  authorizes.
- Read views pin catalog epoch, selector map, overlay map, searcher, and
  edge snapshot at request start; writers replace whole views and preserve
  fields they do not own.
- The single-writer actor discipline is preserved: materializer index scans
  run through the existing sanctioned read paths; generation re-emission
  runs inside the writer pass that owns the rebuild.
- Relative paths are validated at every generation, staging, codec, and
  re-emission boundary; the codec never decodes twice; no new id derives
  from a path, URL, alias, or string hash except the documented
  content-addressed generation ids over canonical bytes.
- Typed refusal vocabulary added this phase: `history_inventory_missing`,
  `history_commitment_mismatch`, `history_generation_referenced`,
  `empty_root_refused`, plus the existing shapes; every refusal preserves
  last-good state.
- Operator acknowledgement flags pass through only; no agent-side defaulting
  (RX-V1 discipline applies to any acknowledgement this phase adds).

## 13. Live bootsmoke protocol for every milestone

Same discipline as Phases 1 and 2: stablesigned binaries, throwaway state,
isolated ports, facade-produced migrated root (D-030), never the shared
production daemon. Two smokes per milestone that changes runtime behavior:

- Bridge parity smoke: register/list round trip, local staging, P2-D
  dual-read spot checks, plus this phase's enumerated 4.3 deltas asserted
  affirmatively (for example the schema-reset carryover after P3-E).
- Catalog-mode smoke: boot on the migrated root, remote-only activation and
  search with lease counters at zero, forced replacement round trip after
  P3-E, overlay round trip after P3-F, doctor sections present.

Smoke drivers are rebuilt from the thread notes; the hardening inventory
from thread-45180565 (SSE framing, isError traps, /tmp canonicalization,
listener-targeted traps) carries forward.

## 14. Bookend protocol

### Before implementation

1. Finish this plan.
2. `scripts/kimi-review.sh plan-review` in a fresh session (the fixed lens
   reads every `durable-project-catalog-phase*-impl.md`).
3. Any verdict other than exact `PASS` is `REVISE`; repair and `plan-resume`
   the same session. No implementation before the exact `PASS`.
4. Commit and push the clean plan milestone.

### After implementation

1. Finish all milestone commits, bootsmokes, and exact-ref cluster gates
   (P3-A through P3-F committed and pushed separately, each cluster
   verified).
2. `scripts/kimi-review.sh review` in a fresh session; the fixed scope
   remains `monolith-decomposition-pre-attempt-2..HEAD`.
3. Repair every finding, rerun gates, push, reverify, and resume the same
   session until the final verdict is exactly `PASS`.

GLM 5.2 fallback per the Phase 1 plan applies unchanged on genuine Kimi
disruption. Autonomous material decisions during implementation are recorded
in `DECISION_LEDGER.md` as D-034 onward.

## 15. Reviewer checklist

The plan reviewer must reject this plan unless it proves:

- dependency correctness: substrate types before staging conversion, staging
  before planning, materializer before any milestone that can trigger the
  destructive replacement, the schema bump isolated to exactly one milestone
  that also wires the guarded replacement, and the overlay after the cut;
- history and quarantine generations are constructed through exactly one
  shared creation path whose only callers are the materializer and the P3-F
  live refresh, the materializer's proofs bind to persisted Phase 1
  commitments through the new immutable asset, and every refusal arm
  preserves the last-good lexical and vector views including on crash
  (prepared/committed manifest recovery, bridge spill-lane recovery);
- unclaimed namespaces are representable only through the rebuild manifest,
  consistent with the catalog validation invariants, and the importer
  refusal reconciliation (section 4.4) is stated without weakening either;
- the catalog schema change (section 4.1) cannot break strict opens of
  existing v2 bytes, cannot forge `Ready` states, and amends the importer
  and governing text in the same milestone;
- the bridge window contract (section 4.3) is complete: every
  bridge-observable change is enumerated, tested, and justified by a
  governing-design sentence, and the schema-reset carryover eliminates the
  bridge history-loss window;
- remote-only projects are planned, preserved, edge-registered, GC-rooted,
  and searchable with zero leases, and the F1 through F8 defect closures
  each name their test;
- purge and freshness rekeying cannot strand pre-bump documents (the bump
  and the rekey ship together) and cannot delete last-good state for
  collected, unavailable, cutback-pending, detached, or empty-root projects;
- the filter-lane addition preserves the literal lane's permanence and adds
  no identity authority to it;
- no new document, vector input, embedding text, metric label, or response
  fabricates a host path, and the display-path fallback order matches
  governing section 10.2 with tier 1 explicitly deferred;
- selector, snapshot-id, and entity-ref shapes stay backward-parseable, the
  `legacylocal-` widening is explicit, and `rel_path_hash` stability is a
  stated decision;
- concurrency rules hold: no lock across walking/embedding/commit, one
  transact owner for catalog advancement, single-writer actor discipline,
  and read-view field preservation under the searcher-only republish;
- the exit-gate proof discharges the governing section 17 rows this phase
  owns (remote-only code path, Git and cutback rows named in section 11 of
  this plan) and defers the rest with their owning phase named.
