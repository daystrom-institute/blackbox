---
title: "Typed Git-history and provenance transport implementation plan"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, git-history, provenance, collector, typed-transport, checkout-leases]
brief: "Replace published catalog-mode checkout Git-history and provenance note I/O with typed, scope-authorized producer transport while preserving the LegacyLocal history adapter, the Phase 3 history substrate, and the landed provenance schema and local writer."
---
# Typed Git-history and provenance transport implementation plan
Date: 2026-07-26
Baseline: branch `beta/blackbox-v2`, committed `HEAD` `09e1ee785380e54c7ebe56b04094a1563f394a99`.
Governing design: [`durable-project-catalog-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-impl.md).
Transport substrate: [`distributed-code-source-collector-impl.md`](../../../../../design/daemon-runtime/distributed-code-source-collector-impl.md).
History dependencies: [`durable-project-catalog-phase3-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase3-impl.md) and [`durable-project-catalog-phase6-impl.md`](../../../../../design/daemon-runtime/durable-project-catalog-phase6-impl.md).
Provenance predecessor: [`checkout-provenance-export-impl.md`](../../../../../design/daemon-runtime/checkout-provenance-export-impl.md).
Decision authority: [`DECISION_LEDGER.md`](../../../../../DECISION_LEDGER.md). This plan uses slice-local decisions `GH-FD-*`; D-043 records GH-C's certified P3-F caller-list and selector-source amendment.

> **Implementation status (2026-08-08).** The caller/owner map was rebaselined
> against current `beta/blackbox-v2`. GH-A is implemented: code and Git lanes
> share one producer credential snapshot, whole-repository grants are derived
> from catalog membership, and `bbox-git-source` owns the dependency-clean
> wire contract. GH-B intake through durable `ready` is implemented: bounded
> authenticated routes, resumable immutable storage, exact-HEAD stable Git
> capture, canonical fragments, shallow refusal, and independent collector
> backoff. GH-B is implemented: background-only upload expiry, generation
> retention, explicit future-materializer roots, grace-delayed CAS reclamation,
> the SHA-1/SHA-256 and graph/path/fragment fixture matrix, and an isolated
> FreshV2 daemon-plus-collector rehearsal all pass. GH-C is implemented:
> verified producer sources use the canonical P3 builder, repo-level commit
> views and exact snapshot receipts publish under a monotonic recovery
> journal, monorepo selectors swap atomically through the typed
> `ProducerTransport` arm, startup re-proves only selected producer views, and
> loss of grant/code/source currency clears those arms before eligible
> pre-marker attachment refresh resumes. The strict remote-only smoke covers
> every action-ahead crash point, grant loss/restoration, code-ahead mismatch,
> matching republish, force-push replacement, and source retirement. GH-D and
> later milestones remain unimplemented; provenance note I/O therefore still
> uses its existing checkout-backed behavior.

## 1. Required outcome
At this slice's exit gate, proved against strict catalog state after Phases 3 through 6 have landed:
1. A scope-authorized producer publishes one complete reachable Git-history snapshot without the corpus host opening a checkout or invoking Git.
2. The corpus validates the typed snapshot, feeds it through the single Phase 3 history-generation creation path, activates repo-level commit documents once, and builds project overlays for matching active code generations.
3. A producer pulls deterministic provenance export pages, applies them through `bbox_provenance::apply_export_page`, and acknowledges the exact completed generation.
4. A producer uploads one stable provenance notes-ref snapshot; the corpus validates documents and publishes provenance edges through a durable replay journal.
5. V2 notes import from validated `target_ref`; V1 notes are upgraded during overlap and have a bounded corpus-side fallback against the pinned active code view.
6. For each `Published` repo governed by transport authority, strict catalog mode acquires zero `CheckoutAccessKind::ProvenanceNoteIo` leases after cutover. Git-history checkout refresh is suppressed while the repo is transport-current, meaning a verified `ProducerTransport` overlay remains the current selector arm under GH-FD-7. Before marker coverage, any loss of transport currency clears that arm and resumes P3-F attachment refresh; those deltas are expected evidence whenever the predicate is false, regardless of prior transport publication. After marker coverage, no-fallback rules keep `CheckoutAccessKind::GitHistory` at zero. A never-covered `Published` repo blocked by unassigned or split members retains P3-F attachment refresh until it is coverable. Attached Git `LegacyLocal` projects retain validated lease-backed refresh under either `RepoHistoryAuthority::LocalProject` or `RepoHistoryAuthority::LegacyNamespace`; both are outside producer transport authority. Bridge mode remains rollback-compatible.
7. Checkout detach leaves accepted code, commit documents, history health, provenance edges, receipts, and recovery state intact.
8. `bbox_provenance_export_plan` and `bro provenance export` remain as interactive checkout-local mutation paths; neither restores daemon Git I/O.
9. Published knowledge and gaps remain deferred: no `AcceptedPublicationStore`, publisher binding, alias, provisional overlay, or knowledge-write authority changes.
10. The `G19` report proves the Git-history and provenance rows are path-free for `Published` transport repos in their per-repo, per-capability post-swap and post-cutover windows, separately reports expected overlap, never-covered blocked-Published, and both retained `LegacyLocal` authority rows, and truthfully leaves published knowledge plus the final all-adapter zero-observation gate open.

## 2. Fixed cross-document anchors
Each citation quotes its section so renumbering or semantic drift is detectable. Later sections cite anchor labels.
| Anchor | Verified phrase | Binding consequence |
|---|---|---|
| `G10-RV` | Governing section 10.3: "A request pins one catalog epoch, active code selector, vector selector, and edge snapshot." | V1 fallback resolves against one request-pinned corpus view. |
| `G11-A` | Governing section 11: "Git history becomes an attachment-backed immutable overlay with identity:" | Transport overlays require a surgical source-provenance amendment while preserving overlay selection semantics. |
| `G11-B` | Governing section 11: "Every history generation is a complete, self-contained snapshot, never a cursor delta." | Every accepted logical snapshot is complete. |
| `G11-C` | Governing section 11: "Old `COMMIT_TOUCHED_FILE` edges cannot target the new snapshot." | Overlay matching is exact on code generation and repo head. |
| `G11-X` | Governing section 11: "Phase 3's pre-replacement history materializer, reused by the Phase 6 path-free-rebuild subcommand and by the Phase 3 live history refresh, owns the single creation path for `RepoHistoryGeneration` and `RepoHistoryQuarantineGeneration`; no other code constructs those generations." | The producer caller requires a surgical caller-enumeration amendment while preserving one constructor. |
| `G12-A` | Governing section 12.1: "The request carries only scope." | Requests never carry project id, repo-history id, namespace, or attachment id. |
| `G12-B` | Governing section 12.1: "An upload cannot create, rename, attach, select, or delete a project." | Routes resolve existing catalog authority only. |
| `G-D12` | Governing decision 12: "Catalog retire refuses while any configured producer assignment targets the project. The assignment is removed first." | Retiring a covered member changes its repo's assignment commitment before catalog membership changes. |
| `G14-H` | Governing section 14 Git-history row: "no current-file overlay, stale commit docs labeled" | Missing transport degrades history without rolling back code. |
| `G14-P` | Governing section 14 provenance row: "`attachment_required` for Git note I/O" | Replace Git note I/O while keeping plan construction corpus-only. |
| `G14-L` | Governing section 14: "Provenance plan generation stays corpus-only; only legacy Git-note I/O acquires a lease." | Producer pulls a corpus-authored plan and applies locally. |
| `G16-L` | Governing section 16: "No lock is held across filesystem walking, Git, embedding, or index commit." | Producer Git, validation, materialization, publication, and CAS are separate phases. |
| `G16-A` | Governing section 16: "Authentication happens before bounded request parsing, and scope membership is checked before any durable upload mutation, as in the collector design." | Reuse bearer middleware and grants before parse or write. |
| `G19` | Governing section 19: "the same scope-bound producer credential infrastructure for typed Git-history/provenance and published knowledge transports" | This is the coupled Git-history/provenance slice; knowledge is next. |
| `C4.1` | Collector section 4.1: "Wire requests carry only a normalized `PublishedScope`" | Reuse `PublishedScope` as caller-supplied authority. |
| `C4.3` | Collector section 4.3: "Authentication proves the configured producer, not the truth of its bytes." | Validate graph closure, hashes, schemas, paths, and order corpus-side. |
| `C5` | Collector section 5: "`bbox-code-source`, a small leaf crate owning ... versioned wire structs and structured error codes" | Use the same leaf-contract and leaf-store dependency shape. |
| `C6.2` | Collector section 6.2: "The API is resumable and idempotent" | Persist sessions, contiguous pages, hashes, and replay-safe finalize. |
| `C8` | Collector section 8: "Generation manifests are immutable after completion." | Source and note generations never mutate in place. |
| `P3-D` | Phase 3 section 8: "builds and proves the machinery" | Consume the landed history store, materializer, and rebuild manifest. |
| `P3-E` | Phase 3 section 9: "add `relative_path`, `source_uri`, `source_kind` stored fields" | Resolve V1 provenance against path-free active corpus documents. |
| `P3-F` | Phase 3 section 10: "one shared creation path whose only callers are the materializer and the live refresh" | Add producer refresh as a caller, never a second constructor. |
| `P3-RV` | Phase 3 section 4.5: "`CodeReadView` gains `catalog_epoch: u64` (from `ProjectRecordsSnapshot.authority_epoch`) and, in catalog mode, `git_overlays: BTreeMap<String, GitOverlaySelector>` (section 10)." | P3-F owns the pinned read-view substrate used by V1 fallback. |
| `P6-R` | Phase 6 section 3.4: "`path-free-rebuild` subcommand is a thin caller of that creation path; it MUST NOT specify a parallel manifest writer" | Add no parallel rebuild or recovery manifest. |
| `P6-C` | Phase 6 milestone P6-C: "Routine `transact` epoch advances (including P3-F live history refresh) are tolerated." | Cutover startup freshness cannot require equality with the apply-time catalog epoch. |
| `P6-CLI` | Phase 6 section 3.1: "Both new commands produce the D-020 versioned result envelope. The envelope `command` values are snake_case" and preflight resolves configured state through `ConfigArgs` under D-021. | The cutover verb uses the same report/resolution, envelope, naming, and config-precedence contract. |
| `PX-S` | Provenance predecessor section 3.3: "Create a leaf crate named `bbox-provenance`." | Keep the landed shared schema and local writer. |
| `PX-V2` | Provenance predecessor section 3.4: "New exports always emit v2 and always copy `Edge::target` into `target_ref`." | V2 `target_ref` is the normal path-free import case. |
| `PX-P` | Provenance predecessor section 3.5: "Fragmentation and pagination are generation-bound" | Reuse landed generation, fragment, page, and cursor rules. |
| `PX-W` | Provenance predecessor section 4: "`apply_export_page(root, page)` performs all checks before the first write in that page" | Collector calls this function, not a new Git writer. |
| `PX-I` | Provenance predecessor section 9: "Import extraction begins only after a separate reviewed plan answers all of these questions" | This plan closes the authority, transaction, and replay gate. |

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

### 3.3 Provenance export is split but not producer-authorized
Legacy daemon path:
1. `src/tools/graph.rs::bbox_provenance_export`.
2. `acquire_provenance_projects` obtains `ProvenanceNoteIo` write leases.
3. `provenance::export_provenance` builds documents from `EdgeIndex`.
4. `bbox_provenance::append_note_documents_dedup` writes notes.
Interactive local path:
1. `src/tools/graph.rs::bbox_provenance_export_plan`.
2. `BlackboxServer::authoritative_session_checkout` supplies `ResolvedCheckoutScope`.
3. `provenance_plan::export_plan_page` builds `ProvenanceExportPlan`.
4. `crates/bro-cli/src/provenance.rs` pages, restarts on `error.stale_generation`, and calls `apply_export_page`.
The second path proves the boundary but uses MCP session context, not an unattended producer credential.

### 3.4 Provenance import is checkout-derived
Verified caller path:
1. `src/tools/graph.rs::bbox_provenance_import`.
2. `acquire_provenance_projects` obtains `ProvenanceNoteIo` read leases.
3. `prepare_provenance_import` calls `git::list_notes` and `git::show_note`.
4. Target resolution branches by note schema: V2 calls `validated_target_for_project`; V1 calls `LegacyTargetResolver`.
5. The V1 production branch reaches `bbox_indexing::index::resolve_current_project_chunk_entity`.
6. `project_files::resolve_current_chunk_entity` reads and chunks the checkout file.
7. `publish_prepared_provenance_import` calls `append_explicit_edges`; the daemon rebuilds `EdgeIndex`.
The current idempotency key is `bbox_edge_sidecar::edge_sidecar::edge_import_key`, hashing source, kind, target, and `anchor.commit_sha_at_edit`. A full caller walk found no V1 resolver over a pinned searcher or stored `relative_path`.

### 3.5 Landed provenance substrate
`crates/bbox-provenance/src/lib.rs` owns verified identifiers: `GitProvenanceNote`, schema versions 1 and 2, `GitProvenanceNotePart`, `NoteToolCall.target_ref`, `ProvenanceExportDocument`, `ProvenanceExportPlan`, `ProvenanceExportPage`, `fragment_note`, `serialize_note`, `parse_note_document`, `split_note_documents`, `validate_notes_ref`, `resolve_committed_scope`, `apply_export_page`, `append_note_documents_dedup`, and `capture_project_catalog_owner_snapshot_stable`.
`provenance_plan.rs` owns stable plan hashing, cursors, limits, and existing errors `error.stale_generation`, `error.tool_call_too_large`, `error.note_metadata_too_large`, and `error.invalid_cursor`. This slice extends these owners rather than forking them.

### 3.6 Assumed Phase deliverables
Implementation starts only after:
- P3-D supplies immutable history generations, materializer, and `RepoHistoryRebuildManifestV1`.
- P3-E supplies the path-free `relative_path`, `source_uri`, and `source_kind` fields.
- `P3-RV` and `G10-RV` supply the pinned `CodeReadView` used to locate active chunks by project and relative path.
- P3-F supplies `GitOverlaySelector`, consolidated history, health, vector lifecycle, and history GC.
- Phase 6 supplies `path-free-rebuild` and its committed-manifest startup gate.
If a landed owner name differs, update the anchor to the landed owner; do not recreate the planned type under another name.

## 4. Scope, deferrals, and predecessor relationship
### 4.1 Scope decision
This slice covers Git history and both provenance note directions as one Git transport.
Rationale:
- `G19` names typed Git-history/provenance before separate published knowledge.
- All three lanes need admitted checkout identity, repository access, commit namespace, and the same credential.
- Export and import share the notes ref and repo concurrency boundary.
- History alone leaves the adjacent untyped provenance adapter.
- Export alone leaves import checkout-bound and does not answer `PX-I`.

### 4.2 Published knowledge deferral
The later slice exclusively owns accepted knowledge/gap upload, `AcceptedPublicationStore`, pointers, establish/advance/rebind, visibility, aliases, and application of `GitProvenanceNote.knowledge_writes`.
This slice transports existing `knowledge_writes` bytes but ignores them exactly as committed import does. No note can mutate knowledge or gaps.

### 4.3 Relationship to `checkout-provenance-export-impl.md`
On ratification, this plan supersedes the proposed predecessor as provenance movement authority; GH-A then moves the predecessor lifecycle to `superseded`. Until ratification, this document consumes its landed substrate and closes its deferred import gate:
| Predecessor section | Treatment |
|---|---|
| Section 1 | Superseded as authority on this plan's ratification: export is joined by authenticated import and unattended sync. |
| Section 2 | Consumed as baseline, updated by landed crate and CLI paths. |
| Sections 3.1-3.2 | Superseded as primary authority on ratification; retained for manual MCP/CLI compatibility. |
| Sections 3.3-3.5 | Consumed: shared crate, V1/V2 schema, target ref, fragmentation, paging. |
| Section 4 | Consumed unchanged: `apply_export_page` remains the local writer. |
| Phase 0 and Phase A | Landed prerequisites, not reopened. |
| Phase B and Phase C | Landed interactive substrate, reused through producer authority. |
| Phase D | Superseded as authority by GH-F/G migration and cutover on ratification. |
| Section 6 | Import/off-host non-goals superseded; blame, render, push/fetch, and global rendering non-goals retained. |
| Section 8 | Retained where compatible and expanded. |
| Section 9 | Satisfied by producer attestation, note-generation commitment, validation, journal, and parity proof. |
GH-A changes the predecessor lifecycle to `superseded` and links this plan.

### 4.4 Non-goals
- No Git pack, bundle, object database, arbitrary ref, clone, fetch, or corpus-side Git command.
- No producer-selected project id, repo-history id, namespace, attachment id, or catalog epoch.
- No new token file, token table, token format, or auth family.
- No second `RepoHistoryGeneration` constructor or rebuild manifest.
- No entity-ref syntax change.
- No history for refs unreachable from exact observed `HEAD`.
- No automatic notes push/fetch/merge policy.
- No provenance deletion from snapshot omission.
- No caller-supplied raw `Edge` list.
- No `knowledge_writes` application.
- No blame, render, mutation, refactor, artifact, or tool/transcript transport.
- No cutover while coverage or parity is incomplete.
- No bridge-code deletion before its separate rollback gate.

## 5. Fixed decisions
### GH-FD-1: One slice, three lanes, one credential
Lanes are history upload, provenance export pull plus receipt, and provenance import upload. All use `CodeCollectionProducerConfig`.
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

### GH-FD-8: Existing provenance schema and writer are authoritative
Export uses `ProvenanceExportPage` and `apply_export_page`; import uploads exact note documents and parses with `parse_note_document`. No request contains an `Edge`.
Rejected: a collector-private schema, which breaks manual export and note interoperability.

### GH-FD-9: Producer export filters the observed lane to file edges
New plan-defined `bbox_edge_sidecar::edge_sidecar::read_observed_edges` supplies the producer planner.
Existing interactive `bbox_provenance_export_plan` retains committed `EdgeIndex` behavior; producer export uses a pure observed-lane sibling, then emits notes only for `EDITED_FILE` and `READ_FILE`. `RAN_BASH` remains in the observed corpus lane but is intentionally not transported, matching the legacy exporter and the landed note schema. Imported notes cannot loop.
Rejected: filtering imported edges by metadata, because dedup may preserve older untagged copies.

### GH-FD-10: Import is additive and journaled
Accepted snapshots union new document hashes with prior accepted inventory and append only new `edge_import_key` values.
New `ProvenanceImportJournalV1` binds note generation, project worklist, document commitment, edge-key commitment, and completion bitmap. Recovery resumes; omission never deletes.
Rejected: replace-all import, because rewritten or incomplete notes refs are not deletion authority.

### GH-FD-11: V2 normal path, V1 migration compatibility
V2 targets must be `ProjectFile` or `ProjectFileV2` for an admitted member.
V1 uses P3-E `relative_path` plus the `P3-RV`/`G10-RV` pinned active `CodeReadView` and stored chunk offsets. One match publishes; zero yields `provenance_legacy_target_unresolved`; multiple yield `provenance_legacy_target_ambiguous`.
Before cutover, each V1 repo runs final legacy import, producer V2 export, producer import, and `edge_import_key` parity.
Rejected: linking `bbox-chunker` into the collector, which moves parser authority producer-side.

### GH-FD-12: Published transport authority never falls back to checkout Git I/O
After GH-G, history for transport-governed `Published` repos evaluates producer state only; their catalog `bbox_provenance_export` and `bbox_provenance_import` calls return `error.provenance_transport_authoritative`; bridge paths remain exact.
`LegacyLocal` is not transport-governed. An attached Git `LegacyLocal` project retains P3-F's validated `GitHistory` lease-backed refresh under its local or imported legacy namespace, and this slice does not alter its other certified Phase 5 adapter behavior. Full local-adapter retirement belongs to the later governing section 19 gate.
A never-covered `Published` repo with a blocked grant is likewise not yet transport-governed and retains P3-F attachment refresh. If it was briefly transport-current before coverage, later grant re-block clears the invalid transport arm and resumes that refresh. This is deliberately asymmetric with GH-FD-16: producer removal after marker coverage is an authority decision, so that covered repo becomes `unavailable_no_transport` and never reacquires a checkout lease; incomplete assignment before first coverage is migration-in-progress, so it keeps pre-slice behavior.
`bbox_provenance_export_plan` remains because it performs no daemon Git I/O. Missing transport preserves last-good durable state and reports unavailable.
Rejected: transport-first with silent checkout fallback, which defeats zero-observation proof.

### GH-FD-13: Notes ref is server authority
Producer obtains validated `refs/notes/<safe-namespace>/provenance`; import echoes it exactly. `validate_notes_ref` remains structural authority.
Rejected: independent collector environment evaluation of `BBOX_GIT_NOTES_NAMESPACE`.

### GH-FD-14: No notes push/fetch policy
Collector reads and writes only local Git and reports local tip evidence. Operators retain ref distribution authority.
Rejected: automatic push/fetch, which adds remote credentials and destructive ref policy.

### GH-FD-15: Cutover is an offline, artifact-bound catalog operation
Add plan-defined `ProjectCatalogCommand::GitTransportCutover(GitTransportCutoverArgs)` with exact operator forms:
```text
blackbox project-catalog git-transport-cutover --preflight --report <path> --resolution <path>
blackbox project-catalog git-transport-cutover --apply --report <path> --resolution <path> --configured
blackbox project-catalog git-transport-cutover --verify --configured
```
All forms accept D-021 `ConfigArgs` with unchanged precedence: `--config`, then explicit `--state-dir` and `--projects-path` overrides. Preflight is read-only, resolves configured state through that precedence, and emits the GH-F coverage report, a canonical empty-or-explicit resolution artifact, plus predicted `GitTransportCutoverMarkerV1`. The verb returns the D-020 versioned result envelope with snake_case `command` values `project_catalog_git_transport_cutover_preflight`, `project_catalog_git_transport_cutover_apply`, and `project_catalog_git_transport_cutover_verify`.
Apply reuses `open_admin_store`, requires the configured-state opt-in, rechecks catalog epoch, grant commitments, generations, receipts, journals, parity commitments, and report/resolution hashes, then atomically writes a checksummed auxiliary marker beside the catalog store. The marker is not a `CatalogSnapshotV2` field and does not advance catalog epoch. Verify proves marker and live state agree before daemon restart.
`FreshV2` catalog stores with configured producers perform the same preflight, apply, verify, and restart ceremony. Their legacy-history and V1-provenance parity sets are vacuous, not exempt; uniform authority activation prevents a second startup mode.
Rejected: a live MCP authority flip, because shared-service state, stale artifacts, and rollback ownership need the Phase 6 offline-admin discipline.

### GH-FD-16: Code cutback does not reactivate Git checkout adapters
Removing a producer may cut code back to a validated local source through the existing source state machine. History and provenance retain last-good durable generations and become `unavailable_no_transport`; they do not acquire Git or note leases. Reassignment resumes transport.
Rejected: coupling code cutback to history/provenance fallback, because it violates GH-FD-12 and makes zero observations unstable.

## 6. Typed contract and storage model
All names below are new plan-defined identifiers unless stated as landed.

### 6.1 Dependency-clean crates
Create `crates/bbox-git-source` for wire structs, canonical encoders, commitments, validators, graph closure, and typed errors.
Allowed internal dependencies: `bbox-corpus-core`, `bbox-provenance`, `serde`, `sha2`, `thiserror`, `hex`, and small leaf utilities.
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

### 6.5 Overlay source provenance
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

### 6.6 Provenance export
Reuse landed `ProvenanceExportPage`.
```text
ProvenanceExportPullRequestV1 { scope, cursor, generation }
ProvenanceExportReceiptV1 {
  schema_version, scope, generation, notes_ref,
  document_count, ordered_document_commitment,
  local_notes_tip, written, unchanged
}
```
Routes:
```text
POST /internal/code-source/v1/provenance/export/page
POST /internal/code-source/v1/provenance/export/receipt
```
Page processing authenticates, resolves scope to project, reads the observed lane, filters to `EDITED_FILE` and `READ_FILE`, deliberately excludes `RAN_BASH` to match legacy export, and calls a pure planner sibling.
Collector validates root and committed scope, pages one generation, and calls `apply_export_page`. After page validation, that landed function calls `lock_repository`, acquiring the advisory lock in the repository's shared `git_common_dir`; it re-reads and writes notes while the lock is held. The collector bounded-restarts on stale, resolves local notes tip, and receipts.
Receipt is authenticated operational evidence, not cryptographic proof of remote Git.

### 6.7 Provenance import
```text
ProvenanceImportDescriptorV1 {
  schema_version, scope, notes_ref, notes_tip,
  manifest_sha256, document_count, logical_bytes
}
ProvenanceImportManifestEntryV1 {
  note_commit, document_ordinal, encoded_bytes, document_sha256
}
ProvenanceNoteGenerationV1 {
  generation_id, repo_history_id, notes_ref, notes_tip,
  ordered_document_commitment, documents, accepted_at
}
```
Routes:
```text
POST /internal/code-source/v1/provenance/imports
PUT  /internal/code-source/v1/provenance/imports/{id}/manifest/{page}
POST /internal/code-source/v1/provenance/imports/{id}/manifest/complete
GET  /internal/code-source/v1/provenance/imports/{id}/missing?cursor=...
PUT  /internal/code-source/v1/provenance/imports/{id}/documents/{sha256}
POST /internal/code-source/v1/provenance/imports/{id}/finalize
GET  /internal/code-source/v1/provenance/generations/{generation}/status
```
`note_commit` is the Git object carrying the note and must equal `GitProvenanceNote.commit`; it is distinct from `NoteToolCall.target_ref`, which names the project-file entity targeted by an imported edge.
Collector captures one stable listing through `StableGitRepository::snapshot_notes_bounded`, preserving note commit and exact document bytes.
Corpus validates scope/grant, exact notes ref, object ids, hash, UTF-8, schema, note-commit equality, V2 part metadata, target membership, conflicts, counts, and bytes. Server derives generation id.

### 6.8 Store layout and GC
```text
git-sources/
  records/sha256/<first-two>/<hash>
  uploads/<producer-hash>/<upload-id>/...
  repos/<repo-history-id>/history/<source-generation-id>/...
  repos/<repo-history-id>/provenance/<note-generation-id>/...
  activations/<repo-history-id>.json
  provenance-import/<note-generation-id>.journal.json
  provenance-export/<project-id>.receipt.json
  quarantine/...
```
Private no-follow directories; same-filesystem temp, file fsync, atomic rename, parent fsync; checksummed versioned journal replacement.
The root is sibling to lexical index and P3 history generations, so schema replacement cannot delete it.
History roots: open uploads, ready/in-flight/active/retained source generations, activation journals, and source evidence referenced by retained P3 generations.
Provenance roots: open imports, current/retained note generations, unfinished journals, latest receipt, and failed/quarantined generations pending acknowledged retirement.
P3 GC remains sole authority for `RepoHistoryGeneration` and vectors.

## 7. Runtime, transaction, lock, and recovery mechanics
### 7.1 Producer schedule
Extend `bbox-code-collector`; do not create a second checkout daemon.
New additive collector project flags `git_history` and `provenance` default false.
Loop: validate roots/scopes, publish code, group by Git common dir, obtain contract, pull/apply export per project, upload history if HEAD changed, upload notes if tip changed, report bounded lane status.
Code activation never waits for history or provenance. Lane backoff is independent.

### 7.2 Repo grouping and monorepos
Local grouping is scheduling only; server `RepoTransportGrant` is authority.
Reject nonowning producer, nonmember V2 target, unsafe repo path, and concurrent producer for one repo-history id.
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

### 7.5 Provenance export transaction
Plan pins project id, observed-lane commitment, notes ref, and ordered document inventory. Paging changes no server state.
Landed local semantics remain: all checks precede page writes; `apply_export_page` then acquires `lock_repository` on the shared Git common directory, re-reads existing notes, and holds that advisory lock through merge-strategy configuration and page writes. Commit writes are individually durable, and a crash leaves a valid prefix. Hash dedup makes restart safe.
Final receipt must match current plan generation; stale receipt rejects and restarts. Last successful receipt is health evidence.

### 7.6 Provenance import transaction
Finalize installs immutable note generation, then prepares edge rows without publication lock.
V2 resolves `target_ref`; V1 resolves pinned corpus chunks; invalid calls are excluded with bounded diagnostics.
Accepted edges retain current `edges_from_note` fields and add note-generation/document-hash metadata without changing `edge_import_key`.
Projects sort by id. For each: acquire provenance sidecar lock, verify journal, call `append_explicit_edges`, fsync, mark complete, release.
After all projects: rebuild `EdgeIndex` once, verify edge-key commitment, commit journal.

### 7.7 Lock order
1. Process-lifetime migration lock for offline commands.
2. Catalog store/transaction lock.
3. The process-local `bbox_edge_sidecar::snapshot::MANIFEST_COORDINATOR` `OnceLock<Mutex<()>>`, acquired only through `bbox_edge_sidecar::snapshot::with_manifest_coordinator`.
4. Writer actor ownership, never caller-held mutex.
5. Git-source upload/GC lock.
6. Per-project provenance sidecar lock.
7. Producer-local Git common-directory lock.
No path needs all layers: producer Git locks never enter daemon; upload locks release before materialization; catalog locks release before index commit; provenance publication holds no catalog lock; GC uses pinned references; auth reads are short.

### 7.8 Reload and security
Reload builds one replacement auth table, code assignments, repo grants, and limits. Only a valid auth table swaps; repo split installs blocked health without breaking valid code grants.
Removing producer revokes future requests. Accepted generations remain; in-flight upload cannot finalize after grant recheck.
Bearer auth precedes parse; scope precedes lookup/write; ids are producer-bound; server derives catalog ids; paths validate at every boundary; notes ref uses `validate_notes_ref`; object ids are consistent lower hex; documents are UTF-8/schema validated; only `READ_FILE`/`EDITED_FILE` import; invalid refs skip with diagnostics; secrets and content do not enter ids, logs, or metric labels.

### 7.9 Cutover transaction
`RepoHistoryRecord` gains additive `#[serde(default)] membership_generation: u64` in `bbox-corpus-core::project_catalog`. Its durable home is each record in `CatalogSnapshotV2.repo_histories`; old records decode as zero and new writers emit the field.
`ProjectCatalogStore::transact` in `crates/bbox-indexing/src/project_catalog_store.rs` is the sole bump authority. It snapshots the ordered pre/post membership projection for every `RepoHistoryId`: every referencing project's `(project_id, ProjectScope, repo_history_id)`, where `PublishedScope` carries repo authority and `bbox_root_relpath`. After the transaction closure returns, but before candidate validation/publication, the store requires existing records to retain their pre-transaction `membership_generation` and new records to enter at zero, computes changed source and target repo ids, and checked-increments each surviving affected `RepoHistoryRecord` exactly once. A new record with members therefore publishes at one; a removed record is simply absent. This automatically covers `promote_project`, `retire_project`, and both attached and operator-attested scope migration because they all commit through `ProjectCatalogStore::transact`; routine materialization, attachment, alias, and assignment-config changes do not alter the projection. Overflow refuses before publish with plan-defined `error.project_catalog_membership_generation_overflow`.
`GitTransportCutoverMarkerV1` binds report hash, resolution hash, predecessor catalog epoch, an apply-time aggregate producer grant hash, per-repo coverage rows containing the `RepoTransportGrant` commitment plus `membership_generation`, accepted history source/P3 generation pairs, accepted provenance generations, export receipts, zero-prepared-journal proof, parity commitments, and apply timestamp.
Apply acquires the offline admin store and store mutation lock, rechecks every bound input, fsyncs and atomically replaces the auxiliary marker, verifies it, and emits a receipt. A crash before rename leaves no marker; a crash after rename is completed by `--verify`.
Startup revalidates the marker checksum/schema, that this is the atomically selected current artifact rather than a superseded predecessor, and each covered repo independently. Row validity requires equality of both the current member-assignment commitment and `RepoHistoryRecord.membership_generation`. The assignment commitment covers repo-history id, assigned producer id, and ordered `(project_id, PublishedScope, bbox_root_relpath)` assignments. A token rotation, limit change, primary-namespace change, or grant change for another repo does not alter the subject repo's row; ordinary runtime matching still rejects any accepted generation that no longer matches current namespace or authority.
The predecessor catalog epoch, aggregate grant hash, exact accepted generations, receipts, zero-prepared-journal proof, parity commitments, report hash, resolution hash, and apply timestamp are apply-time evidence only. Their current-state equality is not a startup requirement. Generation presence and integrity remain governed by ordinary history recovery and the P6-C `Ready`/manifest rules, not by marker epoch equality.
Routine `transact` epoch advances do not stale the marker. Explicitly tolerated non-authority mutations are alias/display-name changes, attachment add/detach/status/default changes, code activation, history `Ready` advancement, retained-generation and GC bookkeeping, and changes to repos outside the marker. A newly added transport repo remains unauthorized until a newer marker covers it, but does not invalidate covered repos.
A covered repo's transport coverage is current only while both its own assignment commitment and membership-generation watermark equal its marker row. Other covered repos remain authoritative, and the marker artifact remains valid for their rows. Exact restoration of a removed producer assignment without a catalog membership change restores assignment equality while the watermark remains unchanged, so that repo's original row becomes current again under GH-FD-16. Any committed member retire, promotion, or scope migration advances the watermark and permanently invalidates the old row until re-cutover, even if a later symmetric round-trip restores byte-identical assignments. A newer marker artifact carries forward unchanged valid rows and replaces only affected repo rows. Absent or corrupt artifacts fail separately. For a valid artifact, strict catalog startup refuses transport authority only for affected rows while bridge rollback remains available.

### 7.10 Checkout-observation taxonomy
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
The taxonomy is evaluated per `(repo_history_id, capability)` so one repo has exactly one row per applicable capability. `overlap_window` begins at the earlier of GH-F parity-work start or the first observation snapshot, folding fixture setup and any pre-GH-F lease evidence into the same expected baseline. Its `ProvenanceNoteIo` arm ends when the marker first covers that repo. Its pre-coverage `GitHistory` arm is predicate-driven rather than event-latched: whenever `history_transport_current(repo)` is false, attachment-refresh deltas remain expected `overlap_window` evidence regardless of any prior transport publication; whenever the predicate is true, the capability moves to `history_transport_current_pre_cutover` and requires zero deltas. The report stores baselines at predicate transitions and evaluates deltas only while each row is active.
`history_transport_current_pre_cutover` applies only to Git history while the GH-FD-7 currency predicate is true before marker coverage. The row requires zero `GitHistory` lease deltas but makes no G19 authority claim. The marker independently governs provenance-adapter refusal and entry into the `Published` transport portion of G19.
`blocked_published_never_covered` names a `Published` repo whose complete `RepoTransportGrant` does not exist because a member is unassigned or split. Its P3-F attachment `GitHistory` rows remain expected and named until a later marker covers it. If it previously reached `history_transport_current_pre_cutover`, grant re-block clears the invalid transport overlay through GH-FD-7, the currency predicate becomes false, and attachment refresh resumes. It gains no producer overlay or transport provenance authority while blocked.
The two `legacy_local_*` rows distinguish v2-created `LocalProject(ProjectId)` from migrated `LegacyNamespace(CommitNamespace)` history authority. Both keep lease-backed refresh and remain project-local.
`covered_producer_removed` is the specific temporary state where the repo's producer assignment was explicitly removed without a catalog membership mutation. It records no target lease, retains last-good state, and reports `unavailable_no_transport` under GH-FD-16. Exact assignment restoration returns it to its prior covered row without requiring a new marker; unrelated covered rows never change.
`covered_blocked_pending_recutover` is the specific state where a committed membership addition blocks the current all-members grant, notably when promotion adds an unassigned Published member to a covered repo. The promotion also advances membership generation, but this specific blocked reason takes precedence over generic stale-pending classification. It retains last-good state but freezes history for the whole repo with no checkout fallback until assignment plus newer-marker coverage. The minimal-window operator runbook is: promote, assign the new member to the same producer, then run the GH-F/G re-cutover ceremony immediately; the freeze window is operator-bounded.
`coverage_stale_pending_recutover` applies when either the current assignment commitment or membership-generation watermark differs from the row. It retains last-good state, acquires no checkout fallback, and exposes no transport authority while invalid. If the mismatch is config-only, the watermark is unchanged and exact assignment restoration before any catalog membership mutation returns the original row current under section 7.9 with no newer marker. Once membership generation advances, equality can never return for that row, even after a symmetric scope-migration round-trip; a newer marker covering every changed repo is required.
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
7. Because this plan is ratified movement authority before implementation begins, move the predecessor lifecycle to `superseded` and link this plan.
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
10. Derive catalog checkout refresh from `history_transport_current(repo)`: suppress refresh while a verified `ProducerTransport` arm is current, even before marker coverage; if a never-covered repo loses grant completeness or selector validity, GH-FD-7 clears the arm and eligible P3-F attachment refresh resumes. Marker coverage separately controls provenance refusal and the G19 authority claim, and after coverage the no-fallback rules override resumption.
Parity: equal facts yield identical commit docs, vectors, parent/file edges; selector source provenance and remote overlay availability are the only intended overlay changes.
Verification: checkout-vs-typed golden, monorepo fan-out, head/force-push/detach/retire/GC matrices, attachment-to-transport source swap, publication-before-marker and marker-before-publication orders, complete grant then pre-marker publication then member unassignment/split then overlay clear and attachment-refresh resumption, publication then routine code-ahead mismatch then refresh resumption then matching transport republish, mismatch clear, detach independence, sibling retention, every journal-checkpoint/probe crash boundary, `DenyCheckoutAccess` search/graph bootsmoke.
Gate: focused history/writer/edge/vector/doctor tests, cluster verify, strict catalog smoke.

### GH-D: Authenticated provenance export
Ownership: `bbox-provenance`, `provenance_plan.rs`, edge sidecar, `git_source.rs`, collector, tool docs.
Dependencies: GH-A, `PX-S`, `PX-V2`, `PX-P`, `PX-W`, `G14-L`.
Mechanics:
1. Add observed-lane reader.
2. Add pure producer planner that emits only `EDITED_FILE`/`READ_FILE`; assert `RAN_BASH` exclusion matches legacy export.
3. Add page/receipt routes.
4. Add collector page/apply/restart/receipt.
5. Preserve interactive plan and CLI bytes.
6. Add export health/counters.
7. Prove imported explicit edges never re-export.
Verification: lane fixtures including retained corpus-only `RAN_BASH`, page/cursor/cap/stale/receipt/ref tests, common-dir lock, crash after write before receipt, deterministic idempotent smoke.
Gate: focused provenance/sidecar/MCP/CLI/collector tests, dependency acceptance, concurrency lint, cluster verify.

### GH-E: Authenticated provenance import
Ownership: Git source crates, `bbox-provenance`, `provenance.rs`, corpus path-free lookup, edge sidecar, `git_source.rs`, collector.
Dependencies: GH-C, GH-D, `P3-E`, `P3-RV`, `G10-RV`, `PX-I`.
Mechanics:
1. Add stable notes capture.
2. Add import upload/finalize/status.
3. Validate V2 targets/membership.
4. Add pinned-corpus V1 resolver.
5. Preserve `edges_from_note` semantics.
6. Add note generation, additive inventory, journal, replay.
7. Add metadata without changing import key.
8. Rebuild in-memory edge index once.
9. Ignore `knowledge_writes`.
10. Add health/quarantine.
Verification: V1/V2/multipart/duplicate/conflict/malformed/cross-project/oversize matrices, tip probe, all crash points, replay, remote graph smoke.
Gate: focused note/store/resolver/sidecar/graph/collector tests, cluster verify, strict catalog smoke.

### GH-F: Overlap migration and parity proof
Ownership: `bbox-corpus-core` catalog types, `ProjectCatalogStore::transact`, migration/reporting in `bbox-indexing`, doctor, `src/tools/graph.rs`, operations/smoke fixtures.
Dependencies: GH-C through GH-E and Phase 6.
Mechanics:
1. Land `RepoHistoryRecord.membership_generation` and the automatic `ProjectCatalogStore::transact` projection-diff bump before any preflight report is accepted.
2. Implement `git-transport-cutover --preflight --report <path> --resolution <path>` with the D-020 envelope, snake_case command value, and D-021 `ConfigArgs` precedence.
3. Inventory heads, commit/vector commitments, overlays, notes tips, V1/V2 counts, legacy/typed edge keys, grants, counters, membership generations, and the per-repo, per-capability `overlap_window` baseline.
4. For each V1 repo proposed for marker coverage, run operator-controlled legacy import, producer export, producer import, and parity compare.
5. Require typed history parity with checkout generation at same HEAD.
6. Require typed provenance keys cover legacy keys.
7. Require an export receipt for every member of each repo proposed for coverage; blocked-Published repos are excluded from coverage and named in the report.
8. Require no prepared journal.
9. Emit report and canonical empty-or-explicit resolution artifacts bound to catalog epoch, grants, membership generations, generations, tips, and commitments.
10. Run the identical ceremony for `FreshV2` producer stores with vacuous legacy parity sets.
11. Leave blocked-Published repos uncovered and preserve their attachment refresh; change no authority.
Verification: complete/missing/split/stale/unresolved/mismatch/corrupt/prepared/detached matrices, blocked-Published exclusion plus retained refresh, FreshV2 vacuous-parity coverage, and one full rehearsal.
Gate: migration/doctor tests, cluster full verify, reviewed report fixture.

### GH-G: Strict catalog cutover
Ownership: runtime source selection, `code_source.rs`, `graph.rs`, docs, observation assertions, operations.
Dependencies: accepted current GH-F receipt.
Mechanics:
1. Implement offline `--apply` and `--verify`; write `GitTransportCutoverMarkerV1`.
2. Validate current-marker identity and each covered repo row independently at startup; stale or blocked rows do not invalidate unrelated covered repos, and apply-time evidence plus routine epoch tolerance remain exact to section 7.9.
3. Remove each covered `Published` repo's `GitHistory` lease path after its first verified `ProducerTransport` overlay publication, a source swap when an `Attachment` arm existed; preserve never-covered blocked-Published refresh and both named `LegacyLocal` refresh shapes.
4. Make legacy provenance tools for transport-governed `Published` projects fail before lease; leave `LegacyLocal` certified adapter behavior unchanged.
5. Preserve bridge paths/assets.
6. Keep interactive export plan/CLI.
7. Preserve last-good on unavailable transport, never fallback.
8. Assert the section 7.10 taxonomy across startup, incremental, rebuild, upload, detach, reload, failure, and recovery: overlap deltas are expected evidence; both transport-current categories have zero post-boundary deltas; blocked-Published and both `LegacyLocal` classes have only their named history rows; removed, blocked-pending, and stale-pending covered rows have no fallback; no observation crosses repo or category.
9. Report the `G14-H`/`G14-P` transport rows as path-free only for `Published` transport repos in their capability-specific post-swap or post-cutover windows, name overlap, transport-current-pre-cutover, blocked-Published, both pending-recutover categories, and both surviving `LegacyLocal` rows, and leave the governing section 19 all-adapter gate open.
Verification: attached Published overlap refresh and selector swap in both marker orders, complete grant then pre-marker publication then grant re-block and attachment-refresh resumption, publication then routine code-ahead mismatch then refresh resumption and transport republish, empty-attachment covered boot, complete remote flow, blocked-Published refresh, both `LegacyLocal` authority refreshes, covered producer removal with unrelated-repo survival, config-only assignment restoration without re-cutover, member retire plus newer-marker recovery, LegacyLocal promotion plus immediate assignment/re-cutover recovery, scope-migrate out/in with source/target row recovery, symmetric scope-migration round-trip remaining stale until newer-marker recovery, attempted legacy tools, marker epoch-tolerance and own-row-staleness matrices, bridge rollback, full cluster closeout, fresh adversarial review.
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
New provenance HTTP codes:
- `provenance_transport_disabled`
- `provenance_notes_ref_mismatch`
- `provenance_manifest_out_of_order`
- `provenance_document_invalid`
- `provenance_document_conflict`
- `provenance_target_project_forbidden`
- `provenance_legacy_target_unresolved`
- `provenance_legacy_target_ambiguous`
- `provenance_export_stale_generation`
- `provenance_export_receipt_mismatch`
- `provenance_import_incomplete`
- `provenance_publication_failed`
New MCP error: `error.provenance_transport_authoritative`.
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
Provenance health:
```text
export = current | lagging | unavailable | failed
import = current | lagging | blocked_v1 | unavailable | failed
```
Doctor reports ref, last attested tip, note generation, counts, quarantine, edge commitment, journal, and receipt, never bodies.

## 10. Bridge parity contract
Complete intended observable changes and preserved exceptions:
1. New authenticated history/provenance internal routes.
2. Additive transport config fields default disabled.
3. Collector can publish history, apply export, and upload import.
4. Remote-only projects can gain commit docs and overlays.
5. History health gains producer evidence and `unavailable_no_transport`.
6. Producer export reads the observed lane and emits only `EDITED_FILE`/`READ_FILE`; `RAN_BASH` remains corpus-observed but is intentionally not transported, matching legacy export.
7. Local notes can change automatically through landed V2 writer.
8. Typed import appends edges without checkout.
9. V1 producer import resolves pinned corpus state and reports ambiguity.
10. GH-G legacy provenance tools for transport-governed `Published` projects return `error.provenance_transport_authoritative`.
11. GH-G `Published` transport repos record zero post-boundary Git-history/provenance-note lease deltas and never fall back.
12. Published target-lease rows are judged by the active capability category: pre-cutover history may return to expected `overlap_window` evidence after prior transport publication whenever currency becomes false.
13. Before marker coverage, Git-history lease evidence is predicate-driven: a current transport arm suppresses refresh, while grant re-block or routine head mismatch resumes expected attachment refresh regardless of prior publication.
14. Attached Git `LegacyLocal` projects with either `LocalProject` or `LegacyNamespace` authority retain their `GitHistory` refresh and may record that named lease; this is not transport fallback.
15. `GitOverlaySelector` replaces flat `attachment_id` source identity with `GitOverlaySourceV1`; existing local overlays use `Attachment`, and transport overlays use `ProducerTransport`.
16. Covered committed membership changes advance a durable repo-history watermark and enter named blocked-pending or stale-pending rows; config-only assignment mismatch can recover because the watermark is unchanged, while every committed change requires a newer marker even after a symmetric round-trip.
17. Bridge mode remains exact.
Everything else remains byte-identical: code-source routes, code identity/activation, commit refs/docs/truncation/vectors/edges, interactive plan args/response, `bro provenance export`, note JSON/separator/merge strategy, imported edge fields/import key, knowledge non-application, published knowledge/gaps, blame, render, mutation, artifacts, tool/transcript behavior.
Any drift outside these seventeen entries requires plan amendment and re-review.

## 11. Test and validation plan
### 11.1 Contract and auth
- Round-trip every struct; reject unknown fields.
- Deterministic ids under canonical permutation; every authority/content field affects id.
- Limits at zero, exact cap, cap plus one, overflow.
- Object-id/path/ref/V1/V2/page/cursor/error golden corpora.
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

### 11.3 Provenance parity
Import equal V2 notes through legacy and typed paths; assert document hashes, source, kind, target, confidence/provenance, anchor metadata, `edge_import_key`, and graph results.
For V1, assert parity when checkout and pinned corpus bytes agree; divergence follows pinned corpus and emits health.

### 11.4 Fault injection
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
| Export write before next page | restart dedups prefix |
| All export writes before receipt | reapply unchanged, receipt |
| During note document stream | remove temp, hash missing |
| Note generation before journal | prepare journal |
| Mid-project edge append | dedup resume |
| Between projects | resume completion bitmap |
| Appends before edge rebuild | rebuild on startup |
| Edge rebuild before journal commit | verify and commit |
| GC during activation | pinned objects survive |
| Producer revoked mid-upload | next request denied, no finalize |

### 11.5 Bootsmokes and gates
Use throwaway state, isolated ports, temporary repos, no shared-service restart.
Smokes: existing code collector after GH-A; history ready after GH-B; remote history/search/overlay after GH-C; automatic export after GH-D; remote import/graph after GH-E; migrated and FreshV2 overlap reports after GH-F; attached Published overlap-to-transport swap, empty-attachment covered boot, blocked-Published refresh, both `LegacyLocal` authority shapes, covered producer removal, and bridge rollback after GH-G.
Mid-cycle plan commands: `scripts/fmt.sh --check`, targeted `cargo nextest run --workspace` expressions, three dependency acceptance scripts, and `scripts/lint-concurrency.sh`.
Closeout: cluster verify wrapper, full workspace nextest profile, clippy/concurrency, isolated daemon plus collector rehearsal, and fresh adversarial implementation review resumed to exact pass.

## 12. Exit-gate proof
Fixture: covered repo A is the attached Published swap candidate; covered repo B has two published collected members sharing one repo history; covered repo C is detached and remote-only; one never-covered blocked-Published repo has an assigned attached member and an unassigned sibling; one v2-created attached Git `LegacyLocal` project has `LocalProject` authority; one migrated attached Git `LegacyLocal` project has `LegacyNamespace` authority; V1 note is upgraded during overlap; multipart V2 notes, matching/lagging heads, retained prior generations, and windowed observation counters are present. The checkout policy permits only category-valid leases before each per-capability boundary and denies every target lease where section 7.10 requires zero.
Sequence:
1. Start strict catalog without a Git-transport marker and snapshot `overlap_window` counters.
2. Lease-refresh the attached Published swap candidate, producing an `Attachment` overlay; also refresh the blocked-Published repo and both `LegacyLocal` authority fixtures.
3. Upload and activate code and typed history for coverable repos; perform provenance export/import parity and receipts.
4. Run GH-F preflight and GH-G apply/verify, covering every eligible repo while leaving the blocked-Published repo uncovered.
5. Record each covered repo's post-cutover provenance baseline; publish the swap candidate's verified `ProducerTransport` overlay and record its post-swap history baseline.
6. Exercise commit lexical/hybrid search plus `COMMIT_PARENT` and `COMMIT_TOUCHED_FILE` expansion.
7. Activate a mismatching code HEAD, observe overlay clear, then upload matching history and restore the transport overlay.
8. Run P6-R full path-free rebuild and restart recovery.
9. Detach the swap candidate's attachment and prove its transport overlay remains selected.
10. For repo B, remove the retiring member's assignment as `G-D12` requires, assert B enters `coverage_stale_pending_recutover` while A and C remain current, retire the member, run source/history GC, prove the surviving B sibling and retained references survive, then run the GH-F preflight plus GH-G apply/verify ceremony to install a newer marker carrying B's replacement row and prove B returns to transport-current.
11. Remove repo A's producer assignment and prove A enters `covered_producer_removed` with `unavailable_no_transport` and no lease fallback while B and C retain transport authority; restore A's exact assignment and prove A resumes without changing B or C.
12. Refresh the still-blocked Published repo and both `LegacyLocal` fixtures.
13. Attempt legacy provenance tools against covered Published transport projects.
14. Read the windowed observation report and exercise bridge rollback.
Enumerated overlay assertions:
1. Swap: the published `Attachment` selector becomes `ProducerTransport` atomically with no mixed manifest.
2. Mismatch clear: a new code HEAD cannot retain the old transport overlay.
3. Detach independence: detaching the former source attachment cannot clear a valid transport arm.
4. Retire/GC: no source or P3 generation is freed while a sibling, retained overlay, read view, in-flight build, or rebuild manifest references it.
Observation assertions: pre-GH-F and GH-F overlap counters share one expected baseline; every covered repo has zero target-lease delta after its provenance cutover and first transport-overlay publication boundaries regardless of order; the never-covered blocked-Published repo has only named P3-F history rows and no producer overlay; both `LegacyLocal` authority shapes have only their named project-local history rows; removed, blocked-pending, and stale-pending covered repos have no lease fallback; repo-local membership or producer changes do not alter unrelated covered rows. Expected docs/vectors/overlays/edges exist, no host path enters published transport identity, no legacy cursor seeds a producer generation, and bridge rollback serves retained state.
This proves the `Published` Git-history/provenance transport part of `G19` in the correct per-repo capability windows while truthfully preserving overlap evidence, never-covered blocked-Published refresh, and both `LegacyLocal` history adapters. It does not claim full off-host mobility before published knowledge and the later all-adapter zero-observation gate.

## 13. Three riskiest calls
### Risk 1: Repo authority from scope credentials
Accepted operational consequence: one unassigned published monorepo member, or one member assigned to another producer, blocks history and provenance transport for the whole repo indefinitely. While never covered, the repo retains attachment-backed P3-F history but gains no producer overlay or transported provenance; whenever pre-coverage currency is false, including routine code-ahead mismatch after prior publication, attachment refresh resumes. Once covered, member retire, promotion, or scope migration moves only affected repos into a no-fallback pending-recutover row until a newer marker covers the changed membership. The durable membership watermark makes this strict even for symmetric scope-migration round-trips. Promotion therefore freezes repo history until the operator assigns the new member and immediately re-cuts over; that window is deliberately operator-bounded. This is intentional because granting one subproject's credential authority over sibling history would widen authority silently. The operator must either assign the member to the same producer or scope-migrate it to a distinct recorded repo authority.
### Risk 2: V1 provenance without checkout bytes
One-time V2 upgrade plus bounded active-corpus fallback preserves history while refusing ambiguity; V2-only strands notes and producer chunking forks parser authority.
### Risk 3: Extending the P3 transaction
Producer transport adds a caller to the single P3 creation path only through the certified same-commit amendment and implementation-time Decision Ledger mechanic. The activation journal must coordinate intake, catalog, writer, overlay, and recovery without becoming a second rebuild manifest.

## 14. Recommended implementation order
1. GH-A, prove no auth drift.
2. GH-B, retain history at `ready`.
3. GH-C, prove typed/checkout history parity.
4. GH-D, prove exact landed V2 export.
5. GH-E, prove import parity and replay.
6. GH-F, complete overlap rehearsal.
7. GH-G only from current accepted receipt.
8. Author published-knowledge transport next.

## 15. Author sign-off summary
1. Milestone spine: GH-A extracts one producer auth runtime and lands the typed wire contract.
2. Milestone spine: GH-B adds resumable complete-history intake and collector capture without publication.
3. Milestone spine: GH-C feeds the P3-F creation path and activates remote commit views and overlays.
4. Milestone spine: GH-D pulls corpus-authored provenance pages and applies them with the landed local writer.
5. Milestone spine: GH-E uploads stable note snapshots and replays validated edges through a durable journal.
6. Milestone spine: GH-F proves history, provenance, grant, receipt, and observation parity before authority changes.
7. Milestone spine: GH-G applies per-repo transport windows, retains blocked-Published and both LegacyLocal history adapters, and preserves bridge rollback.
8. Riskiest call 1: one unassigned or split published member blocks whole-repo transport until assignment or scope migration.
9. Riskiest calls 2 and 3: V1 target migration refuses ambiguity, and typed history must extend the single P3 creation path without forking recovery.
10. Relationship: on ratification this plan supersedes `checkout-provenance-export-impl.md` as movement authority; GH-A marks it superseded, while consuming its schema, paging, local writer, and deferred import gate.
