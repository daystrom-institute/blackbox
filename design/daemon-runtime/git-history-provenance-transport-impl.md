---
title: "Typed Git-history transport implementation plan"
kind: design
lifecycle: archived
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, git-history, collector, typed-transport, checkout-leases]
brief: "Replace published catalog-mode checkout Git-history refresh with typed, scope-authorized producer transport while preserving the LegacyLocal history adapter and the Phase 3 history substrate."
---
# Typed Git-history transport implementation plan
Date: 2026-07-26
Baseline: branch `beta/blackbox-v2`, committed predecessor `fce0861ac6ae9b002832c0b78d7812cbbe0ea869`.
Governing design: [`durable-project-catalog-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-impl.md).
Transport substrate: [`distributed-code-source-collector-impl.md`](../../../../../design/daemon-runtime/distributed-code-source-collector-impl.md).
History dependencies: [`durable-project-catalog-phase3-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase3-impl.md) and [`durable-project-catalog-phase6-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase6-impl.md).
Decision authority: [`DECISION_LEDGER.md`](../../../../../DECISION_LEDGER.md). This plan uses slice-local decisions `GH-FD-*`; D-043 records GH-C's certified P3-F caller-list and selector-source amendment.

> **Status: archived.** Typed Git-history transport (GH-A through GH-C) is
> the live design. The GH-F overlap parity gate and the GH-G strict cutover
> (marker, receipt, checkout parity proof, and the `git-transport-cutover` and
> `git-transport-checkout-parity` subcommands) are retired: no daemon loads
> them, and the first start archives any left in the state directory. History
> ownership follows the journal-currency rule described in
> [the collector guide](../../docs/code-source-collector.md#git-history-ownership):
> a repository whose committed activation journal is current is
> transport-owned, and no other writer touches its history.

## 1. Required outcome
At this slice's exit gate, proved against strict catalog state after Phases 3 through 6 have landed:
1. A scope-authorized producer publishes one complete reachable Git-history snapshot without the corpus host opening a checkout or invoking Git.
2. The corpus validates the typed snapshot, feeds it through the single Phase 3 history-generation creation path, activates repo-level commit documents once, and builds project overlays for matching active code generations.
3. For each `Published` repo governed by transport authority, Git-history checkout refresh is suppressed while the repo is transport-current, meaning a verified `ProducerTransport` overlay remains the current selector arm under GH-FD-7. Before marker coverage, any loss of transport currency clears that arm and resumes P3-F attachment refresh; those deltas are expected evidence whenever the predicate is false, regardless of prior transport publication. After marker coverage, no-fallback rules keep `CheckoutAccessKind::GitHistory` at zero. A never-covered `Published` repo blocked by unassigned or split members retains P3-F attachment refresh until it is coverable. Attached Git `LegacyLocal` projects retain validated lease-backed refresh under either `RepoHistoryAuthority::LocalProject` or `RepoHistoryAuthority::LegacyNamespace`; both are outside producer transport authority. Bridge mode remains rollback-compatible.
4. Checkout detach leaves accepted code, commit documents, history health, snapshot receipts, and recovery state intact.
5. Published knowledge and gaps remain deferred: no `AcceptedPublicationStore`, publisher binding, alias, provisional overlay, or knowledge-write authority changes.
6. The `G19` report proves the Git-history rows are path-free for `Published` transport repos in their per-repo, per-capability post-swap and post-cutover windows, separately reports expected overlap, never-covered blocked-Published, and both retained `LegacyLocal` authority rows, and truthfully leaves published knowledge plus the final all-adapter zero-observation gate open.

## 2. Fixed cross-document anchors
Each citation quotes its section so renumbering or semantic drift is detectable. Later sections cite anchor labels.
| Anchor | Verified phrase | Binding consequence |
|---|---|---|
| `G11-A` | Governing section 11: "Git history becomes an attachment-backed immutable overlay with identity:" | Transport overlays require a surgical source-identity amendment while preserving overlay selection semantics. |
| `G11-B` | Governing section 11: "Every history generation is a complete, self-contained snapshot, never a cursor delta." | Every accepted logical snapshot is complete. |
| `G11-C` | Governing section 11: "Old `COMMIT_TOUCHED_FILE` edges cannot target the new snapshot." | Overlay matching is exact on code generation and repo head. |
| `G11-X` | Governing section 11: "Phase 3's pre-replacement history materializer, reused by the Phase 6 path-free-rebuild subcommand and by the Phase 3 live history refresh, owns the single creation path for `RepoHistoryGeneration` and `RepoHistoryQuarantineGeneration`; no other code constructs those generations." | The producer caller requires a surgical caller-enumeration amendment while preserving one constructor. |
| `G12-A` | Governing section 12.1: "The request carries only scope." | Requests never carry project id, repo-history id, namespace, or attachment id. |
| `G12-B` | Governing section 12.1: "An upload cannot create, rename, attach, select, or delete a project." | Routes resolve existing catalog authority only. |
| `G-D12` | Governing decision 12: "Catalog retire refuses while any configured producer assignment targets the project. The assignment is removed first." | Retiring a covered member changes its repo's assignment commitment before catalog membership changes. |
| `G14-H` | Governing section 14 Git-history row: "no current-file overlay, stale commit docs labeled" | Missing transport degrades history without rolling back code. |
| `G16-L` | Governing section 16: "No lock is held across filesystem walking, Git, embedding, or index commit." | Producer Git, validation, materialization, publication, and CAS are separate phases. |
| `G16-A` | Governing section 16: "Authentication happens before bounded request parsing, and scope membership is checked before any durable upload mutation, as in the collector design." | Reuse bearer middleware and grants before parse or write. |
| `G19` | Governing section 19: "the same scope-bound producer credential infrastructure for typed Git-history and published knowledge transports" | This is the Git-history slice; knowledge is next. |
| `C4.1` | Collector section 4.1: "Wire requests carry only a normalized `PublishedScope`" | Reuse `PublishedScope` as caller-supplied authority. |
| `C4.3` | Collector section 4.3: "Authentication proves the configured producer, not the truth of its bytes." | Validate graph closure, hashes, schemas, paths, and order corpus-side. |
| `C5` | Collector section 5: "`bbox-code-source`, a small leaf crate owning ... versioned wire structs and structured error codes" | Use the same leaf-contract and leaf-store dependency shape. |
| `C6.2` | Collector section 6.2: "The API is resumable and idempotent" | Persist sessions, contiguous pages, hashes, and replay-safe finalize. |
| `C8` | Collector section 8: "Generation manifests are immutable after completion." | Source generations never mutate in place. |
| `P3-D` | Phase 3 section 8: "builds and proves the machinery" | Consume the landed history store, materializer, and rebuild manifest. |
| `P3-F` | Phase 3 section 10: "one shared creation path whose only callers are the materializer and the live refresh" | Add producer refresh as a caller, never a second constructor. |
| `P6-R` | Phase 6 section 3.4: "`path-free-rebuild` subcommand is a thin caller of that creation path; it MUST NOT specify a parallel manifest writer" | Add no parallel rebuild or recovery manifest. |
| `P6-C` | Phase 6 milestone P6-C: "Routine `transact` epoch advances (including P3-F live history refresh) are tolerated." | Cutover startup freshness cannot require equality with the apply-time catalog epoch. |
| `P6-CLI` | Phase 6 section 3.1: "Both new commands produce the D-020 versioned result envelope. The envelope `command` values are snake_case" and preflight resolves configured state through `ConfigArgs` under D-021. | The cutover verb uses the same report/resolution, envelope, naming, and config-precedence contract. |

## 3. Survey of committed `HEAD`
### 3.1 Producer authentication is reusable
Verified caller path:
1. `bbox_config::config::RawCodeCollectionConfig` reads strict `[code_collection]`.
2. `CodeCollectionProducerConfig` supplies `producer_id`, `token_file`, and `scopes`.
3. `src/server/code_source.rs::build_snapshot` validates ids, loads `bro_rpc::ServiceToken`, rejects duplicate token digests and scopes, and resolves each scope.
4. `CodeSourceSnapshot` holds `AuthEntry { token, grant }`.
5. `CodeSourceRuntime::authenticate` returns `ProducerGrant`.
6. `authenticate_request` runs before route handlers.
7. `require_scope` maps `PublishedScope` through `ProducerGrant.projects` or returns `scope_forbidden`.
`src/server/mcp.rs` merges `code_source::router`, whose one `route_layer` covers every `/internal/code-source/v1/*` route. A committed-HEAD caller walk and search found no second producer token table.

### 3.2 Git history still opens the checkout
Verified caller path:
1. Collected activation calls `src/server/code_source.rs::stage_git_current_overlay_after_activation`.
2. It acquires `CheckoutAccessKind::GitHistory`.
3. It calls `IndexWriterHandle::stage_git_current_overlay`.
4. The actor handles `IndexWriteOp::StageGitCurrentOverlay` through `run_git_current_overlay`.
5. The actor calls `bbox_corpus_index::index::git_history::index_git_history_for_project`.
6. The indexer calls `bbox_corpus_core::git::commit_log` and `changed_files_for_commit`.
7. It writes commit docs, calls `emit_git_message`, stages `GitHistoryPublication`, and publishes Git edges plus `GitIngestMeta.last_ingested_sha`.
Two `project_files.rs` callers and the actor helper `stage_git_current_edges` reach the same adapter. P3-F consolidates this to one repo walk and `GitOverlaySelector`, but still needs one checkout source. The complete caller walk plus searches for `index_git_history_for_project`, `stage_git_current_overlay`, `commit_log`, and `changed_files_for_commit` establish that no path-free history source exists at committed `HEAD`.

### 3.3 Assumed Phase deliverables
Implementation starts only after:
- P3-D supplies immutable history generations, materializer, and `RepoHistoryRebuildManifestV1`.
- P3-F supplies `GitOverlaySelector`, consolidated history, health, vector lifecycle, and history GC.
- Phase 6 supplies `path-free-rebuild` and its committed-manifest startup gate.
If a landed owner name differs, update the anchor to the landed owner; do not recreate the planned type under another name.

## 4. Scope, deferrals, and predecessor relationship
### 4.1 Scope decision
This slice covers Git history as one typed Git transport.
Rationale:
- `G19` names typed Git-history transport before separate published knowledge.
- The lane needs admitted checkout identity, repository access, a commit namespace, and the credential code collection already uses.

### 4.2 Published knowledge deferral
The later slice exclusively owns accepted knowledge/gap upload, `AcceptedPublicationStore`, pointers, establish/advance/rebind, visibility, and aliases.

### 4.3 Non-goals
- No Git pack, bundle, object database, arbitrary ref, clone, fetch, or corpus-side Git command.
- No producer-selected project id, repo-history id, namespace, attachment id, or catalog epoch.
- No new token file, token table, token format, or auth family.
- No second `RepoHistoryGeneration` constructor or rebuild manifest.
- No entity-ref syntax change.
- No history for refs unreachable from exact observed `HEAD`.
- No render, mutation, refactor, artifact, or tool/transcript transport.
- No cutover while coverage or parity is incomplete.
- No bridge-code deletion before its separate rollback gate.

## 5. Fixed decisions
### GH-FD-1: One slice, one lane, one credential
The history upload lane uses `CodeCollectionProducerConfig`.
Rejected: `git_collection.producers`, because duplicate grants can conflict and violate `G19`.

### GH-FD-2: Factor auth ownership without code-source drift
New `src/server/producer_auth.rs` owns extracted `ProducerGrant`, `AuthEntry`, auth snapshot, bearer verification, scope lookup, and repo-grant derivation.
`CodeSourceRuntime` keeps code store and activation but holds `Arc<ProducerAuthRuntime>`. `code_source::router` and new `git_source::router` use the same middleware and grant.
Rejected: a second router calling private `CodeSourceRuntime::authenticate`, because it ties unrelated lifecycle and prevents one atomic auth candidate.

### GH-FD-3: Derive repo authority from all published members
New plan-defined `RepoTransportGrant` contains producer id, authority scope, repo-history id, primary namespace, and ordered `(project_id, PublishedScope, bbox_root_relpath)` members.
A grant exists only when every `Published` member of one `RepoHistoryId` is assigned to the same producer. One unassigned member or any split assignment blocks transport for the entire repo and reports `repo_history_scope_split`, while valid code grants remain.
This all-members rule is accepted even when it blocks indefinitely. The operator unblocks it by assigning the missing member to the same producer or scope-migrating that member to a distinct recorded repo authority. Sibling-authority widening is never an implicit recovery action.
A blocked repo that has never been covered by a cutover marker is not transport-governed yet. It retains its P3-F attachment-backed history refresh and appears as `blocked_published_never_covered` in observations until a complete grant enters a later cutover ceremony.
`LegacyLocal` members neither authorize nor veto, receive no producer overlay until published, and retain their certified local history adapter.
Rejected: any subproject scope publishing whole-repo data, because that widens authority over siblings.

### GH-FD-4: Full logical snapshot with content-deduplicated transfer
Every history generation is complete per `G11-B`; upload may reuse immutable record blobs. A same-HEAD probe skips the walk, otherwise the producer builds a full HEAD-reachable manifest.
Rejected: cursor deltas, because force-push, first upload, replay, and GC would depend on untrusted predecessor state.

### GH-FD-5: Typed records, never Git packs
Wire facts are commit id, ordered parents, author name/email, message, and changed repo-relative paths. The corpus enforces bounds, path safety, object format, graph closure, and HEAD reachability.
Rejected: Git bundle upload, because corpus-side Git/object-store lifecycle recreates checkout coupling.

### GH-FD-6: One history creation path through the certified amendment mechanic
`bbox-git-source-store` yields `VerifiedGitHistorySourceV1`; the P3-F builder creates the only `RepoHistoryGeneration`.
Slice-local authority does not rewrite the certified caller or selector invariants. In the same GH-C commit that adds typed producer refresh and transport-built overlays:
1. Surgically amend `P3-F` section 10 item 3 to enumerate materializer, live checkout refresh, and typed producer refresh while retaining "with no other code constructing generations."
2. Surgically amend `P3-F` section 10 item 1 to replace the flat `attachment_id` selector source with the `GitOverlaySourceV1` discriminant defined in section 6.5.
3. Surgically amend governing section 11 both where it repeats the exclusive caller enumeration and where it defines `GitOverlaySelector`.
4. Record the combined caller-expansion and selector-source choice as a new `DECISION_LEDGER.md` entry whose number is assigned at implementation time, never assumed by this plan.
5. Review the amendments and implementation together under the existing phase-family plan and implementation review gates.
Constructor, content-addressed creation path, and rebuild manifest remain singular.
Rejected: a second producer-history format dual-read by queries and rebuild, or a slice-local caller expansion without the established amendment record.

### GH-FD-7: Exact overlay matching
Overlay requires same repo-history id, `code.head_commit == history.repo_head`, admitted membership, valid path mapping, verified current history, and a valid typed source arm. `Attachment` requires its validated attachment; `ProducerTransport` requires the accepted source generation and matching `RepoTransportGrant` but no attachment. Otherwise clear the new code generation's overlay and report `lagging`.
`history_transport_current(repo)` is a derived currency predicate, never a durable one-way latch. It is true only while a verified `ProducerTransport` arm is current and every matching input above remains valid. Grant loss, selector replacement, or mismatch makes it false. GH-C suppresses checkout refresh while true; if the repo has never had marker coverage, false re-enables eligible P3-F attachment refresh. Marker-covered repos remain under GH-FD-16 and pending-recutover no-fallback rules.
Rejected: newest-history attachment to any code generation, which creates stale file targets.

### GH-FD-12: Published transport authority never falls back to checkout Git I/O
After GH-G, history for transport-governed `Published` repos evaluates producer state only; bridge paths remain exact.
`LegacyLocal` is not transport-governed. An attached Git `LegacyLocal` project retains P3-F's validated `GitHistory` lease-backed refresh under its local or imported legacy namespace, and this slice does not alter its other certified Phase 5 adapter behavior. Full local-adapter retirement belongs to the later governing section 19 gate.
A never-covered `Published` repo with a blocked grant is likewise not yet transport-governed and retains P3-F attachment refresh. If it was briefly transport-current before coverage, later grant re-block clears the invalid transport arm and resumes that refresh. This is deliberately asymmetric with GH-FD-16: producer removal after marker coverage is an authority decision, so that covered repo becomes `unavailable_no_transport` and never reacquires a checkout lease; incomplete assignment before first coverage is migration-in-progress, so it keeps pre-slice behavior.
Missing transport preserves last-good durable state and reports unavailable.
Rejected: transport-first with silent checkout fallback, which defeats zero-observation proof.

### GH-FD-15: Cutover is an offline, artifact-bound catalog operation
Add plan-defined `ProjectCatalogCommand::GitTransportCutover(GitTransportCutoverArgs)` with exact operator forms:
```text
blackbox project-catalog git-transport-cutover --preflight --report <path> --resolution <path>
blackbox project-catalog git-transport-cutover --apply --report <path> --resolution <path> --configured
blackbox project-catalog git-transport-cutover --verify --configured
```
If a migrated repository's first transport activation committed before the
operator completed checkout parity, the committed activation journal remains
immutable. The operator may instead reproduce that repository's canonical
history generation through the certified checkout row builder and install the
result as a separate offline review artifact:
```text
blackbox project-catalog git-transport-checkout-parity --proof <path> --configured
```
The canonical, checksummed proof must enumerate exactly every current
non-equal typed-history row and bind the repo-history id, current source
generation and HEAD, P3 generation, commit-document count and commitment, and
vector-input count and commitment. Acceptance refuses prepared journals,
extra or missing rows, stale source state, noncanonical encoding, and proof
paths inside managed state roots. Preflight and apply recheck the installed
proof against live state. A later source or P3 change therefore invalidates
the row. This proof closes parity evidence only; it neither rewrites an
activation journal nor changes transport authority.
All forms accept D-021 `ConfigArgs` with unchanged precedence: `--config`, then explicit `--state-dir` and `--projects-path` overrides. Preflight is read-only, resolves configured state through that precedence, and emits the GH-F coverage report, a canonical empty-or-explicit resolution artifact, plus predicted `GitTransportCutoverMarkerV1`. The verb returns the D-020 versioned result envelope with snake_case `command` values `project_catalog_git_transport_cutover_preflight`, `project_catalog_git_transport_cutover_apply`, and `project_catalog_git_transport_cutover_verify`.
Apply reuses `open_admin_store`, requires the configured-state opt-in, rechecks catalog epoch, grant commitments, generations, journals, parity commitments, and report/resolution hashes, then atomically writes a checksummed auxiliary marker beside the catalog store. The marker is not a `CatalogSnapshotV2` field and does not advance catalog epoch. Verify proves marker and live state agree before daemon restart.
`FreshV2` catalog stores with configured producers perform the same preflight, apply, verify, and restart ceremony. Their legacy-history parity set is vacuous, not exempt; uniform authority activation prevents a second startup mode.
Rejected: a live MCP authority flip, because shared-service state, stale artifacts, and rollback ownership need the Phase 6 offline-admin discipline.

### GH-FD-16: Code cutback does not reactivate Git checkout adapters
Removing a producer may cut code back to a validated local source through the existing source state machine. History retains last-good durable generations and becomes `unavailable_no_transport`; it does not acquire a Git lease. Reassignment resumes transport.
Rejected: coupling code cutback to history fallback, because it violates GH-FD-12 and makes zero observations unstable.

## 6. Typed contract and storage model
All names below are new plan-defined identifiers unless stated as landed.

### 6.1 Dependency-clean crates
Create `crates/bbox-git-source` for wire structs, canonical encoders, commitments, validators, graph closure, and typed errors.
Allowed internal dependencies: `bbox-corpus-core`, `serde`, `sha2`, `thiserror`, `hex`, and small leaf utilities.
Forbidden: filesystem/Git execution, Axum, Reqwest, Tantivy, indexing, vectors, edge index, root package, harness, V8.
Create `crates/bbox-git-source-store` for sessions, blobs, immutable generations, journals, receipts, verification, retention, and GC.
Add `scripts/acceptance-git-source-deps.sh`.

### 6.2 History wire
```text
GitObjectFormatV1 = Sha1 | Sha256
GitHistoryDescriptorV1 {
  schema_version, scope, repo_head, object_format,
  manifest_sha256, commit_count, fragment_count, logical_bytes
}
GitHistoryManifestEntryV1 {
  commit_oid, fragment_index, encoded_bytes, content_sha256
}
GitHistoryCommitHeaderV1 {
  parent_oids, author_name, author_email, message
}
GitHistoryCommitFragmentV1 {
  commit_oid, fragment_index, fragment_count,
  header, changed_paths
}
```
Validation:
1. All object ids are consistently 40 or 64 lowercase hex.
2. Manifest order is `(commit_oid, fragment_index)`.
3. Fragments are contiguous; only fragment zero has the header.
4. Parent order is preserved.
5. Changed paths are normalized, sorted, deduplicated, repo-relative, and traversal-free.
6. Reconstructed set contains HEAD, every parent exists unless root, and every included commit is HEAD-reachable.
7. Shallow producer repos and missing-parent graphs reject.
8. Counts, bytes, hashes, and commitments match.
9. Server derives generation id from producer, repo-history id, namespace, HEAD, schema, and manifest.
10. Fragmentation occurs only at changed-path boundaries; oversized indivisible headers reject without changing last-good.

### 6.3 History routes and states
```text
POST /internal/code-source/v1/git-history/probe
POST /internal/code-source/v1/git-history/uploads
PUT  /internal/code-source/v1/git-history/uploads/{id}/manifest/{page}
POST /internal/code-source/v1/git-history/uploads/{id}/manifest/complete
GET  /internal/code-source/v1/git-history/uploads/{id}/missing?cursor=...
PUT  /internal/code-source/v1/git-history/uploads/{id}/records/{sha256}
POST /internal/code-source/v1/git-history/uploads/{id}/finalize
GET  /internal/code-source/v1/git-history/generations/{generation}/status
```
States: `receiving_manifest`, `missing_records`, `ready`, `materializing`, `publishing`, `active`, `superseded`, `failed`.
Probe carries scope and observed HEAD. It returns current only for same verified HEAD and schema.
Manifest pages are bounded, contiguous, digest-bound, and replay-safe. Record install streams and hashes. Finalize reconstructs and validates the graph before `ready`.
Begin resumes the open upload for an identical descriptor and reports its persisted `state`; a response without `state` decodes as `receiving_manifest`. In `receiving_manifest` the producer replays every page from zero so stored page digests prove identical page boundaries; in `missing_records` it skips manifest PUTs and calls the idempotent complete. Begin state is an observation, not a lease: a typed `invalid_upload_state` from a concurrent same-descriptor client triggers a bounded re-probe and re-begin, and exhaustion falls back to lane backoff. Content conflicts, authority failures, and malformed data are never retried as state moves.
Finalize is idempotent per generation id. An existing generation with the same identity, descriptor, and exact manifest is reused with its original creation time and lifecycle state; any mismatch fails closed. Each newly accepted upload attempt receives a durable per-repository acceptance sequence, checkpointed in its upload record before `current-ready.json` moves and carried in that pointer with the upload id. The pointer advances only to a newer sequence; an equal sequence must name the same upload and source. Only the winning acceptance reopens a `superseded` or `failed` source to `ready` and clears its diagnostic, durably before the pointer names it, so an interrupted acceptance never leaves a probe-current source that cannot activate; in-flight and `active` sources keep their state. Replaying a completed upload is a no-op, and an older checkpoint may finish its upload without rewinding a newer pointer, while a fresh upload of a retained generation is a new acceptance.
Additive fields keep old and new components decodable in both directions, but state-aware resume and ordered acceptance need the upgraded daemon and collector together: an older collector ignores `state`, and an older daemon omits it.

### 6.4 Verified source handoff
```text
VerifiedGitHistorySourceV1 {
  source_generation_id, producer_id, authority_scope,
  repo_history_id, primary_namespace, repo_head,
  ordered_commits, manifest_sha256, source_evidence
}
```
`ordered_commits` is a streaming reader. P3-F produces the same canonical commit docs, vectors, source evidence, and generation commitment as an equal checkout walk.
The source stays pinned until P3 generation verification, catalog advance, commit/vector publication, overlay publication or degradation, and committed activation journal.

### 6.5 Overlay source identity
The GH-C amendment replaces the certified selector's flat `attachment_id` with a typed source discriminant:
```text
GitOverlaySourceV1 =
  Attachment { attachment_id }
  | ProducerTransport { producer_id, source_generation_id }

GitOverlaySelector {
  project_id, code_generation, repo_history_generation,
  source: GitOverlaySourceV1,
  repo_head, commit_namespace, overlay_generation
}
```
Existing local overlays normalize to `Attachment`; transport-built overlays use `ProducerTransport` and cannot carry an attachment sentinel. A bounded migration reader accepts the old flat `attachment_id` form only to rewrite it under the manifest coordinator; new writers emit only the discriminated form.
Read-view matching applies GH-FD-7 to the selected arm. Attachment detach invalidates only an `Attachment` arm. A `ProducerTransport` arm remains valid across attachment changes while its accepted source generation, P3 generation, code generation, and repo grant still match.
The selector arm does not alter P3 history-GC ownership: active or retained overlays and pinned read views root the referenced `RepoHistoryGeneration`; `bbox-git-source-store` separately roots source evidence referenced by retained P3 generations. Project retirement drops only its overlay reference. A source swap never frees a generation still referenced by a sibling, retained overlay, read view, in-flight build, or rebuild manifest.
Rejected: `attachment_id: Option<_>`, because `None` fails to name transport evidence and permits invalid source combinations.

### 6.6 Store layout and GC
```text
git-sources/
  records/sha256/<first-two>/<hash>
  uploads/<producer-hash>/<upload-id>/...
  repos/<repo-history-id>/history/<source-generation-id>/...
  repos/<repo-history-id>/history/current-ready.json
  repos/<repo-history-id>/history/acceptance-sequence.json
  generation-index/...
  activations/<repo-history-id>.json
```
Private no-follow directories; same-filesystem temp, file fsync, atomic rename, parent fsync; checksummed versioned journal replacement.
The root is sibling to lexical index and P3 history generations, so schema replacement cannot delete it.
History roots: open uploads, ready/in-flight/active/retained source generations, activation journals, and source evidence referenced by retained P3 generations.
P3 GC remains sole authority for `RepoHistoryGeneration` and vectors.
History acceptance format: upload records and the history ready pointer carry optional acceptance fields, and `acceptance-sequence.json` holds the next per-repository sequence. Records written before ordered acceptance decode as legacy state. A legacy pointer is a baseline below every new acceptance; a completed legacy upload stays a no-op and never gains an acceptance; an unfinished legacy upload receives its first acceptance at its first verified finalize. Allocation takes the maximum of the durable counter, the pointer's sequence, and retained upload checkpoints, so a missing or lagging counter is recovered rather than reset, and a restart between counter and checkpoint leaves only an unused gap. Malformed acceptance fields fail closed. The upgrade is forward-only: a daemon without ordered acceptance cannot decode records that carry these fields.

## 7. Runtime, transaction, lock, and recovery mechanics
### 7.1 Producer schedule
Extend `bbox-code-collector`; do not create a second checkout daemon.
New additive collector project flag `git_history` enables the lane per project.
Loop: validate roots/scopes, publish code, group by Git common dir, obtain contract, upload history if HEAD changed, report bounded lane status.
Code activation never waits for history. Lane backoff is independent.

### 7.2 Repo grouping and monorepos
Local grouping is scheduling only; server `RepoTransportGrant` is authority.
Reject nonowning producer, unsafe repo path, and concurrent producer for one repo-history id.
One history generation emits commits once; path fan-out uses each member `bbox_root_relpath`.

### 7.3 History publication transaction
1. Verify the immutable source generation and prepare canonical rows through the sole P3-F builder off-lock, deriving the exact future generation id and manifest hash without publishing it.
2. Write `HistoryActivationJournalV1::Prepared` with the verified source evidence, catalog epoch, repo id, prior/new P3 generations, code selectors, and planned overlays.
3. Publish the prepared P3 generation, re-open it, and verify its bound id, manifest hash, document commitment, and vector-input commitment.
4. Atomically advance the journal to `GenerationVerified`.
5. Recheck catalog epoch, repo authority, the repository's `current-ready` source pointer, and code selectors, then advance `RepoHistoryRecord.materialization` through the regular catalog CAS. Every later bounded plan recheck repeats the source-pointer proof; an older activation loses to a newer accepted source while the previously selected Active view remains last-good until its successor commits.
6. Record `MaterializationAdvanced` with the resulting catalog epoch. If the catalog already names the exact planned generation after an action-ahead crash, prove it and checkpoint instead of issuing another CAS; if it names any other generation, mark the attempt `Superseded`.
7. Build the exact matching project edges off-lock and publish the exact `(repo_id, doc_type=commit)` lane plus snapshot receipts through the writer actor; the edge-project and receipt-project key sets must be identical, and repository code documents sharing `repo_id` are outside that replacement.
8. Query that authoritative commit lane, compare every stored commit row to the retained P3 generation, verify each durable snapshot-receipt digest, then record `CommitViewPublished` with the commitments.
9. Recheck catalog authority and code selectors, then swap or clear all typed overlay selectors in one manifest transaction.
10. Verify the selected overlays and record `OverlaysPublished`.
11. Atomically record `Committed`, mark the source active, and republish `CodeReadView`.
Journal states are plan-defined and monotonic: `Prepared`, `GenerationVerified`, `MaterializationAdvanced`, `CommitViewPublished`, `OverlaysPublished`, `Committed`, or terminal `Superseded`.
No lock spans source traversal or index publication. Catalog and manifest critical sections are short and do not nest source reads or index commit.

### 7.4 History crash recovery
The durable state is a progress lower bound, not the sole discriminator. Recovery performs this authoritative probe sequence for every nonterminal journal, and startup re-proves only producer sources actually selected by the first read view rather than scanning the global sidecar estate:
1. Verify the exact planned `RepoHistoryGeneration` exists and matches its bound hash.
2. Read `RepoHistoryRecord.materialization` and compare it to the exact planned generation id.
3. Query the exact primary `(repo_id, doc_type=commit)` lane and compare the complete stored row set to the retained generation; compare the retained document/vector-input commitments to the journal.
4. Read every planned `GitOverlaySelector` and the durable finalized snapshot-receipt digest in project-id order.
Recovery arms:
- No journal: no work.
- Planned generation absent: resume builder from `Prepared`.
- Generation present but materialization still names the prior generation: resume staging and the catalog CAS.
- Materialization names a different new generation: atomically mark `Superseded`; never overwrite it.
- Exact materialization advanced but commit-view probe mismatches: re-emit before bind.
- Commit view and durable snapshot receipts match but selectors are not yet swapped: reuse those publications and perform only the manifest transaction. A missing or corrupt durable publication is re-emitted exactly; a valid one is never rebuilt merely because its next journal checkpoint was interrupted.
- All probes match but journal is not `Committed`: advance missing checkpoints and commit.
- `Committed`: re-run all probes before exposure; mismatch fails history capability closed to last-good.
Exact-generation comparisons make each probe monotone under the catalog CAS and distinguish crashes between an external action and its next journal checkpoint. Recovery reuses P3-D/P3-F manifests. `HistoryActivationJournalV1` orchestrates intake only and is not a replacement rebuild manifest.

### 7.5 Lock order
1. Process-lifetime migration lock for offline commands.
2. Catalog store/transaction lock.
3. The process-local `bbox_edge_sidecar::snapshot::MANIFEST_COORDINATOR` `OnceLock<Mutex<()>>`, acquired only through `bbox_edge_sidecar::snapshot::with_manifest_coordinator`.
4. Writer actor ownership, never caller-held mutex.
5. Git-source upload/GC lock.
6. Producer-local Git common-directory lock.
No path needs all layers: producer Git locks never enter daemon; upload locks release before materialization; catalog locks release before index commit; GC uses pinned references; auth reads are short.

### 7.6 Reload and security
Reload builds one replacement auth table, code assignments, repo grants, and limits. Only a valid auth table swaps; repo split installs blocked health without breaking valid code grants.
Removing producer revokes future requests. Accepted generations remain; in-flight upload cannot finalize after grant recheck.
Bearer auth precedes parse; scope precedes lookup/write; ids are producer-bound; server derives catalog ids; paths validate at every boundary; object ids are consistent lower hex; secrets and content do not enter ids, logs, or metric labels.

### 7.7 Cutover transaction
`RepoHistoryRecord` gains additive `#[serde(default)] membership_generation: u64` in `bbox-corpus-core::project_catalog`. Its durable home is each record in `CatalogSnapshotV2.repo_histories`; old records decode as zero and new writers emit the field.
`ProjectCatalogStore::transact` in `crates/bbox-indexing/src/project_catalog_store.rs` is the sole bump authority. It snapshots the ordered pre/post membership projection for every `RepoHistoryId`: every referencing project's `(project_id, ProjectScope, repo_history_id)`, where `PublishedScope` carries repo authority and `bbox_root_relpath`. After the transaction closure returns, but before candidate validation/publication, the store requires existing records to retain their pre-transaction `membership_generation` and new records to enter at zero, computes changed source and target repo ids, and checked-increments each surviving affected `RepoHistoryRecord` exactly once. A new record with members therefore publishes at one; a removed record is simply absent. This automatically covers `promote_project`, `retire_project`, and both attached and operator-attested scope migration because they all commit through `ProjectCatalogStore::transact`; routine materialization, attachment, alias, and assignment-config changes do not alter the projection. Overflow refuses before publish with plan-defined `error.project_catalog_membership_generation_overflow`.
`GitTransportCutoverMarkerV1` binds report hash, resolution hash, predecessor catalog epoch, an apply-time aggregate producer grant hash, per-repo coverage rows containing the `RepoTransportGrant` commitment, `membership_generation`, member set, accepted history source/P3 generation pair, history parity commitment, and per-capability observation baselines, a zero-prepared-journal proof, and apply timestamp. The V1 row shape also keeps per-member provenance maps and a `ProvenanceNoteIo` capability baseline for decode compatibility; preflight writes the maps empty, and a row without `members` names its members by their keys.
Apply acquires the offline admin store and store mutation lock, rechecks every bound input, fsyncs and atomically replaces the auxiliary marker, verifies it, and emits a receipt. A crash before rename leaves no marker; a crash after rename is completed by `--verify`.
Startup revalidates the marker checksum/schema, that this is the atomically selected current artifact rather than a superseded predecessor, and each covered repo independently. Row validity requires equality of both the current member-assignment commitment and `RepoHistoryRecord.membership_generation`. The assignment commitment covers repo-history id, assigned producer id, and ordered `(project_id, PublishedScope, bbox_root_relpath)` assignments. A token rotation, limit change, primary-namespace change, or grant change for another repo does not alter the subject repo's row; ordinary runtime matching still rejects any accepted generation that no longer matches current namespace or authority.
The predecessor catalog epoch, aggregate grant hash, exact accepted generations, zero-prepared-journal proof, parity commitments, report hash, resolution hash, and apply timestamp are apply-time evidence only. Their current-state equality is not a startup requirement. Generation presence and integrity remain governed by ordinary history recovery and the P6-C `Ready`/manifest rules, not by marker epoch equality.
Routine `transact` epoch advances do not stale the marker. Explicitly tolerated non-authority mutations are alias/display-name changes, attachment add/detach/status/default changes, code activation, history `Ready` advancement, retained-generation and GC bookkeeping, and changes to repos outside the marker. A newly added transport repo remains unauthorized until a newer marker covers it, but does not invalidate covered repos.
A covered repo's transport coverage is current only while both its own assignment commitment and membership-generation watermark equal its marker row. Other covered repos remain authoritative, and the marker artifact remains valid for their rows. Exact restoration of a removed producer assignment without a catalog membership change restores assignment equality while the watermark remains unchanged, so that repo's original row becomes current again under GH-FD-16. Any committed member retire, promotion, or scope migration advances the watermark and permanently invalidates the old row until re-cutover, even if a later symmetric round-trip restores byte-identical assignments. A newer marker artifact carries forward unchanged valid rows and replaces only affected repo rows. Absent or corrupt artifacts fail separately. For a valid artifact, strict catalog startup refuses transport authority only for affected rows while bridge rollback remains available.

### 7.8 Checkout-observation taxonomy
The G19 handoff and exit fixture classify every target lease observation by repo, capability, and lifecycle window:
```text
overlap_window
history_transport_current_pre_cutover
transport_covered_post_boundary
blocked_published_never_covered
legacy_local_local_project
legacy_local_legacy_namespace
covered_producer_removed
covered_blocked_pending_recutover
coverage_stale_pending_recutover
```
The taxonomy is evaluated per `(repo_history_id, capability)` so one repo has exactly one row per applicable capability. `overlap_window` begins at the earlier of GH-F parity-work start or the first observation snapshot, folding fixture setup and any pre-GH-F lease evidence into the same expected baseline. Its pre-coverage `GitHistory` arm is predicate-driven rather than event-latched: whenever `history_transport_current(repo)` is false, attachment-refresh deltas remain expected `overlap_window` evidence regardless of any prior transport publication; whenever the predicate is true, the capability moves to `history_transport_current_pre_cutover` and requires zero deltas. The report stores baselines at predicate transitions and evaluates deltas only while each row is active.
`history_transport_current_pre_cutover` applies only to Git history while the GH-FD-7 currency predicate is true before marker coverage. The row requires zero `GitHistory` lease deltas but makes no G19 authority claim. The marker independently governs entry into the `Published` transport portion of G19.
`blocked_published_never_covered` names a `Published` repo whose complete `RepoTransportGrant` does not exist because a member is unassigned or split. Its P3-F attachment `GitHistory` rows remain expected and named until a later marker covers it. If it previously reached `history_transport_current_pre_cutover`, grant re-block clears the invalid transport overlay through GH-FD-7, the currency predicate becomes false, and attachment refresh resumes. It gains no producer overlay while blocked.
The two `legacy_local_*` rows distinguish v2-created `LocalProject(ProjectId)` from migrated `LegacyNamespace(CommitNamespace)` history authority. Both keep lease-backed refresh and remain project-local.
`covered_producer_removed` is the specific temporary state where the repo's producer assignment was explicitly removed without a catalog membership mutation. It records no target lease, retains last-good state, and reports `unavailable_no_transport` under GH-FD-16. Exact assignment restoration returns it to its prior covered row without requiring a new marker; unrelated covered rows never change.
`covered_blocked_pending_recutover` is the specific state where a committed membership addition blocks the current all-members grant, notably when promotion adds an unassigned Published member to a covered repo. The promotion also advances membership generation, but this specific blocked reason takes precedence over generic stale-pending classification. It retains last-good state but freezes history for the whole repo with no checkout fallback until assignment plus newer-marker coverage. The minimal-window operator runbook is: promote, assign the new member to the same producer, then run the GH-F/G re-cutover ceremony immediately; the freeze window is operator-bounded.
`coverage_stale_pending_recutover` applies when either the current assignment commitment or membership-generation watermark differs from the row. It retains last-good state, acquires no checkout fallback, and exposes no transport authority while invalid. If the mismatch is config-only, the watermark is unchanged and exact assignment restoration before any catalog membership mutation returns the original row current under section 7.7 with no newer marker. Once membership generation advances, equality can never return for that row, even after a symmetric scope-migration round-trip; a newer marker covering every changed repo is required.
Transition precedence is closed: `LegacyLocal` authority selects one `legacy_local_*` row; a Published repo with no prior marker selects `blocked_published_never_covered` when grant-blocked; explicit whole-repo producer removal selects `covered_producer_removed`; a committed membership addition that blocks the all-members grant selects `covered_blocked_pending_recutover`; any other assignment or watermark mismatch selects `coverage_stale_pending_recutover`; otherwise an uncovered Git-history capability selects `history_transport_current_pre_cutover` while the currency predicate is true and `overlap_window` whenever it is false, while covered post-boundary capabilities select `transport_covered_post_boundary`.
Any observation outside its active category fails the gate. `GitHistory` deltas fail while `history_transport_current_pre_cutover` or `transport_covered_post_boundary` is active, but are expected `overlap_window` evidence whenever the currency predicate is false before marker coverage. Post-coverage behavior remains zero-delta regardless of currency loss.

## 8. Milestone spine
Every milestone is independently committable and cluster-verifiable. Runtime changes get isolated bootsmokes. Bridge parity stays green except the explicit parity changes below.

### GH-A: Shared auth extraction and wire leaf
Status: implemented 2026-08-08.
Ownership: new `src/server/producer_auth.rs`, new `crates/bbox-git-source`, `src/server/code_source.rs`, `src/server/state.rs`, `src/server/mcp.rs`, `crates/bbox-config/src/config.rs`, dependency script.
Dependencies: `C4.1`, `C4.3`, `C5`, `G12-A`, `G16-A`.
Mechanics:
1. Move auth candidate, bearer verify, and scope lookup to `ProducerAuthRuntime`.
2. Preserve existing `ProducerGrant`, route, response, reload, and error behavior.
3. Derive `RepoTransportGrant`.
4. Add transport enable/limits under `[code_collection]`, no new credentials.
5. Land wire types, hashing, validation, and errors.
6. Add disabled/contract-only `git_source::router`.
Verification: auth goldens, duplicate/missing/split matrices, codec/adversarial tests, dependency ceiling, existing code collector bootsmoke.
Gate: pinned format, focused workspace nextest, concurrency lint, cluster verify.

### GH-B: Durable history intake and collector capture
Status: implemented 2026-08-08. Maintenance runs off startup and request paths; the isolated FreshV2 rehearsal reached durable `ready` and a second exact-HEAD collector run reused the probe result without another upload.
Ownership: new `bbox-git-source-store`, new `src/server/git_source.rs`, `bbox-code-collector`, packaging/docs, maintenance.
Dependencies: GH-A, `C6.2`, `C8`, `G11-B`.
Mechanics:
1. Add probe/upload/page/missing/record/finalize/status.
2. Persist sessions and immutable records.
3. Add HEAD probe and complete reachable scan.
4. Deterministically fragment changed paths.
5. Reject shallow repositories.
6. Revalidate local source before upload.
7. Add independent history backoff/status.
8. Land verification and GC roots.
9. Stop at `ready`.
Verification: SHA-1/SHA-256, linear/merge/root/rename/delete/large fixtures, graph/path/fragment/hash refusals, expiry/restart/cache/probe, live ready bootsmoke.
Gate: focused tests, dependency acceptance, isolated smoke, cluster verify.

### GH-C: History activation and remote overlays
Status: implemented 2026-08-08.
Ownership: P3-F builder, `writer_actor.rs`, history orchestration, edge sidecar, `code_source.rs`, doctor/GC.
Dependencies: GH-B, `P3-D`, `P3-E`, `P3-F`, `P6-R`, `G11-A`, `G11-C`, `G11-X`.
Mechanics:
1. In the same commit that adds the producer-refresh caller and transport selector arm, surgically amend `P3-F` section 10 items 1 and 3 plus governing section 11's selector and repeated caller enumeration, add the combined implementation-time Decision Ledger entry without assuming its number, and review the amendment with the milestone.
2. Adapt verified source to the single P3-F builder.
3. Add the monotonic activation journal and authoritative recovery probes.
4. Materialize commit docs/vectors once per repo.
5. CAS history materialization.
6. Build matching `ProducerTransport` project overlays without an attachment sentinel.
7. Swap or clear typed source arms atomically under the manifest coordinator.
8. Record transport source health.
9. Add startup recovery/retention.
10. Derive catalog checkout refresh from `history_transport_current(repo)`: suppress refresh while a verified `ProducerTransport` arm is current, even before marker coverage; if a never-covered repo loses grant completeness or selector validity, GH-FD-7 clears the arm and eligible P3-F attachment refresh resumes. Marker coverage separately controls the G19 authority claim, and after coverage the no-fallback rules override resumption.
Parity: equal facts yield identical commit docs, vectors, parent/file edges; selector source identity and remote overlay availability are the only intended overlay changes.
Verification: checkout-vs-typed golden, monorepo fan-out, head/force-push/detach/retire/GC matrices, attachment-to-transport source swap, publication-before-marker and marker-before-publication orders, complete grant then pre-marker publication then member unassignment/split then overlay clear and attachment-refresh resumption, publication then routine code-ahead mismatch then refresh resumption then matching transport republish, mismatch clear, detach independence, sibling retention, every journal-checkpoint/probe crash boundary, `DenyCheckoutAccess` search/graph bootsmoke.
Gate: focused history/writer/edge/vector/doctor tests, cluster verify, strict catalog smoke.

### GH-F: Overlap migration and parity proof
Status: implemented 2026-08-08. The checked-in FreshV2 rehearsal drives code,
history, P3 activation, producer overlays, daemon restart, and offline clean
preflight.
Ownership: `bbox-corpus-core` catalog types, `ProjectCatalogStore::transact`, migration/reporting in `bbox-indexing`, doctor, operations/smoke fixtures.
Dependencies: GH-C and Phase 6.
Mechanics:
1. Land `RepoHistoryRecord.membership_generation` and the automatic `ProjectCatalogStore::transact` projection-diff bump before any preflight report is accepted.
2. Implement `git-transport-cutover --preflight --report <path> --resolution <path>` with the D-020 envelope, snake_case command value, and D-021 `ConfigArgs` precedence.
3. Inventory heads, commit/vector commitments, overlays, grants, counters, membership generations, and the per-repo, per-capability `overlap_window` baseline.
4. Require typed history parity with checkout generation at same HEAD.
   A committed activation that predates checkout proof may satisfy this only
   through the separately installed GH-FD-15 external proof, whose exact
   source and P3 commitments are rechecked during preflight and apply.
5. Exclude blocked-Published repos from coverage and name them in the report.
6. Require no prepared journal.
7. Emit report and canonical empty-or-explicit resolution artifacts bound to catalog epoch, grants, membership generations, generations, and commitments.
8. Run the identical ceremony for `FreshV2` producer stores with a vacuous legacy parity set.
9. Leave blocked-Published repos uncovered and preserve their attachment refresh; change no authority.
Verification: complete/missing/split/stale/unresolved/mismatch/corrupt/prepared/detached matrices, blocked-Published exclusion plus retained refresh, FreshV2 vacuous-parity coverage, and one full rehearsal.
Gate: migration/doctor tests, cluster full verify, reviewed report fixture.

### GH-G: Strict catalog cutover
Status: implemented 2026-08-08. The offline apply/verify marker closes the
checkout-backed Git-history capability per covered repo, row-scoped runtime
classification retains unrelated authority, reload republishes revocation
before transition dispatch, and re-cutover carries unchanged rows exactly.
Ownership: runtime source selection, `code_source.rs`, docs, observation assertions, operations.
Dependencies: accepted current GH-F receipt.
Mechanics:
1. Implement offline `--apply` and `--verify`; write `GitTransportCutoverMarkerV1`.
2. Validate current-marker identity and each covered repo row independently at startup; stale or blocked rows do not invalidate unrelated covered repos, and apply-time evidence plus routine epoch tolerance remain exact to section 7.7.
3. Remove each covered `Published` repo's `GitHistory` lease path after its first verified `ProducerTransport` overlay publication, a source swap when an `Attachment` arm existed; preserve never-covered blocked-Published refresh and both named `LegacyLocal` refresh shapes.
4. Preserve bridge paths/assets.
5. Preserve last-good on unavailable transport, never fallback.
6. Assert the section 7.8 taxonomy across startup, incremental, rebuild, upload, detach, reload, failure, and recovery: overlap deltas are expected evidence; both transport-current categories have zero post-boundary deltas; blocked-Published and both `LegacyLocal` classes have only their named history rows; removed, blocked-pending, and stale-pending covered rows have no fallback; no observation crosses repo or category.
7. Report the `G14-H` transport row as path-free only for `Published` transport repos in their capability-specific post-swap or post-cutover windows, name overlap, transport-current-pre-cutover, blocked-Published, both pending-recutover categories, and both surviving `LegacyLocal` rows, and leave the governing section 19 all-adapter gate open.
Verification: attached Published overlap refresh and selector swap in both marker orders, complete grant then pre-marker publication then grant re-block and attachment-refresh resumption, publication then routine code-ahead mismatch then refresh resumption and transport republish, empty-attachment covered boot, complete remote flow, blocked-Published refresh, both `LegacyLocal` authority refreshes, covered producer removal with unrelated-repo survival, config-only assignment restoration without re-cutover, member retire plus newer-marker recovery, LegacyLocal promotion plus immediate assignment/re-cutover recovery, scope-migrate out/in with source/target row recovery, symmetric scope-migration round-trip remaining stale until newer-marker recovery, marker epoch-tolerance and own-row-staleness matrices, bridge rollback, full cluster closeout, fresh adversarial review.
Verification transition rows:
| Operation | Required category transition | Recovery and isolation proof |
|---|---|---|
| Pre-coverage transport currency loss | `history_transport_current_pre_cutover` to `blocked_published_never_covered` after member unassignment or split clears the transport arm | Attachment refresh resumes, the new local overlay is named expected evidence, and no marker or G19 authority is claimed. |
| Pre-coverage code-ahead oscillation | `history_transport_current_pre_cutover` to `overlap_window` when `code.head_commit` no longer matches current history, then back after matching transport publication | P3-F attachment refresh deltas are expected while the predicate is false; zero-delta enforcement resumes only when transport currency returns. |
| Covered member retire | `transport_covered_post_boundary` to `coverage_stale_pending_recutover` after the `G-D12` assignment removal | Exact restoration before retire returns current without a marker; if retire commits, a newer marker carries the reduced member set, the surviving sibling returns current, and unrelated repo rows never change. |
| `LegacyLocal` promotion into a covered repo | Covered row to `covered_blocked_pending_recutover` when the promoted member is unassigned | History freezes with no fallback; promote, assign the member to the same producer, immediately run the GH-F/G re-cutover ceremony for a newer marker row, and prove unrelated repos remain current. |
| Published scope migration out or in | Every already-covered source or target row whose assignment triple or membership generation changed enters `coverage_stale_pending_recutover` | A migration out/back round-trip restores assignment bytes but not the watermark; apply a newer marker covering each changed repo, while all other rows remain authoritative. |
| Exact producer assignment removal/restoration | `transport_covered_post_boundary` to `covered_producer_removed` and back | No checkout fallback, exact restoration reuses the matching row, and sibling covered repos retain authority throughout. |
Gate: full nextest profile, clippy/concurrency via cluster wrapper, isolated end-to-end smoke, exact review pass.

## 9. Typed error and health vocabulary
Existing HTTP codes retained: `unauthorized`, `scope_forbidden`, `service_disabled`, `not_found`, `content_length_required`, and code-source store codes.
New auth/grant HTTP codes:
- `repo_history_scope_split`
New catalog/store error:
- `error.project_catalog_membership_generation_overflow`
New history HTTP codes:
- `git_transport_disabled`
- `repo_history_not_found`
- `history_source_shallow`
- `history_object_format_mismatch`
- `history_manifest_out_of_order`
- `history_fragment_gap`
- `history_record_too_large`
- `history_graph_incomplete`
- `history_unreachable_record`
- `history_commitment_mismatch`
- `history_generation_stale`
- `history_generation_conflict`
- `history_materialization_failed`
- `history_publication_superseded`
History health additively extends P3-F:
```text
current
lagging
unavailable_no_attachment   # bridge
unavailable_no_transport    # catalog
invalid_scope
failed_last_refresh
```
Detail names generation, HEAD, active code heads, producer, success/failure code, and retry without paths.

## 10. Bridge parity contract
Complete intended observable changes and preserved exceptions:
1. New authenticated history internal routes.
2. Additive transport config fields default disabled.
3. Collector can publish history.
4. Remote-only projects can gain commit docs and overlays.
5. History health gains producer evidence and `unavailable_no_transport`.
6. GH-G `Published` transport repos record zero post-boundary Git-history lease deltas and never fall back.
7. Published target-lease rows are judged by the active capability category: pre-cutover history may return to expected `overlap_window` evidence after prior transport publication whenever currency becomes false.
8. Before marker coverage, Git-history lease evidence is predicate-driven: a current transport arm suppresses refresh, while grant re-block or routine head mismatch resumes expected attachment refresh regardless of prior publication.
9. Attached Git `LegacyLocal` projects with either `LocalProject` or `LegacyNamespace` authority retain their `GitHistory` refresh and may record that named lease; this is not transport fallback.
10. `GitOverlaySelector` replaces flat `attachment_id` source identity with `GitOverlaySourceV1`; existing local overlays use `Attachment`, and transport overlays use `ProducerTransport`.
11. Covered committed membership changes advance a durable repo-history watermark and enter named blocked-pending or stale-pending rows; config-only assignment mismatch can recover because the watermark is unchanged, while every committed change requires a newer marker even after a symmetric round-trip.
12. Bridge mode remains exact.
Everything else remains byte-identical: code-source routes, code identity/activation, commit refs/docs/truncation/vectors/edges, published knowledge/gaps, render, mutation, artifacts, tool/transcript behavior.
Any drift outside these twelve entries requires plan amendment and re-review.

## 11. Test and validation plan
### 11.1 Contract and auth
- Round-trip every struct; reject unknown fields.
- Deterministic ids under canonical permutation; every authority/content field affects id.
- Limits at zero, exact cap, cap plus one, overflow.
- Object-id/path/page/cursor/error golden corpora.
- Bearer before parser; forbidden scope creates no state.
- Malicious project id is unknown field.
- Same/split/missing/LegacyLocal repo-grant matrices.
- An unassigned published monorepo member blocks the repo grant until same-producer assignment or scope migration.
- A never-covered blocked-Published fixture keeps attachment history and gains no producer overlay; after assignment and a later cutover it enters the covered zero-delta class.
- `LegacyLocal` fixtures cover both `RepoHistoryAuthority::LocalProject` and `RepoHistoryAuthority::LegacyNamespace`.
- Transition matrix covers pre-marker grant loss and code-ahead oscillation with refresh resumption, config-only exact restoration without re-cutover, assigned-member retire under `G-D12`, LegacyLocal promotion into a covered repo, scope migration out/in plus symmetric round-trip watermark staleness, exact producer removal/restoration, newer-marker recovery, and unaffected covered-repo survival.
- Membership watermark tests: old catalog bytes default to zero; a new member-bearing record starts at one; retire, promotion, and scope migration bump affected surviving records once per transaction; routine materialization and config-only assignment changes do not bump; a scope-migration out/back round-trip advances twice; direct closure edits and overflow refuse before publication.
- Reload retains prior on failure; token removal blocks finalize; cross-producer upload is not found; no secret leakage.

### 11.2 History parity
Capture one repo through P3-F checkout walk and typed source; assert namespace, commit ids, stored fields, truncation, vector id/hash/text, parent edges, file edges, generation count/commitment, and overlay files equal.
Cover linear, merge, root, rename, delete, long message, large path list, SHA-1, SHA-256, force-push, code ahead/history ahead, detach, sibling retire, GC, and monorepo path fan-out.
| Selector test row | Required proof |
|---|---|
| Transport overlay swap, clear, and GC | Old flat attachment input normalizes to `Attachment`; attachment-to-transport swap is atomic; transport-to-mismatch clears; attachment detach does not clear a valid transport arm; project retirement and source-store GC retain generations until every overlay, sibling, read view, build, and rebuild-manifest reference is gone. |

### 11.3 Fault injection
| Fault | Recovery |
|---|---|
| Before history upload id | retry begin |
| After manifest page | same page no-op |
| During record stream | remove temp, hash stays missing |
| After record install | later finalize reuses |
| After source generation before journal | resume discovery or bounded orphan GC |
| Prepared journal before P3 generation | generation probe is absent; resume builder |
| P3 generation before `GenerationVerified` checkpoint | generation hash probe succeeds; write checkpoint, resume staging/CAS |
| Catalog CAS before `MaterializationAdvanced` checkpoint | exact materialization-pointer probe succeeds; write checkpoint, re-emit commit view |
| Commit view before `CommitViewPublished` checkpoint | selector/commitment probe succeeds; write checkpoint, build overlays |
| Overlay publish before `OverlaysPublished` checkpoint | ordered overlay probes succeed; write checkpoint, commit journal |
| GC during activation | pinned objects survive |
| Producer revoked mid-upload | next request denied, no finalize |

### 11.4 Bootsmokes and gates
Use throwaway state, isolated ports, temporary repos, no shared-service restart.
Smokes: existing code collector after GH-A; history ready after GH-B; remote history/search/overlay after GH-C; migrated and FreshV2 overlap reports after GH-F; attached Published overlap-to-transport swap, empty-attachment covered boot, blocked-Published refresh, both `LegacyLocal` authority shapes, covered producer removal, and bridge rollback after GH-G.
Mid-cycle plan commands: `scripts/fmt.sh --check`, targeted `cargo nextest run --workspace` expressions, three dependency acceptance scripts, and `scripts/lint-concurrency.sh`.
Closeout: cluster verify wrapper, full workspace nextest profile, clippy/concurrency, isolated daemon plus collector rehearsal, and fresh adversarial implementation review resumed to exact pass.

## 12. Exit-gate proof
Fixture: covered repo A is the attached Published swap candidate; covered repo B has two published collected members sharing one repo history; covered repo C is detached and remote-only; one never-covered blocked-Published repo has an assigned attached member and an unassigned sibling; one v2-created attached Git `LegacyLocal` project has `LocalProject` authority; one migrated attached Git `LegacyLocal` project has `LegacyNamespace` authority; matching/lagging heads, retained prior generations, and windowed observation counters are present. The checkout policy permits only category-valid leases before each per-capability boundary and denies every target lease where section 7.8 requires zero.
Sequence:
1. Start strict catalog without a Git-transport marker and snapshot `overlap_window` counters.
2. Lease-refresh the attached Published swap candidate, producing an `Attachment` overlay; also refresh the blocked-Published repo and both `LegacyLocal` authority fixtures.
3. Upload and activate code and typed history for coverable repos.
4. Run GH-F preflight and GH-G apply/verify, covering every eligible repo while leaving the blocked-Published repo uncovered.
5. Publish the swap candidate's verified `ProducerTransport` overlay and record its post-swap history baseline.
6. Exercise commit lexical/hybrid search plus `COMMIT_PARENT` and `COMMIT_TOUCHED_FILE` expansion.
7. Activate a mismatching code HEAD, observe overlay clear, then upload matching history and restore the transport overlay.
8. Run P6-R full path-free rebuild and restart recovery.
9. Detach the swap candidate's attachment and prove its transport overlay remains selected.
10. For repo B, remove the retiring member's assignment as `G-D12` requires, assert B enters `coverage_stale_pending_recutover` while A and C remain current, retire the member, run source/history GC, prove the surviving B sibling and retained references survive, then run the GH-F preflight plus GH-G apply/verify ceremony to install a newer marker carrying B's replacement row and prove B returns to transport-current.
11. Remove repo A's producer assignment and prove A enters `covered_producer_removed` with `unavailable_no_transport` and no lease fallback while B and C retain transport authority; restore A's exact assignment and prove A resumes without changing B or C.
12. Refresh the still-blocked Published repo and both `LegacyLocal` fixtures.
13. Read the windowed observation report and exercise bridge rollback.
Enumerated overlay assertions:
1. Swap: the published `Attachment` selector becomes `ProducerTransport` atomically with no mixed manifest.
2. Mismatch clear: a new code HEAD cannot retain the old transport overlay.
3. Detach independence: detaching the former source attachment cannot clear a valid transport arm.
4. Retire/GC: no source or P3 generation is freed while a sibling, retained overlay, read view, in-flight build, or rebuild manifest references it.
Observation assertions: pre-GH-F and GH-F overlap counters share one expected baseline; every covered repo has zero target-lease delta after its cutover and first transport-overlay publication boundaries regardless of order; the never-covered blocked-Published repo has only named P3-F history rows and no producer overlay; both `LegacyLocal` authority shapes have only their named project-local history rows; removed, blocked-pending, and stale-pending covered repos have no lease fallback; repo-local membership or producer changes do not alter unrelated covered rows. Expected docs/vectors/overlays/edges exist, no host path enters published transport identity, no legacy cursor seeds a producer generation, and bridge rollback serves retained state.
This proves the `Published` Git-history transport part of `G19` in the correct per-repo capability windows while truthfully preserving overlap evidence, never-covered blocked-Published refresh, and both `LegacyLocal` history adapters. It does not claim full off-host mobility before published knowledge and the later all-adapter zero-observation gate.

## 13. Two riskiest calls
### Risk 1: Repo authority from scope credentials
Accepted operational consequence: one unassigned published monorepo member, or one member assigned to another producer, blocks history transport for the whole repo indefinitely. While never covered, the repo retains attachment-backed P3-F history but gains no producer overlay; whenever pre-coverage currency is false, including routine code-ahead mismatch after prior publication, attachment refresh resumes. Once covered, member retire, promotion, or scope migration moves only affected repos into a no-fallback pending-recutover row until a newer marker covers the changed membership. The durable membership watermark makes this strict even for symmetric scope-migration round-trips. Promotion therefore freezes repo history until the operator assigns the new member and immediately re-cuts over; that window is deliberately operator-bounded. This is intentional because granting one subproject's credential authority over sibling history would widen authority silently. The operator must either assign the member to the same producer or scope-migrate it to a distinct recorded repo authority.
### Risk 2: Extending the P3 transaction
Producer transport adds a caller to the single P3 creation path only through the certified same-commit amendment and implementation-time Decision Ledger mechanic. The activation journal must coordinate intake, catalog, writer, overlay, and recovery without becoming a second rebuild manifest.

## 14. Recommended implementation order
1. GH-A, prove no auth drift.
2. GH-B, retain history at `ready`.
3. GH-C, prove typed/checkout history parity.
4. GH-F, complete overlap rehearsal.
5. GH-G only from current accepted receipt.
6. Author published-knowledge transport next.

## 15. Author sign-off summary
1. Milestone spine: GH-A extracts one producer auth runtime and lands the typed wire contract.
2. Milestone spine: GH-B adds resumable complete-history intake and collector capture without publication.
3. Milestone spine: GH-C feeds the P3-F creation path and activates remote commit views and overlays.
4. Milestone spine: GH-F proves history, grant, and observation parity before authority changes.
5. Milestone spine: GH-G applies per-repo transport windows, retains blocked-Published and both LegacyLocal history adapters, and preserves bridge rollback.
6. Riskiest call 1: one unassigned or split published member blocks whole-repo transport until assignment or scope migration.
7. Riskiest call 2: typed history must extend the single P3 creation path without forking recovery.
