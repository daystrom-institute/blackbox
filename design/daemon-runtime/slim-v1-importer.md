---
title: "Slim v1 project catalog importer"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [catalog, project-identity, v1-import, migration, rollback, self-verification, archive]
brief: "Convert a v1 state tree into a MigratedV1 catalog by converting only the stores v2 reads (preserved project ids, all LegacyLocal), archiving every store v2 never opens under a hash manifest, rehearsing on a copy, committing through the existing migration transaction, and leaving local evidence that startup, doctor, verify and rollback check without any operator-side log collection."
status: "proposed; deferred, must be resolved before v2 merges to mainline"
---

# Slim v1 project catalog importer

## 0. Problem and invariants

A v2 binary on a v1 state tree boots in bridge mode (`LegacyV1` or
`AbsentBridge` from `probe_project_store_mode`). No operator surface moves
that state into catalog mode: genesis refuses any bundle whose v1
`projects.json` lists a project or whose legacy owner stores hold
project-scoped rows. Hosts that run v1 today cannot be observed: no logs,
state listings or reports can be collected from them. The importer is the
path from v1 to catalog mode, and it must prove its own result on the host
where it runs.

Invariants the design holds:

- **I1. Identity is preserved.** Every v1 project becomes a catalog project
  with the same 8-hex `project_id`. Id-keyed files (`edges/<pid>.jsonl`,
  `git_meta/<pid>.json`, code and vector keys, stamped rows) stay valid
  without renaming.
- **I2. Nothing is silently ignored.** Every entry at depth 1 of the state
  roots is classified by an explicit known-names table. An unknown entry
  refuses by name unless the operator names it in an override, and the
  override is recorded and bound into the commit evidence.
- **I3. Nothing is deleted.** Stores v2 never opens are moved, byte for
  byte, into `S/v1-archive/<txn>/tree/` under a hash manifest. Pre-images of
  every rewritten file are kept beside them.
- **I4. One commit point.** The existing `V1Migration` catalog transaction
  is the only commit point. Every step before it is undone by recovery;
  nothing after it changes live store content.
- **I5. Rehearse before touching live state.** The full plan is applied to a
  copy and the copy is re-opened strictly and checked before any live
  mutation.
- **I6. Evidence stays local and is re-checkable.** Marker, receipt and
  manifest are written on the host; `import-v1 --verify`, doctor and startup
  re-check them. The existing strict marker and journal binding is
  unchanged.
- **I7. Existing v2 states are untouched.** `FreshV2`, full-engine
  `MigratedV1`, and catalogs with cutovers applied are recognised and left
  alone.

Path roots below: `S` = state dir, `B` = bro home (default `S/bro`), `D` =
data dir (default `~/.local/share/blackbox`). All paths resolve through
`Config`, so environment overrides (`BLACKBOX_KNOWLEDGE_PATH`,
`BLACKBOX_NOTES_PATH`, `TRANSCRIPT_SEARCH_INDEX_PATH`,
`BLACKBOX_VECTORS_PATH` and siblings) relocate the store the table row
refers to, never the table.

## 1. Scope

### 1.1 Classes

Every entry the importer inspects lands in exactly one class.

| Class | Meaning | Members |
|---|---|---|
| `convert` | rewritten into a v2 shape | `S/projects.json` (v1 registry becomes the catalog in place; the v1 bytes become the `LegacyProjectStoreBackup` immutable asset) |
| `stamp` | kept in place; rows lacking `project_id` gain one by exact path | `blackbox-knowledge.json`, `blackbox-gaps.json`, `blackbox-threads.json`, `artifacts/`, `B/tasks.json`, `B/slack-channel-bindings.json`, `B/slack-proposal-links.json` |
| `keep` | v2 reads it as is, keyed by preserved id or global | `edges/<registered id>.jsonl`, `edges/explicit/`, `edges/observed/`, `edges/derived/<ns>/<registered id>.jsonl`, `edges/materialized/`, `edges/migrations/`, `edges/.versions/`, `git_meta/<registered id>.json`, `B/brofiles/`, `B/config.json`, `B/mcp.json`, allocator and fleet state, `transcript-sources/`, `conversation-sources/`, harness sessions, `v1-archive/` |
| `history` | read in place to build the commit-namespace asset, never moved | `D/index/` (Tantivy), the vector store |
| `archive` | moved into `v1-archive/<txn>/tree/` with a manifest row | `blackbox-notes.json`, `blackbox-pins.json`, `blackbox-roadmap.json`, `packets/`, `B/whiteboards/`, `B/badgey/`, `B/teamplates/`, gap spool (`gap-spool-imports.json`, `D/gaps/inbox/`), `S/backups/`, orphan edge stems, orphan `git_meta` files, bridge-only stores listed in 1.2, stray v1 atomic-write temps (`*.json.<pid>.<nonce>.tmp`) |
| `ignore` | lock and log files, never moved | `*.json.lock` sidecars, `project-catalog-migration.lock`, `.instance.lock`, log files |
| `catalog-family` | handled by the store transaction itself | `project-attachments.json`, `project-catalog-transaction.json`, `project-catalog-migration.json`, `project-catalog-migration-receipt.json`, `project-catalog-migration-assets/`, `project-catalog-stage/`, `project-catalog-backups/` |

The stamp class is optional for correctness today (rows without an id fall
back to the owner store's path predicate) and required for exactness: once
the catalog owns identity, a retire-and-add at the same path mints a random
id, and an unstamped row would leak into the new project. Unattached
projects (section 5.2) also have no path for the fallback to match.

Derived content is not rebuilt by the importer. The first v2 start sees an
index schema mismatch (v1 writes an older index schema than v2 runs) and
runs the existing catalog schema replacement, which carries commit documents
through history generations and re-derives the rest (section 6). The
importer never moves or deletes `D/index/` or the vector store: for projects
whose checkout is gone, the index is the only copy of their commit history.

### 1.2 Bridge-created stores

Installing a v2 binary and starting it once in bridge mode creates stores a
mainline v1 host never had. Both expected host shapes (mainline v1 after the
v2 install, and a beta bridge host) therefore carry them, and the table
lists them explicitly:

| Store | Class | Reason |
|---|---|---|
| `B/checkout-registry.json` | keep | discovery index, opened in both modes |
| `B/checkout-access-observations.json`, `knowledge-transport-observations.json`, `render-operations/` | keep | opened in both modes; observation state, not identity |
| `B/render-locality-observations.json` | archive | retired locality cutover evidence; no current binary opens it |
| `B/resolver-compat-observations.json` | archive | written by earlier bridge boots; no current binary opens it |
| `S/code-sources/` | archive | bridge activations are keyed by v1 id with no catalog binding; catalog mode re-creates the directory and re-collects from attached checkouts (UNVERIFIED: catalog-mode open accepts a `LegacyLocal` project with no activation; `add --legacy-local` projects imply it does) |
| `publisher-refs.json` (in `S` or `B`; the runtime reads one and reindex writes the other) | archive | pins bind published scopes; every imported project is `LegacyLocal`, and publication re-pins after `promote` |
| `accepted-publications/` | archive | same reason |
| cutover markers and receipts (`*-cutover-marker.json`) | archive | cannot be produced without a catalog; a stray one is leftover state from a removed catalog |
| `S/render-locality-cutover-*`, `S/code-source-locality-cutover-*`, `S/blame-locality-cutover-*`, `S/code-source-locality-observations.json` | archive | retired locality cutover markers, receipts and evidence; no current binary reads them, and daemon startup moves them into `S/cutover-artifacts/retired-locality-<timestamp>/` |
| `aliases` field inside v1 `projects.json` records | converted | carried into `operator_aliases` |
| `.bbox/local/checkout-id` markers inside checkouts | reused | an existing marker id is reused for the attachment rather than minted again |

### 1.3 Out of scope

Published-scope proofs, publisher pins and G1 content, code-source
activations, repo-grouping beyond one history record per v1 `repo_id`,
namespace-collision resolution, the per-row legacy path ledger, and
provenance git notes (they live in repositories, not the state tree, and
the importer never touches repositories beyond checkout-id markers).
Publication goes through the existing `project-catalog promote` after
import.

## 2. Command surface

```
blackbox project-catalog import-v1 --preflight   [--unknown <name>=archive|keep]...
blackbox project-catalog import-v1 --apply       [--unknown <name>=archive|keep]...
blackbox project-catalog import-v1 --verify
blackbox project-catalog import-v1 --recover
blackbox project-catalog import-v1 --rollback
```

All modes emit the existing single redacted JSON envelope plus a short human
summary on stderr. Every refusal names the entry, the class rule it broke,
and the command that resolves it. No mode needs a reviewed report or
resolution file: `--apply` recomputes the plan under the exclusive lock,
rehearses it, and re-checks source hashes before the commit (section 4.1).

`--unknown` is the only override. It names one depth-1 entry exactly (no
globs), chooses `archive` or `keep`, and refuses if the named entry does not
exist. The set is written to `v1-archive/<txn>/overrides.json`, whose hash
becomes the marker's `resolution_artifact_sha256`. There is no override for
a parse failure, a duplicate id, or a class the table refuses.

The bridge-mode daemon logs one line at startup, and doctor reports one
finding, when the probe returns `LegacyV1` with at least one project:
`v1 project state is in bridge mode; run blackbox project-catalog import-v1
--preflight`.

## 3. Preflight

Preflight is read-only. It takes the shared lifetime lock, so it can run
beside a live bridge daemon.

1. **Store mode.** Run `probe_project_store_mode`.
   - `CatalogV2`: report the origin and stop (section 7). Exit success.
   - `AbsentBridge`: report "no v1 registry; use genesis" and stop.
   - Probe error (half pair, torn read, invalid snapshot): refuse with the
     probe's code. If a `v1-archive/*/import-journal.json` is `Prepared`,
     point at `--recover`.
   - `LegacyV1`: continue. A v1 registry with zero projects imports like any
     other (a zero-project `MigratedV1` catalog), so the dead stores are
     archived on the same path.
2. **Enumeration.** List `S`, `B` and `D` to depth 1 (nofollow), plus the
   fixed subtrees the table classifies at depth 2 (`edges/`, `git_meta/`,
   `edges/derived/<ns>/`). Resolve every configured store path through
   `Config`; a relocated store is classified at its configured path.
   - Symlinks refuse by name (the archive move and the manifest hash are
     defined only on real files and directories).
   - Every entry must match exactly one table row; zero matches refuse
     unless an override names it; two matches is a table bug and refuses.
   - Absence of any known name is never an error. The table has no
     "required" rows except `projects.json`.
3. **Parse.** Parse every `convert`, `stamp` and `keep` store with the v2
   runtime parser the daemon uses (so a slack file that would panic startup
   refuses here). Parse the v1 registry with the bridge registry parser.
   `archive` stores are never parsed: a corrupt notes file archives by
   bytes.
4. **Project plan.** For each v1 record: same id, `LegacyLocal`,
   `display_name` from the path basename, aliases carried. Refuse on
   duplicate ids or duplicate canonical paths (v1 `load_store` does not
   validate, so hand-edited duplicates are possible). Nested roots are
   allowed; the stamp rule below handles them, and the rehearsal's strict
   attachment validation decides whether nested attachments are admissible
   (UNVERIFIED).
   - Checkout present (canonical path is a directory): plan an attachment
     and a checkout-id marker action (reuse an existing marker id).
   - Checkout absent: plan an unattached project (section 5.2).
5. **History plan.** Scan the index with the cross-schema commit scan
   (`history_generations::scan_commit_documents`, which reads any schema
   marker) and the vector store's migration snapshot. Plan one
   `RepoHistoryRecord` per distinct v1 `repo_id` among git projects, with
   `primary_namespace` = that `repo_id`, linked from each project carrying
   it. Plan the `LegacyCommitNamespaceInventory` asset rows with attribution
   `Proved{ids}` for namespaces matching a planned record, `Unclaimed`
   otherwise (section 6).
6. **Stamp plan.** For each stamp-class row without `project_id`, resolve
   its path key with the v1 deepest-root rule over the v1 canonical paths.
   Resolved rows are planned for stamping; unresolved rows stay unstamped
   and are counted. A row that already carries a `project_id` (bridge
   writes stamp the v1 id) must equal the planned id or the preflight
   refuses.
7. **Capacity.** Sum the bytes the rehearsal copies (convert, stamp and
   keep entries that the rehearsal re-opens; not `history` or `archive`)
   and refuse if the filesystem holding `S` lacks that plus a fixed margin.
   Archive moves are same-filesystem renames; an archive entry on another
   filesystem (a relocated store) refuses with its path.
8. **Report.** Emit what converts, stamps (per store: stamped, already
   stamped, unresolved), keeps, archives (with sizes), and which projects
   attach or stay unattached, plus the history plan counts. The report is
   hashed; its canonical bytes become `plan.json`.

## 4. Apply

### 4.1 Exclusion

- Take the lifetime lock exclusively. A v2 daemon or preflight holding it
  shared makes apply refuse with "stop the daemon".
- Claim the daemon instance-lock roots, so a v2 daemon cannot start
  mid-apply (UNVERIFIED that the instance-lock API is usable from the
  offline CLI).
- A mainline v1 daemon takes neither lock. Apply holds each stamp and
  convert store's v1 `<store>.json.lock` sidecar exclusively for the
  install window, and re-hashes every convert and stamp source immediately
  before the commit. A mismatch against the rehearsed pre-image aborts with
  no live mutation.

### 4.2 Rehearsal

Automatic, never operator-staged.

1. Create `S/v1-import-rehearsal/<txn>/` (same filesystem; the
   existing rehearsal-separation check proves it cannot alias the live
   layout). Copy the convert, stamp and keep entries into it. Archive-class
   entries are not copied; their manifest rows are hashed from the live
   originals.
2. Apply the whole plan against the copy through the same code path the
   live apply uses, with a `Config` rooted at the rehearsal directory.
   Checkout-id marker actions target rehearsal-local marker paths; live
   checkouts are never written during rehearsal.
3. Re-open strictly and check invariants V1 to V10 below. Any failure stops
   with the failing invariant named; live state is unchanged; the rehearsal
   directory is removed.

The rehearsal does not run the index schema replacement (the index can be
large, and the replacement already refuses before dropping the old index if
materialization fails). It does run the asset load and the checks the first
boot depends on (V5).

### 4.3 Commit sequence

1. Write `v1-archive/<txn>/import-journal.json` in state `Prepared`, plus
   `plan.json`, `overrides.json` and `manifest.json`. Fsync.
2. Copy each stamp-class file's pre-image to `v1-archive/<txn>/preimages/`
   and record its hash in the manifest.
3. Move each archive entry to `v1-archive/<txn>/tree/<relative path>` by
   rename. The plan is fixed, so recovery determines each entry's location
   by inspection; no per-move journal append is needed.
4. Install stamped post-images by atomic replace.
5. Run the existing `V1Migration` transaction (`validate_migration_plan`,
   `transact_migration_classified`) with the participant and asset set in
   4.4. This is the commit point.
6. Set the import journal to `Committed`, write `receipt.json`, remove the
   rehearsal directory.
7. Post-commit verification: re-run V1 to V11 against live state. A failure
   here does not roll back (the commit is durable); it is reported, written
   into the receipt, and surfaced by doctor and `--verify`.

Recovery (`--recover`, and the first step of every other mode):

- Import journal `Prepared` and no marker for `<txn>`: run the store's
  existing `recover_locked`, then restore pre-images, move archived entries
  back, and mark the journal `RolledBack`. The result is byte-identical to
  the pre-apply tree except for the retained `v1-archive/<txn>/` evidence
  and any checkout-id markers already minted (monotonic by design of the
  existing transaction).
- Import journal `Prepared` and a committed marker for `<txn>`: roll
  forward to step 6.

### 4.4 Transaction content

Reuses the store transaction unchanged; no new participant role and no
marker wire change.

| Item | Value |
|---|---|
| `Catalog` participant | v2 catalog snapshot, epoch 1, origin `MigratedV1{txn}`; expected old image = v1 `projects.json` bytes |
| `Attachments` participant | one active attachment per present checkout, `LegacyLocal` (no `validated_scope`) |
| `EffectiveSourceManifest` participant | the empty code-source manifest (required by plan validation) |
| `MigrationMarker` | as today |
| `LegacyProjectStoreBackup` asset | exact v1 `projects.json` bytes |
| `LegacyCommitNamespaceInventory` asset | rows from the cross-schema scan (section 6) |
| checkout identity actions | one per attachment |
| publisher pins, dispositions, quarantine | empty |
| `inventory_sha256` | sha256 of `manifest.json` |
| `report_artifact_sha256` | sha256 of `plan.json` |
| `resolution_artifact_sha256` | sha256 of `overrides.json` (canonical empty set when no override) |
| `code_source_inventory_sha256` | sha256 of the empty code-source snapshot |

UNVERIFIED: that an empty `EffectiveSourceManifest`, empty code-source
snapshot and empty quarantine authority pass `validate_migration_plan` and
`ProjectCatalogMigrationMarkerV1::validate`. The rehearsal proves it on
every host; the test matrix proves it before release.

### 4.5 Invariant checks

Run in rehearsal (V1 to V10) and post-commit (V1 to V11).

- **V1.** Catalog project id set equals the v1 record id set.
- **V2.** Each attachment's `checkout_project_dir` equals the canonicalised
  v1 `canonical_path`; status active; scope `LegacyLocal`.
- **V3.** Each unattached project's checkout was absent in the plan.
- **V4.** Each v1 git project with a `repo_id` links the record whose
  `primary_namespace` equals it; no `ambiguous_namespaces` exist.
- **V5.** The asset loads through `load_legacy_commit_namespace_inventory_asset`;
  its rows equal the scan; if any history record exists, the namespace
  inventory is non-empty (the first-boot lineage proof refuses an empty
  inventory).
- **V6.** Each stamped store parses with the v2 runtime parser; row count
  equals the pre-image; each row equals its pre-image with `project_id`
  removed; stamped rows equal the plan; no pre-existing id changed.
- **V7.** Each archived entry hashes to its manifest row at its archive
  location and is absent at its origin (post-commit); present at its origin
  with the manifest hash (rehearsal).
- **V8.** Re-enumeration of the post-state classifies every entry with no
  convert or archive class left at an origin.
- **V9.** `ProjectCatalogStore::open_existing` succeeds, which runs the
  existing strict marker, journal and receipt binding.
- **V10.** Runtime record derivation yields a project record for every
  attached project, and the GC-registered id set covers every kept edge stem
  and `git_meta` file.
- **V11.** Live checkout verification (`verify_live_checkout`) passes for
  every attachment.

V9 and V10 need a strict, service-free open of the durable stores. None
exists today (`blackboxd` has no check mode and doctor runs inside a live
daemon), so the store-opening half of `src/server/open.rs` is extracted into
one function that the daemon, the importer and `--verify` share.

### 4.6 Evidence on disk

| Path | Written | Checked by |
|---|---|---|
| `project-catalog-migration.json` (marker) | commit | every open (existing strict check) |
| `project-catalog-migration-receipt.json` | commit | every open (existing) |
| `project-catalog-migration-assets/` (v1 registry backup, commit-namespace asset) | commit | materializer, rollback |
| `v1-archive/<txn>/import-journal.json` | step 1, 6 | startup, recover |
| `v1-archive/<txn>/plan.json`, `overrides.json`, `manifest.json` | step 1 | marker binding (by hash), verify, rollback |
| `v1-archive/<txn>/preimages/`, `tree/` | steps 2, 3 | verify, rollback |
| `v1-archive/<txn>/receipt.json` | step 6, 7 | doctor, verify |

### 4.7 Startup after import

- **Unchanged:** the strict origin check (`FreshV2` has no marker;
  `MigratedV1` needs marker, journal or receipt, and the field binding).
  Full-engine `MigratedV1` stores and `FreshV2` stores open exactly as
  today.
- **Unchanged:** the materializer's asset requirement and the rebuild
  startup gate. On the first start the schema replacement commits a Drift
  manifest and the gate re-founds it to Equality from the pinned
  generations; no offline rebuild is needed.
- **New, one check:** any `v1-archive/*/import-journal.json` in `Prepared`
  refuses startup with a pointer to `import-v1 --recover`. This keeps a
  bridge daemon from writing into half-stamped stores whose recovery would
  otherwise discard those writes.
- **New, doctor section `v1_import`:** receipt present; `manifest.json`
  hash equals the marker's `inventory_sha256`; post-commit verification
  result; unattached imported projects with their v1 paths; unresolved and
  orphan counts. Doctor hashes only the manifest file; `--verify` re-hashes
  the tree.

### 4.8 Rollback

`import-v1 --rollback` returns the tree to the `LegacyV1` shape.

Preconditions (each refusal lists the differences):

- Catalog origin `MigratedV1{txn}` with `v1-archive/<txn>/receipt.json`
  (full-engine catalogs are refused).
- Exclusive lifetime lock.
- The project set, scopes and attachments equal what the import committed.
  Materialization flips on history records and attachments' runtime status
  are allowed. A project added, promoted, retired or scope-migrated after
  import refuses; the operator reverses it with the existing verb first.
- No cutover marker is present.

Steps:

1. Re-hash `tree/` and `preimages/` against the manifest.
2. Restore `projects.json` from the `LegacyProjectStoreBackup` asset.
3. Move each archived entry back; refuse per entry if its origin now exists
   with different content.
4. For each stamped store: if its current hash equals the import post-image,
   restore the pre-image; otherwise keep the current bytes (they carry
   post-import rows; stamped `project_id` equals the v1 id, which the bridge
   reads natively and mainline v1 parsers ignore because they do not deny
   unknown fields; UNVERIFIED for the mainline artifacts store).
5. Move the catalog family into `v1-archive/<txn>/rolled-back/`, write
   `rollback-receipt.json`, set the journal to `RolledBack`.

Rollback never touches the index, vectors or history generations. A v2
bridge boot reads them as they are. A mainline v1 binary sees a newer index
schema; whether it rebuilds cleanly is UNVERIFIED, and a v1 rebuild would
drop commit history for absent checkouts. Checkout-id markers stay. A later
`--apply` starts a new `<txn>`.

## 5. Project classification

### 5.1 All projects are LegacyLocal with the same id

`validate_project_id` accepts the 8-hex form. v1 ids are a hash of the
canonical path at registration, but rename keeps the id, so the importer
never re-derives an id from a path. `promote` works on `MigratedV1` exactly
as on `FreshV2`: it needs a `LegacyLocal` project, an active attachment, and
a committed `.bbox/config.toml` `repo_id` at HEAD proving the scope. That is
the only path to `Published`.

### 5.2 Absent checkouts

A project whose checkout is absent imports unattached. Consequences, with
the design response:

- A project with no active attachment has no runtime project record, so it
  is invisible to selectors, and its stamped rows are hidden until it is
  attached. The GC registered set uses catalog project keys, so its edge
  and `git_meta` files are protected.
- `bbox_project_attach` refuses without an existing checkout-id marker, and
  catalog-mode register creates a new `LegacyLocal` project with a fresh id
  when no attachment matches. Re-registering the reappeared checkout would
  duplicate the project. The design changes the catalog-mode register
  composite: before minting, it looks up the canonical checkout directory in
  the import receipt's unattached-project path map; an exact match attaches
  to that project id (minting the marker) instead of creating a project.
  Doctor lists unattached imported projects with their v1 paths.

## 6. Git history for absent checkouts

Facts that frame the choice:

- v1 `git_meta/<pid>.json` holds only the ingest cursor (`last_ingested_sha`).
  It holds no commit content.
- Commit content lives in the Tantivy index as commit documents keyed by
  namespace (the v1 `repo_id`), plus commit vectors.
- v2 carries commit documents across a schema replacement through history
  generations sourced from the old index; checkouts are not read for it.
- A `MigratedV1` catalog without the commit-namespace asset fails the
  materializer at the first schema mismatch, which is the first v2 start.
- A catalog `LegacyLocal` project gets a `repo_id` and git tracking only
  through a linked `RepoHistoryRecord`. Without one its `git:<pid>` rows are
  purged on the next reindex and re-emitted history is unclaimed.
- The standalone corpus-index migration snapshot rejects an index at another
  schema version and returns zero commit rows; the cross-schema
  `scan_commit_documents` does not.

Options:

| Option | Mechanism | Result |
|---|---|---|
| A. Asset from the index scan plus history records | build the asset from `scan_commit_documents` and the vector snapshot; one `RepoHistoryRecord` per v1 `repo_id` | history kept and attributed for present and absent checkouts; boot unaided; Equality proof after first start |
| B. FreshV2-like treatment | import with origin `FreshV2` | commit documents still carried, but unowned; no marker allowed, so the import loses its binding to the strict origin check |
| C. Refuse unless every checkout is present | preflight gate | blocks hosts with deleted checkouts; gains nothing, since history never came from checkouts |
| D. Defer until the checkout reappears | import unattached, attach later | covers attachment (section 5.2) but not history; the purge happens before reappearance without a history record |

A thin asset built from `git_meta` is not an option: `git_meta` has no
content to inventory.

**Recommendation: A**, with D's re-attachment for the project itself.
Conditions the test matrix must settle, each UNVERIFIED today:

- Commit documents of an unattached project with a history record survive
  the schema replacement and subsequent incremental reindexes. If they do
  not, the reindex purge rule retains `git:<pid>` rows for catalog projects
  that have a history record and no active attachment, as it already does
  for lease-denied projects.
- A history record whose namespace has zero commit documents does not break
  the first-boot proof when other namespaces are non-empty. If it does, the
  importer writes records only for namespaces the scan found, and V5 already
  refuses the all-empty case.
- A v1 index that is absent or unreadable yields an empty asset and zero
  history records, and the project keeps git tracking off until `promote`.

## 7. Beta-branch state shapes

| Shape | Detection | Importer behaviour |
|---|---|---|
| `LegacyV1` bridge, with bridge-created stores and v1-id-stamped rows | probe `LegacyV1` | normal import; bridge-created stores classified by section 1.2; pre-stamped rows must equal the planned id |
| `LegacyV1` with catalog-family siblings from a rolled-back full-engine migration | probe `LegacyV1` plus sibling files | terminal rollback journal: the transaction supersedes it as it already allows; a `Prepared` journal refuses and points at store recovery (UNVERIFIED: supersession of a terminal rollback by a new migration end to end) |
| `AbsentBridge` | probe | refuse with "no v1 registry; use genesis" |
| `FreshV2` (genesis) | origin | no-op; report and exit success |
| `MigratedV1` from the full engine | origin plus no `v1-archive/<txn>/` | no-op; its marker, receipt and assets stay readable through the kept store code; unstamped rows keep the path fallback |
| `MigratedV1` from this importer | origin plus `v1-archive/<txn>/receipt.json` | `--apply` is a no-op; `--verify` and `--rollback` apply |
| any catalog with cutovers | cutover markers beside a catalog | no-op; rollback refuses |
| half pair, torn or invalid | probe error | refuse with the probe code; `--recover` if an import journal is `Prepared` |

## 8. Hazards

**Storage GC deletes unregistered edge stems: confirmed, and wider than the
import.** The GC runs about five seconds after start and every six hours,
destructively. It deletes a top-level `edges/<stem>.jsonl` (and stems under
`edges/derived/<ns>/`) whose stem is not a registered id and whose mtime is
older than 30 days. The registered set in bridge mode is the v1 registry's
ids; in catalog mode, every catalog project key. With preserved ids the
import does not widen the set at risk, but stems for projects v1 had removed
and non-project stems are deleted, and a rename keeps mtime. `explicit/` and
`observed/` are exempt. Design response:

- The importer archives orphan stems (top level and under `derived/`) so
  nothing the GC could delete remains after import.
- The same deletion already happens on the first bridge boot, before any
  import. Release requires the orphan rule to be non-destructive while the
  store mode is `LegacyV1` or `AbsentBridge`.
- When the in-memory edge graph and its GC are removed, the orphan rule
  goes with them; the importer's archive rule stays because it costs
  nothing and keeps I3.

**Inventory refusal when notes, pins or whiteboards are absent: confirmed
for the old inventory, refuted for genesis, eliminated here.** The old
inventory treats every absent owner except slack and provenance as
`Missing` and refuses. Genesis treats absent as empty and refuses only a
corrupt file. The slim importer has no owner inventory; absence of any
known name is never an error (section 3, step 2), and archive-class stores
are never parsed.

**Other hazards found:**

- A mainline v1 daemon takes no lifetime lock (section 4.1 covers it).
- The first v2 start fails without the commit-namespace asset (section 6).
- Unattached projects duplicate on re-register (section 5.2).
- Nothing in the runtime enumerates `S` or `B` as a whole, so an entry left
  behind is inert; strictness is the importer's own guarantee, not a
  runtime one.
- `B/teamplates/` with an invalid JSON file fails the whole listing; with
  `bro_team` removed it is never read, and it archives.

## 9. Removed stores

- `edges/` holds code-source snapshot and sidecar state only; the importer
  keeps id-keyed stems and archives orphans. `observed/` and tool_call index
  content have no reader and archive or drop with the schema replacement.
- `teamplates/` and team records are inert and archive. No teamplate content
  is converted.

## 10. Code plan

### 10.1 Reused unchanged

- Store transaction, recovery, marker, receipt and strict origin check
  (`project_catalog_store.rs`): `validate_migration_plan`,
  `transact_migration_classified`, `recover_locked`,
  `verify_origin_marker_locked`, rehearsal separation check.
- Lifetime lock, `ProjectCatalogPaths`, `validate_project_id`.
- Bridge registry parser for v1 `projects.json`.
- Runtime parsers of the stamp-class owner stores, and their existing
  single-row stamp functions for those six owners.
- `history_generations::scan_commit_documents`, the vector migration
  snapshot, `load_legacy_commit_namespace_inventory_asset`.
- Schema replacement, history materializer, rebuild startup gate.
- `promote`, `retire`, genesis (its census retargeted to the known-names
  table).

### 10.2 New

| Unit | Rough size |
|---|---|
| known-names table and enumeration (table as data, one row per name or family) | 350 |
| preflight planner and plan-to-`MigrationPlanDraftV1` builder | 600 |
| stamp pass over six owners | 300 |
| archive, manifest, pre-images, import journal, recover | 450 |
| history asset and record builder | 250 |
| verify (V1 to V11) and rollback | 500 |
| strict service-free store open extracted from `open.rs` | 200 |
| register composite import-aware attach | 100 |
| startup `Prepared` check, doctor section, bridge pointer | 150 |
| CLI | 250 |

About 3.2k production lines and 3k to 4k test lines.

### 10.3 Deleted

- Owner inventory and adapters (`project_catalog_inventory.rs`,
  `project_catalog_inventory_adapters.rs`), keeping only the empty
  quarantine-authority constructor the transaction needs (moved into the
  store module).
- The migration facade except the layout, error types and the asset loader.
- Durable backfill and the stamper (`project_catalog_backfill.rs`,
  `src/project_catalog_stamper.rs`) and the `durable-backfill` command.
- Dead-owner codecs and stores: notes owner, packet tree (retire drops its
  packet discharge), notes, pins, `bbox-whiteboards`, and the owner
  snapshot contract beyond what genesis still reads.
- `path-free-rebuild`: its only predecessor is the backfill completion
  journal, and the startup schema replacement already produces an Equality
  manifest. Delete it with the backfill unless a same-schema offline rebuild
  is still wanted, in which case its predecessor binding moves to the import
  receipt (UNVERIFIED: no other consumer of the offline rebuild).
- Migration facade, backfill and rehearsal-fixture tests.

The corpus-index, edge-sidecar and vector `migration_inventory.rs` modules
stay while the asset, history generations and retire read them.

Order: known-names table with its coverage check and the extracted strict
open; importer with its test matrix; the bridge-mode GC guard; deletions.

### 10.4 Test matrix

Fixtures exist from the first commit and use neutral synthetic names.

| Fixture | Must show |
|---|---|
| never provisioned: no `S`; empty `S`; no `D` | refuse or no-op with a clear message; no directory created |
| mainline legacy-shaped: every v1 store in v1 wire format, including a v1-schema index with commit documents | import, rehearsal, V1 to V11, first boot through schema replacement and the startup gate, rollback, re-import |
| beta bridge-shaped: mainline plus every bridge-created store, v1-id-stamped rows, existing checkout-id markers, `aliases`, publisher refs in both locations, code-source activations | same as above; markers reused; pins and activations archived |
| oversized: catalog near its entry limit, stores near their byte caps, large archive trees, many namespaces | bounded memory; capacity refusal when space is short; limits refuse by name |
| orphaned: edge stems for removed ids and non-project stems, `derived/` orphans, orphan `git_meta`, rows matching no project, unclaimed commit namespaces, duplicate ids, nested roots, absent checkouts, `repo_id` absent | orphans archived; duplicates refuse; unattached projects re-attach through register without duplication; commit documents of unattached projects survive the replacement and later reindexes |
| hostile: unknown entry with and without override, stale override, symlinked entry, relocated store, corrupt archive-class store, corrupt stamp-class store, non-UTF-8 name | refusals name the entry; overrides land in `overrides.json` and the marker |
| catalog shapes: `FreshV2`, full-engine `MigratedV1`, slim `MigratedV1`, terminal rollback siblings, half pair | section 7 behaviour |
| crash injection at every IO step (store IO fault injection plus an importer IO seam) | recover yields the exact pre-state or the complete post-state |
| concurrent v1-style writer between rehearsal and commit | abort with no live mutation |
| GC after import and after a bridge boot | deletes nothing imported; bridge-mode orphan rule non-destructive |

A coverage test fails when any state writer in the workspace names a
depth-1 entry the known-names table does not classify.

### 10.5 Release gate

v2 does not release until all of these hold:

- The importer, recover, verify and rollback are on the release branch and
  every matrix row passes in CI.
- The known-names coverage test passes.
- The bridge-mode GC guard is in.
- The bridge startup pointer and the doctor `v1_import` section ship.
- A checked-in rehearsal script builds mainline v1 binaries, drives them in
  an isolated HOME and state directory against synthetic git repositories to
  produce a v1 state tree written by v1 code, boots a v2 build on it in bridge
  mode, and runs preflight, apply, first boot, verify, rollback, bridge boot
  and re-apply with every invariant passing. It proves the importer against
  what v1 writes; state a real host accumulated beyond that is covered by the
  unknown-name refusal, not by the rehearsal.
