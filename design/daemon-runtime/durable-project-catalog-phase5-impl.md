---
title: "Durable project catalog Phase 5 implementation plan"
kind: design
lifecycle: complete
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, project-identity, accepted-publication, knowledge, gaps, checkout-leases]
brief: "Make verified accepted publication the path-free catalog authority for knowledge and gaps, add safe publisher establishment and advance, rebuild provisional overlays from checkout-local ancestry, and convert every remaining checkout-side adapter to capability-specific leases with bounded health."
---
# Durable project catalog Phase 5 implementation plan
Date: 2026-07-25
Governing design: [`durable-project-catalog-impl.md`](durable-project-catalog-impl.md) sections 5 through 7, 9, 13 through 17.
Phase lineage: [`durable-project-catalog-phase1-impl.md`](durable-project-catalog-phase1-impl.md), [`durable-project-catalog-phase2-impl.md`](durable-project-catalog-phase2-impl.md), [`durable-project-catalog-phase3-impl.md`](durable-project-catalog-phase3-impl.md), and [`durable-project-catalog-phase4-impl.md`](durable-project-catalog-phase4-impl.md).
Decision authority: [`DECISION_LEDGER.md`](../../DECISION_LEDGER.md).

## 1. Required outcome
At the Phase 5 exit gate, proved against isolated migrated v2 state and the bridge parity harness:
1. A catalog project with a verified accepted-publication generation serves published knowledge and gaps by durable project identity with `DenyCheckoutAccess` installed and zero checkout lease acquisitions.
2. The same accepted bytes remain readable after every attachment is detached.
3. Catalog published reads never consult `PublisherRefStore`, `elect_publisher`, `PublisherAuthorizationCache`, Git, or repo-local recall statistics.
4. The accepted-publication runtime exposes immutable verified content, separate content and binding stamps, Current/Prior selection, and bounded per-project status through a narrow public `bbox-indexing` facade.
5. Publisher establish creates the first pointer only when pointer absence is proved and one named attachment, full ref, commit, scope, knowledge lane, and gap lane all verify.
6. Publisher bind changes attachment only. Publisher advance writes one immutable dual-lane generation off-lock and compare-and-swaps one pointer under the publication lock.
7. Existing-pointer advance requires expected catalog epoch, expected generation id, and expected pointer SHA-256.
8. A pointer serving its prior generation remains readable but refuses establish, bind, and advance until repair.
9. Scope migration serves the old accepted generation with its old-scope `built_from` stamp and stale health until advance creates a generation at the catalog's current scope.
10. Provisional overlay keys remain `OverlayKey { published_scope, checkout_id }`.
11. Catalog overlay computation receives accepted published manifests and checkout-local Git evidence through a separate entry point with no publisher alternate.
12. `published` remains available whenever the accepted generation verifies. `own` fails explicitly when baseline proof is unavailable. `all` omits only unavailable peers and reports structured degradation.
13. File inspection, blame, render, provenance note I/O, artifact watching, repository mutation, and tool-edge path resolution open checkout bytes only while the matching capability lease is alive.
14. Tool-edge leases remain in `bbox-indexing`. The lower `bbox-corpus-index` carrier contains pure project identity and already-validated roots, not `ValidatedCheckoutLease`.
15. Artifact watchers use native attachment identity while retaining `ArtifactWatchAccess`, event-time lease reacquisition, and publication guards.
16. Checkout observations keep their closed, low-cardinality key space. Per-project capability status is a separate bounded runtime projection.
17. Tool refusals use the existing `CallToolResult` text envelope with stable code prefixes.
18. No new `BuiltFromStamp` variants are added in this phase.
19. The final publisher-bind detach-at-swap residual from D-033 item 1 remains observable and repairable rather than claimed eliminated.
20. No catalog-only corpus request requires `ProjectRecord`, every remaining checkout open is lease-counted, and remote-only projects degrade per capability.
The production authority switch remains prohibited by [D-002](../../DECISION_LEDGER.md#d-002). Phase 5 builds and proves catalog behavior on isolated rehearsal state without applying v2 bytes to configured operator state.

## 2. Scope and non-goals
### 2.1 In scope
- Promote the Phase 1 accepted-publication substrate into a narrow live runtime facade without exposing codecs, raw pointer bytes, or lock guards.
- Add startup verification before route bind.
- Add dedicated catalog published caches keyed by accepted content identity.
- Wire the catalog branches of `session_knowledge_view` and `session_gap_view` to verified generations.
- Add read-only publisher status.
- Add explicit pointer-absence establishment through the publisher advance surface.
- Add pointer-specific compare-and-swap for existing advance.
- Preserve attachment-only bind.
- Preserve Current/Prior verification and make Prior mutation-refusing.
- Preserve the scope-migration publication bridge until new-scope advance.
- Add a catalog overlay recompute entry point that cannot receive a publisher alternate.
- Strengthen overlay stamps and eligibility while keeping overlay keys unchanged.
- Convert the governing section 14 adapters to catalog identity and typed leases.
- Add native attachment-id watcher carriers.
- Remove `ProjectRecord` from the lower tool-edge carrier.
- Keep checkout observation counters unchanged.
- Add separate accepted-publication, publisher, overlay, and capability health.
- Add code-prefixed errors for accepted-publication and overlay domain failures.
- Add bridge parity, fault injection, bootsmoke, static ownership lint, runtime denial probes, and call-site audit.
### 2.2 Non-goals
- No Phase 6 authority cutover.
- No deletion of `ProjectRegistry`, `ProjectRecordsProvider`, `PublisherRefStore`, bridge caches, bridge overlay code, or legacy source lanes.
- No dual-read fallback from accepted publication to the legacy publisher store.
- No new accepted-publication codec version.
- No changes to the limits or generation-id contract fixed by [D-014](../../DECISION_LEDGER.md#d-014).
- No Git ancestry claim from accepted content.
- No publisher or peer Git alternate in catalog overlay computation.
- No automatic publisher election or automatic rebind after attach.
- No fetch, pull, checkout, or object transfer to repair a missing accepted commit.
- No catalog overlay rekeying.
- No new `BuiltFromStamp` variants.
- No project ids, attachment ids, or paths added to `CheckoutAccessObservations`.
- No structured MCP error envelope.
- No partial-success redesign of legacy provenance import/export.
- No daemon refactor MCP revival.
- No requirement that unregistered `bbox_project_init(path)` already have an attachment.
- No claim that Phase 5 closes D-033 item 1.
- No production daemon restart or shared-service mutation as part of validation.

## 3. Survey of the current tree
Code anchors in this section were verified at `994b3187f61c`. This section is inventory, not intent.
### 3.1 Accepted publication exists below a crate visibility boundary
- `accepted_publication_store` is a public module at `crates/bbox-indexing/src/lib.rs:5`.
- Its live-relevant types and functions remain `pub(crate)` or private.
- `AcceptedPublicationGenerationV1` is at `accepted_publication_store.rs:481`.
- `AcceptedPublicationPointerV1` is at `accepted_publication_store.rs:515`.
- `AcceptedPublicationBuildInputV1` is at `accepted_publication_store.rs:540`.
- `AcceptedPublicationStorePaths` is at `accepted_publication_store.rs:614`.
- `prepare_accepted_publication_v1` builds one generation containing knowledge and gap manifests plus both normalized record sets.
- The one-generation construction and pointer assembly are at `accepted_publication_store.rs:1180-1229`.
- `VerifiedAcceptedPublicationSelectionV1` and `VerifiedAcceptedPublicationV1` are at `accepted_publication_store.rs:1601-1611`.
- `verify_selected_locked` is `pub(crate)` at `accepted_publication_store.rs:1632`.
- `verify_selected_from_pointer_locked` is private at `accepted_publication_store.rs:1644`.
- `rebind_pointer_attachment_locked` is `pub(crate)` at `accepted_publication_store.rs:1703`.
- There is no live startup reader and no advance function.
- The root `blackbox` package is a separate crate that depends on `bbox-indexing`. Direct calls to crate-private accepted store items cannot compile.
### 3.2 The legacy publisher path remains live
- `PublisherRefStore::elect_publisher` in `crates/bbox-indexing/src/publisher.rs:349` scans `ProjectRecord` paths.
- `AuthorizedPublisher` in `src/server/knowledge_lifecycle.rs:40-45` carries exactly `project_id`, `published_scope`, `branch_ref`, and `commit`. It has no filesystem-root field.
- `PublisherAuthorizationCache` in `knowledge_lifecycle.rs:95` is keyed by `PublishedScope`.
- `PUBLISHER_AUTHORIZATION_CACHE_TTL` is 250 milliseconds at `knowledge_lifecycle.rs:37`.
- `resolve_authorized_publisher` elects through records and the legacy ref store.
- Catalog mode must stop before this path. Bridge mode keeps it byte-identical.
### 3.3 Catalog scoped views are intentionally empty
- `session_knowledge_view` in `src/server/knowledge_view.rs:274` has a catalog scoped arm that reports the pending Phase 5 wiring.
- `cached_published_knowledge_snapshot` at `knowledge_view.rs:565` is scope-keyed and opens a live publisher root.
- `hydrate_published_snapshot` at `knowledge_view.rs:619` calls `hydrate_repo_recall_stats`.
- `hydrate_repo_recall_stats` reads `.bbox/local/knowledge-stats.json` from a project directory at `crates/bbox-knowledge/src/knowledge.rs:850-891`.
- `session_gap_view` and `cached_published_gap_snapshot` in `src/server/gap_view.rs` mirror the path-dependent shape.
- Remote accepted reads cannot use repo-local recall telemetry without acquiring an attachment, so they omit the advisory boost.
### 3.4 Overlay keys are correct but catalog authority input is missing
- `OverlayKey` remains `{ published_scope, checkout_id }` at `crates/bbox-knowledge/src/overlay.rs:169-173`.
- `OverlayStamp` already carries scope, checkout id, published ref, publisher commit, checkout head, merge base, and working fingerprint at `overlay.rs:175-184`.
- `recompute_overlay_result` at `overlay.rs:640` resolves the publisher commit, passes the publisher root as an alternate to merge-base and baseline reads, and reads the published map from the publisher repository.
- The gap overlay has the parallel key, stamp, and recompute path.
- `preserve_transient_if_latest` is bounded, but baseline unavailability after detach is a definitive structural state and must not be preserved as a transient.
### 3.5 The checkout lease substrate is complete
- `CheckoutAccessKind` is the closed nine-variant enum at `crates/bbox-indexing/src/checkout_access.rs:36-46`.
- The correct provenance variant is `ProvenanceNoteIo`.
- `CheckoutAccessErrorCode` is the closed sixteen-variant enum at `checkout_access.rs:277-296`.
- `ValidatedCheckoutLease` carries opened directory handles, logical identity, roots, operation kind, intent, and source lane.
- `CheckoutAccessBroker::revalidate` is at `checkout_access.rs:742`.
- `CheckoutAccessBroker::publication_guard` is at `checkout_access.rs:794`.
- The publication guard closes broker lifecycle mutation windows only for mutations that participate in that broker fence.
- Catalog detach does not participate, so D-033 item 1 remains.
- `CheckoutAccessObservations` persists only kind, source lane, and outcome.
- Its explicit no-high-cardinality invariant is at `checkout_access.rs:1365-1367`.
### 3.6 Remaining adapters are lease-shaped but legacy-keyed
- `FileProvider` is an active `InspectableEntityProvider` at `crates/bbox-providers/src/providers/file.rs:20-100`.
- `resolve_file` at `file.rs:102` starts from attached `ProjectRecord` rows.
- File scope discovery uses `PublisherConfigTreeRead` at `file.rs:138-154`.
- File content uses `RenderFileProvider` at `file.rs:157-200`.
- Graph blame and provenance selection in `src/tools/graph.rs` start from `ProjectRecord`.
- `acquire_provenance_projects` at `graph.rs:418-443` acquires `ProvenanceNoteIo` for every requested project and returns on the first failure.
- `bbox_provenance_export_plan` at `graph.rs:852-903` is the separate corpus-computation path, but it still checks `ProjectRecord` membership.
- `bbox_project_init` is in `src/tools/projects.rs:608-622`.
- `bbox_project_eject` is in `src/tools/projects.rs:953-970`.
- Catalog attach probes an explicit path off-lock before an attachment exists at `src/tools/project_catalog.rs:497-528`.
- `ArtifactWatchCarrier` and `ArtifactWatchAttachment` are at `crates/bbox-artifacts/src/watcher.rs:20-67`.
- The watcher carrier supports Selected and CheckoutId, but not AttachmentId.
- `ArtifactWatchAccess` is an existing trait at `watcher.rs:82-102`.
- `DaemonArtifactWatchAccess::with_discovery` already reacquires `ArtifactWatchDiscovery` and publication-fences event publication at `src/server/checkout_access.rs:175-231`.
- `ToolEdgeProjectAccess` at `crates/bbox-corpus-index/src/index/tool_edges.rs:51-60` embeds `ProjectRecord` plus validated roots.
- `bbox-indexing` depends on `bbox-corpus-index`; the reverse dependency does not exist and must not be introduced.
- The upper reindex layer builds tool-edge access from live leases at `crates/bbox-indexing/src/index/reindex.rs:299-315`.
- The same upper layer holds the publication guard through tool-edge sidecar publication at `reindex.rs:559-567`.
### 3.7 Existing error and health surfaces constrain the design
- `ProjectResolveError` already provides `error.project_attachment_required`, `error.project_attachment_ambiguous`, `error.project_capability_denied`, and `error.project_catalog_inactive` at `crates/bbox-corpus-core/src/project_selector.rs:159-195`.
- `BlackboxServer::err_text` returns text content and sets `CallToolResult.is_error = Some(true)` at `src/server/response.rs:72-80`.
- `CheckoutAccessHealth` at `checkout_access.rs:1348-1363` reports operation counters and active compatibility lanes, not project status.
- `doctor::checkout_access_section` at `src/doctor.rs:212` renders those counters.
- `BuiltFromStamp` in `crates/bbox-corpus-core/src/built_from.rs:15-30` already has `Published` and `CheckoutOverlay`. Phase 5 does not add variants.

## 4. Fixed decisions
### 4.1 Accepted publication is the sole catalog published authority
Decision: catalog knowledge and gap reads resolve project id to a verified accepted pointer and generation. They do not enter the legacy publisher path.
Rationale: the governing design names the strict accepted-publication store as durable authority and requires restart with zero attachments to serve the selected generation.
Rejected alternative: reuse the 250 millisecond authorization cache with a catalog constructor. That cache is scope-keyed, path-election-shaped, and cannot represent durable detached publication.
### 4.2 The live accepted API is a narrow public facade
Decision: add a public runtime facade in `bbox-indexing`. It exposes immutable verified content, content stamps, binding stamps, Current/Prior selection, status, establish, bind, and advance. It keeps codecs, raw pointer bytes, validated string constructors, lock guards, and private verification helpers inside the crate.
Rationale: the daemon is a separate crate and needs a stable contract, not visibility promotion of implementation details.
Rejected alternative: make every migration helper and lock type public. That enlarges the API and lets callers bypass the runtime invariants.
### 4.3 Content identity and binding identity are separate
Decision: the content stamp contains project id, accepted scope, full ref, accepted commit, generation id, and generation hash. The binding stamp contains attachment id, pointer SHA-256, and Current/Prior selection.
Rationale: rebind changes pointer bytes and attachment authority without changing accepted content.
Rejected alternative: one mixed stamp. It either evicts identical content on rebind or fails to carry the pointer CAS token.
### 4.4 Establish is explicit and pointer-absence-gated
Decision: the publisher advance surface accepts `Establish` and `Advance` modes. Establish requires pointer absence, named attachment, full ref, accepted commit, catalog scope, and complete dual-lane validation.
Rationale: migration may record `no_published_content_acknowledged` and create no pointer. Without establish, such a project can never begin publishing.
Rejected alternative: overload attachment-only bind. Bind cannot choose or validate content identity.
Before implementation, record this material choice as a new ledger entry under the operator-approved decision process.
### 4.5 Existing advance has a pointer-specific CAS
Decision: existing advance requires expected catalog epoch, expected generation id, and expected pointer SHA-256.
Rationale: catalog epoch does not serialize the independently replaced pointer.
Rejected alternative: catalog epoch alone. Two advances at one epoch could silently overwrite one another.
### 4.6 Generation preparation and install are off-lock
Decision: resolve Git, read committed sources, normalize, encode, write, and fsync one immutable dual-lane generation before acquiring the publication lock. The lock covers expected-pointer verification, catalog and attachment freshness recheck, atomic pointer replacement, read-back verification, and release.
Rationale: the governing concurrency invariant forbids holding a lock across filesystem walking or Git.
Rejected alternative: hold the publication lock from source resolution through generation fsync. That turns expensive I/O into a global publication critical section.
### 4.7 One generation contains both lanes
Decision: knowledge and gap validation produce one `AcceptedPublicationGenerationV1`. The pointer either names that complete generation or remains unchanged.
Rationale: the existing codec and D-014 exact-byte identity bind both lanes into one payload.
Rejected alternative: write separate knowledge and gap generations. That creates mixed-epoch states the current pointer cannot represent.
### 4.8 Prior fallback is read-only
Decision: if current verification fails and prior verification succeeds, reads serve Prior and health reports it. Establish, bind, and advance refuse with a repair-required domain error.
Rationale: writing through a damaged current pointer discards evidence needed for repair.
Rejected alternative: advance from Prior and silently overwrite the damaged current state.
### 4.9 Scope migration preserves old accepted truth until new-scope advance
Decision: the old pointer remains readable by project id with its old accepted scope and old `BuiltFromStamp::Published`. Health reports scope refresh required. Bind refuses during scope disagreement. Advance validates the catalog's current scope, creates a new generation at that scope, and retains the old pointer as prior.
Rationale: no accepted snapshot is relabeled, and attachment-only rebind cannot rewrite scope.
Rejected alternative: require the new commit to keep declaring the old pointer scope. That makes the governing publication bridge impossible to clear.
### 4.10 Overlay keys remain unchanged
Decision: catalog overlays remain keyed by `OverlayKey { published_scope, checkout_id }`. The catalog stamp may add accepted generation identity where it makes invalidation explicit.
Rationale: the governing design fixes the host-local overlay key. Freshness belongs in the stamp and eligibility check.
Rejected alternative: rekey by project, attachment, checkout, and generation. That changes a fixed identity contract without need.
### 4.11 Catalog overlays use a separate no-alternate entry point
Decision: add catalog-mode knowledge and gap recompute entry points that consume accepted published manifests plus checkout-local Git evidence. They cannot receive a publisher root or alternate object database.
Rationale: accepted publication supplies content truth but not Git ancestry, as fixed by [D-007](../../DECISION_LEDGER.md#d-007).
Rejected alternative: keep the current signature and pass a former publisher or peer as an alternate. That silently borrows ancestry.
### 4.12 Baseline unavailable is definitive structural degradation
Decision: a checkout missing accepted commit P or unable to prove merge base B produces `overlay_baseline_unavailable`. The transient preservation path cannot preserve a prior valid snapshot over this state.
Rationale: detach and missing objects are structural authority facts, not retryable I/O noise.
Rejected alternative: classify every Git failure as transient and preserve a stale overlay.
### 4.13 No new `BuiltFromStamp` variants
Decision: keep `BuiltFromStamp::Published` and `BuiltFromStamp::CheckoutOverlay` unchanged. Cache identity remains richer than response provenance.
Rationale: scope, ref, and commit already identify accepted published content, and bridge parity is simpler with no response-schema expansion.
Rejected alternative: add catalog-only variants before a fixture proves a representational gap.
### 4.14 Repo-local recall hydration is omitted from remote accepted reads
Decision: catalog accepted reads do not open `.bbox/local/knowledge-stats.json`.
Rationale: recall telemetry is advisory, repo-local, and not part of accepted durable truth.
Rejected alternative: acquire an attachment merely to restore a ranking boost. That breaks remote-only publication.
### 4.15 Tool-edge leases remain above `bbox-corpus-index`
Decision: replace `ToolEdgeProjectAccess.project: ProjectRecord` with pure project identity. Keep validated roots in the lower carrier. Keep the owning leases and publication guard in the upper reindex layer.
Rationale: the dependency direction is `bbox-indexing` to `bbox-corpus-index`.
Rejected alternative: put `Arc<ValidatedCheckoutLease>` in the lower crate. That creates a crate cycle.
### 4.16 Watchers use native attachment identity
Decision: add an AttachmentId variant to `ArtifactWatchAttachment`. Retain the `ArtifactWatchAccess` trait, registration-time discovery, event-time reacquisition, and publication guard.
Rationale: native attachment identity avoids Selected-ladder drift while preserving the existing operation-scoped authority boundary.
Rejected alternative: replace the trait with a lease-bearing data structure or cache a checkout path as authority.
### 4.17 Checkout observations stay closed
Decision: do not add project or attachment maps to `CheckoutAccessObservations`. Build per-project capability status in a separate bounded runtime projection.
Rationale: the durable observations are Phase 6 cut evidence with a closed key space. Synthetic denials would count operations that never occurred.
Rejected alternative: persist high-cardinality project states and active lease counts in the observation file.
### 4.18 Errors keep the existing MCP envelope
Decision: tool handlers return code-prefixed text through `err_text`. Reuse `ProjectResolveError` and `CheckoutAccessErrorCode` where exact. Add new codes only for accepted-publication and overlay domain failures.
Rationale: there is no structured MCP error envelope in the daemon.
Rejected alternative: declare that errors are "not strings" without defining and migrating a new wire shape.
### 4.19 Unregistered init remains a bootstrap exception
Decision: `bbox_project_init(path)` may initialize an unregistered absolute path. Once a selector resolves a catalog project, eject and every other daemon mutation require `RepositoryMutation`.
Rationale: attach needs identity-bearing config, so pre-attachment bootstrap must exist.
Rejected alternative: require an attachment before init. That is circular.
### 4.20 Provenance has three distinct contracts
Decision: `bbox_provenance_export_plan` remains corpus computation and drops its `ProjectRecord` membership check in catalog mode. Legacy export and import require `ProvenanceNoteIo` for every selected project and return typed refusal on failure.
Rationale: plan generation does not open Git notes, while legacy import/export do.
Rejected alternative: silently convert legacy all-project operations into partial success with skipped projects.
### 4.21 D-033 item 1 remains
Decision: Phase 5 retains the final bind or advance detach-at-swap residual documented in [D-033 item 1](../../DECISION_LEDGER.md#d-033). Health reports a detached or stale binding and status guides explicit repair.
Rationale: catalog detach does not take the publication lock and does not participate in the broker lifecycle fence.
Rejected alternative: claim the current publication guard already excludes catalog detach.
### 4.22 Bridge capability asymmetry remains
Decision: bridge read lanes keep their broad version-1 authority. Catalog lanes enforce recorded capabilities and degrade per operation, as fixed by [D-032](../../DECISION_LEDGER.md#d-032).
Rationale: version-1 records contain no capability bits and cannot truthfully derive them.
Rejected alternative: weaken catalog capability checks to preserve bridge grant behavior.

## 5. Phase 4 dependency contract
Phase 5 consumes four named Phase 4 outputs.
### 5.1 P4-A runtime record mode
Dependency: `RuntimeRecordMode::{BridgeV1, CatalogV2}` is store-owned data and catalog live writers emit only v2 records. See Phase 4 sections 4.9 and 4.10.
Use: accepted publication and project health are catalog-mode-scoped while bridge code retains its legacy stores and response bytes.
Fallback: if mode dispatch reaches the server through `ProjectAuthority` rather than a store-exposed mode, use the process-lifetime authority selected at startup. Do not infer mode from record shape.
### 5.2 P4-E post-commit observer
Dependency: after `ProjectCatalogStore::transact` durably publishes and releases locks, it emits committed epoch plus changed project ids. See Phase 4 section 4.5 and P4-E section 9.4.
Use: invalidate attachment-dependent selection, refresh project capability status, reconcile watcher registrations, mark scope bridges, and schedule overlay refresh.
Non-use: do not evict verified accepted content merely because an attachment row changed.
Fallback: route committed receipts from every catalog admin path through one `ProjectRuntimeInvalidator`, and run one bounded full catalog rescan at startup and after delivery failure. This fallback is correct but duplicates event plumbing and must not survive the exit gate without explicit review.
### 5.3 P4-D and P4-E reconciler ownership
Dependency: one project-keyed reconciler owns attachment-sensitive source transitions and re-reads authority before commit.
Use: publisher preparation refuses with existing lifecycle-busy semantics when the selected attachment cannot be leased or revalidated.
Fallback: if the reconciler's event vocabulary changes, classify only through `CheckoutAccessErrorCode::LifecycleBusy` at the publisher boundary. Do not depend on code-source internal event names.
### 5.4 P4-F pre-bind validation
Dependency: catalog and code-source recovery, relationship validation, and startup reducer feed complete before `CodeReadView` construction and listener bind. See Phase 4 P4-F section 10.1.
Use: insert global accepted-store open plus per-project accepted status scan after catalog open and before `CodeReadView` construction.
Failure policy: global accepted-store authority failure blocks bind. Per-project missing or corrupt publication marks that project's published capability unavailable while code search remains available.
Fallback: if P4-F moves the exact insertion point, preserve the ordering relation: catalog recovery first, accepted global open second, per-project scan third, read-view construction fourth, listener bind last.

## 6. Runtime contracts and data model
### 6.1 `AcceptedPublicationContentStamp`
Add a path-free public stamp in a contract-bottom crate or as an opaque public value returned by the `bbox-indexing` facade.
Fields:
- project id; accepted scope; full ref; accepted commit; generation id; generation hash
The stamp is immutable content identity. It contains no path, attachment id, pointer hash, or selection state.
### 6.2 `AcceptedPublicationBindingStamp`
Fields:
- project id; attachment id; pointer SHA-256; selected state: Current or Prior; catalog scope agreement state
The binding stamp is CAS and health identity. Rebind changes it without changing content.
### 6.3 `VerifiedAcceptedPublication`
The public immutable view exposes:
- `content_stamp()`; `binding_stamp()`; `knowledge_manifest()`; `knowledge_records()`; `gap_manifest()`; `gap_records()`; `counts()`
The value owns verified decoded content through `Arc`. Callers cannot mutate it or construct it without facade verification.
### 6.4 `AcceptedPublicationRuntime`
The narrow facade owns:
- `AcceptedPublicationStorePaths`; limits; current/prior verification; process-local verified content cache; per-project status cache; establish preparation and commit; bind; advance preparation and commit; pointer-specific compare-and-swap; protected generation-root inventory for GC
Public methods:
- `status(project_id)`; `load_verified(project_id)`; `prepare_publish(request, source)`; `commit_publish(prepared, freshness)`; `bind(request, proof)`; `invalidate_binding(project_id)`; `invalidate_content(project_id)`; `startup_scan(project_ids)`
Method names are implementation suggestions. The contract boundaries are fixed.
### 6.5 Publish request modes
`PublisherPublishMode`:
- Establish; Advance
Common request fields:
- project id; attachment id; full ref; expected catalog epoch; bounded audit reason; dry-run
Advance-only fields:
- expected generation id; expected pointer SHA-256
Establish requires both fields absent and pointer absence under the final lock.
### 6.6 Prepared publish artifact
The off-lock result contains:
- request identity; resolved catalog scope; resolved full ref; accepted commit; prepared single generation bytes; generation id; generation hash; encoded candidate pointer inputs; old pointer content and digest for Advance; attachment lease identity used during preparation
It contains no durable mutation receipt until commit succeeds.
### 6.7 Catalog overlay input
Add knowledge and gap catalog inputs containing:
- existing `OverlayKey`; accepted content stamp or generation id; canonical accepted published manifest and normalized map; checkout id; attachment id for health only; checkout head; merge base; working fingerprint; baseline committed map read from the checkout; working and untracked map read through the lease
The lower overlay crates compare maps. They do not discover another repository.
### 6.8 Project runtime status
One bounded in-memory status per catalog project:
- accepted state: Current, Prior, Missing, Corrupt; content stamp when verified; binding attachment id; binding attachment status; scope agreement or scope refresh required; advance availability; last pointer verification time; per-attachment capability availability; last overlay outcome per checkout; watcher registered state; cleanup debt or orphan-generation count if known
This is observational. The catalog, attachment store, and accepted pointer remain authority.
### 6.9 Native carriers
Add:
- `ArtifactWatchAttachment::AttachmentId`; native repository I/O target by attachment id; native provider checkout selection by attachment id; pure tool-edge project identity
Do not put host paths in logical carrier ids.

## 7. Transaction and cache mechanics
### 7.1 Catalog published read
1. Resolve catalog project identity.
2. Call `AcceptedPublicationRuntime::load_verified`.
3. Verify current pointer and generation.
4. If current fails, verify the bounded prior pointer.
5. Build content and binding stamps.
6. Reuse or install the content cache by content stamp.
7. Project knowledge or gap rows from accepted normalized records.
8. Intern the existing `BuiltFromStamp::Published` from accepted scope, full ref, and accepted commit.
9. Apply optional catalog overlay logic for Own or All.
10. Return diagnostics and runtime health.
No checkout lease, Git call, publisher election, TTL authorization, or recall sidecar access occurs.
### 7.2 Off-lock publish preparation
1. Resolve project and named attachment through the shared catalog resolver.
2. Pin expected catalog epoch.
3. Acquire native `PublisherConfigTreeRead`.
4. Resolve the requested full ref in that checkout.
5. Resolve one accepted commit.
6. Read committed project identity at that commit.
7. For Establish, require committed scope equal catalog current scope.
8. For normal Advance, require committed scope equal catalog current scope.
9. For scope-bridge Advance, allow the old pointer scope to differ and require the new commit to equal catalog current scope.
10. Capture exact bounded knowledge source bytes at the commit.
11. Capture exact bounded gap source bytes at the commit.
12. Build the old pointer as prior for Advance.
13. Call `prepare_accepted_publication_v1`.
14. Validate both lanes and one encoded generation.
15. Write the immutable generation under its content-derived id.
16. If the file already exists, require byte equality.
17. Fsync the generation file and parent directory.
18. Revalidate the checkout lease.
19. Return the prepared publish artifact.
No catalog lock or publication lock is held.
### 7.3 Pointer commit
1. Acquire the publication lock.
2. Re-read catalog snapshot.
3. Require expected catalog epoch.
4. Re-read the named attachment.
5. Require same project, Attached status, recorded `repo_knowledge`, expected scope, and prepared lease identity.
6. Revalidate the live checkout and full ref immediately before swap.
7. For Establish, require pointer absence.
8. For Advance, read current pointer bytes.
9. Require current generation id equal expected generation id.
10. Require current pointer digest equal expected pointer SHA-256.
11. Require current selection verify as Current, not Prior.
12. Require the prepared prior pointer equal the current pointer content.
13. Atomically replace the pointer.
14. Fsync the pointer directory.
15. Read back and verify pointer-generation agreement.
16. Release the publication lock.
17. Invalidate project binding status.
18. On Advance or Establish, invalidate accepted content and overlay caches.
19. Trigger knowledge and gap index convergence from the new accepted content.
The generation is already complete before step 1. A crash before pointer replacement leaves the old pointer. A crash after replacement finds the complete new generation.
### 7.4 D-033 item 1 residual
The final freshness recheck narrows but does not eliminate detach-at-swap. Catalog detach does not take the publication lock. If detach lands in the final window:
- accepted content remains valid; binding health becomes detached or stale; published reads continue; advance becomes unavailable; explicit bind repairs the attachment
No code path calls this corruption.
### 7.5 Rebind
1. Resolve the new attachment.
2. Acquire native `PublisherConfigTreeRead`.
3. Require current pointer verification.
4. Refuse Prior fallback.
5. Require pointer scope equal catalog current scope.
6. Require the new attachment contain accepted commit.
7. Acquire publication lock.
8. Recheck epoch and attachment status.
9. Recheck commit containment.
10. Call attachment-only pointer rebind.
11. Read back and verify.
12. Refresh binding health and watcher state.
Do not evict content cache or overlays solely because attachment id changed.
### 7.6 Scope-migration bridge
After catalog scope migration:
- project identity resolves to the new catalog scope; old accepted content remains readable by project id; response provenance retains the old accepted scope; runtime status reports scope refresh required; bind refuses because attachment-only rebind cannot change scope; advance prepares at the new catalog scope; the new pointer retains the old pointer as prior; successful advance clears the publication bridge
### 7.7 Cache structure
Catalog-only caches:
- content stamp to immutable accepted content; overlay key to stamped knowledge overlay; overlay key to stamped gap overlay; project id to binding and health status
Bridge-only caches:
- scope-keyed `PublisherAuthorizationCache`; existing published knowledge and gap caches; existing overlay stores and publisher alternate behavior
Invalidation:
- rebind refreshes binding status only; attach/detach refreshes attachment selection, watcher state, and capability status; advance changes content key and invalidates project overlays; scope migration marks scope bridge but preserves old content; catalog observer delivery failure triggers one bounded rescan
### 7.8 Generation retention
Expose current, prior, pinned-read, and in-flight generation ids as GC roots. Unreferenced content-addressed generation files remain safe after crash. Storage maintenance may collect only after a fresh protected-root read and bounded retention policy. Pointer mutation does not perform an unbounded generation-directory walk.

## 8. Milestone spine
Each milestone is independently committable and cluster-verifiable. Every milestone keeps bridge parity green.
### P5-A: Public accepted-publication runtime facade
Ownership:
- `crates/bbox-indexing/src/accepted_publication_store.rs`; `crates/bbox-indexing/src/lib.rs`; `src/server/state.rs`; startup integration point from Phase 4 P4-F
Dependencies:
- Phase 1 accepted generation and pointer codec; Phase 2 catalog authority; Phase 4 runtime mode and pre-bind ordering
Mechanics:
1. Add public content stamp, binding stamp, immutable verified view, status, and runtime facade.
2. Keep raw codecs and lock guard private.
3. Add Current/Prior status.
4. Add global store open and bounded project scan.
5. Insert scan before `CodeReadView` construction.
6. Treat missing pointer as project publication unavailable.
7. Treat corrupt current with valid prior as Prior.
8. Treat corrupt current and prior as project publication corrupt.
9. Block route bind only on global store authority failure.
10. Add protected generation-root inventory.
Bridge parity:
- no bridge caller constructs the runtime; no existing response changes; no new `BuiltFromStamp` variant
Verification:
- valid Current; valid Prior fallback; missing pointer; corrupt current; corrupt current and prior; pointer/generation field mismatch; generation hash mismatch; bounded limits; global store failure blocks startup; one corrupt project does not block another; zero checkout acquisitions
Gate:
- pinned format check; targeted nextest for `bbox-indexing`; cluster verification on the pushed milestone ref
### P5-B: Catalog published knowledge and gap views
Ownership:
- `src/server/knowledge_view.rs`; `src/server/gap_view.rs`; `src/server/knowledge_lifecycle.rs`; `src/server/state.rs`
Dependencies:
- P5-A; Phase 4 post-commit observer
Mechanics:
1. Replace the catalog knowledge empty branch with accepted generation reads.
2. Replace the catalog gap empty branch.
3. Add content-stamp-keyed caches.
4. Project accepted knowledge records into `PublishedKnowledgeSnapshot`.
5. Project accepted gap records into `PublishedGapSnapshot`.
6. Use existing `BuiltFromStamp::Published`.
7. Skip `authorize_publisher`.
8. Skip `with_authorized_publisher_root`.
9. Skip recall-stat hydration.
10. Return bounded diagnostics for Missing, Prior, Corrupt, and scope bridge.
11. Keep unscoped catalog reads keyed by project identity.
12. Keep bridge cache and TTL unchanged.
Bridge parity:
- `AuthorizedPublisher` remains the four-field bridge value; `PublisherRefStore` and `elect_publisher` remain bridge-only; bridge published rows and ordering remain byte-identical
Verification:
- remote-only published knowledge; remote-only published gaps; restart serves G1; detach preserves accepted rows; Prior response and status; missing pointer produces unavailable diagnostic; corrupt project isolation; content cache hit and advance invalidation; scope migration returns old stamped scope; no checkout, Git, or recall sidecar access
Gate:
- targeted knowledge/gap tests with `DenyCheckoutAccess`; bridge fixture comparison; cluster verification
### P5-C: Publisher establish, bind, and advance
Ownership:
- `crates/bbox-indexing/src/accepted_publication_store.rs`; `crates/bbox-indexing/src/project_catalog_admin.rs`; `src/tools/project_catalog.rs`; `src/server/state.rs`
Dependencies:
- P5-A; P5-B; Phase 4 catalog observer and lifecycle semantics
Tool surface:
- existing `bbox_project_publisher_bind`; new `bbox_project_publisher_advance`; new read-only `bbox_project_publisher_status`
Bridge refusal:
- `error.project_catalog_inactive`
Mechanics:
1. Record the Establish material choice in a new ledger entry before code.
2. Add Establish and Advance request modes.
3. Keep bind parameters and attachment-only semantics unchanged.
4. Add pointer-specific expected generation and SHA-256 fields to Advance.
5. Prepare Git and source content off-lock.
6. Write one dual-lane generation off-lock.
7. CAS one pointer under the publication lock.
8. Preserve old pointer as prior for Advance.
9. Require pointer absence for Establish.
10. Refuse Prior mutation.
11. Handle scope bridge through new-scope Advance.
12. Refresh status and caches by content/binding split.
13. Emit bounded audit log and receipt.
14. Preserve D-033 item 1 health.
Verification:
- initial Establish; concurrent Establish; normal Advance; concurrent Advance at one catalog epoch; pointer digest conflict; expected generation conflict; stale catalog epoch; ref movement between preparation and swap; attachment detach during preparation; detach in final swap window yields stale binding health; scope mismatch; scope bridge advance; bind preserves all content fields; bind refuses Prior; dry-run writes nothing; crash before generation install; crash after generation install before pointer swap; crash after pointer swap; read-back verification failure
Gate:
- targeted accepted store and admin nextest; fault-injection suite; bridge tool refusal check; cluster verification
### P5-D: Catalog overlay baseline path
Ownership:
- `crates/bbox-knowledge/src/overlay.rs`; `crates/bbox-gaps/src/overlay.rs`; `src/server/knowledge_view.rs`; `src/server/gap_view.rs`
Dependencies:
- P5-B accepted views; P5-C content invalidation
Mechanics:
1. Keep existing overlay key types.
2. Add catalog recompute entry points.
3. Input accepted manifests and normalized published maps.
4. Acquire native `KnowledgeGapOverlayRead`.
5. Require selected checkout object database contain accepted commit P.
6. Compute `B = merge_base(H, P)` in that checkout only.
7. Read baseline at B from that checkout.
8. Capture committed head and working/untracked maps through the lease.
9. Build stamp from scope, checkout id, accepted commit or generation, H, B, and fingerprint.
10. Revalidate lease and accepted content identity before cache publication.
11. Treat missing P or B as definitive baseline unavailable.
12. `published` ignores overlay failure.
13. `own` returns provisional overlay unavailable.
14. `all` omits only failed peers and reports each reason.
15. Preserve bridge recompute byte-identically.
Degradation reasons:
- overlay baseline unavailable; checkout identity mismatch; attachment not found; attachment inactive; capability denied; lifecycle busy; unsafe or invalid root; accepted content changed during recompute
Verification:
- publisher detached; peer containing P succeeds; peer missing P fails structurally; no merge base; dirty tracked file; untracked file; deletion; invalid local content; head changes during capture; fingerprint changes during capture; accepted generation advances during capture; Prior accepted content; Own strict failure; All peer omission; Published always serves accepted content; no publisher alternate argument exists on catalog entry point
Gate:
- targeted overlay nextest for knowledge and gaps; bridge overlay parity; cluster verification
### P5-E: Read adapter conversion
Ownership:
- `crates/bbox-providers/src/providers/file.rs`; `src/tools/graph.rs`; `src/tools/render.rs`; `src/server/repo_io.rs`; `crates/bbox-provenance/src/lib.rs` verification only
Dependencies:
- P5-A project publication status; Phase 2 shared resolver; existing checkout broker
File provider:
1. Resolve project identity through the shared resolver.
2. Resolve session, explicit attachment, default, or unique-base selection.
3. Use catalog scope directly rather than a preliminary publisher-config lease.
4. Acquire `RenderFileProvider` for working-tree content.
5. Keep absolute path matching limited to active attachment metadata.
6. Never scan `ProjectRecord::canonical_path` in catalog mode.
7. Return attachment-required, ambiguous, capability-denied, stale, or unsafe path codes through `err_text`.
Blame:
1. Resolve project id and normalized relative path from corpus identity.
2. Select one attachment through the catalog resolver.
3. Acquire `Blame`.
4. Require the selected repository contain the requested commit or snapshot.
5. Refuse arbitrary attachment HEAD blame.
Render:
1. Resolve project identity before target path.
2. Acquire `RenderFileProvider` with Write intent.
3. Remove catalog fallback through `ProjectRecord::canonical_path`.
4. Global render remains attachment-free.
Provenance:
1. Replace catalog `ProjectRecord` membership checks with project identity.
2. Keep `bbox_provenance_export_plan` corpus computation.
3. Require `ProvenanceNoteIo` for every selected legacy Git-note export/import project.
4. Return on the first typed refusal.
5. Do not invent partial skipped-project output.
Repo knowledge/gap reads:
1. Add native attachment-id repository carriers.
2. Acquire `KnowledgeGapOverlayRead`.
3. Keep carrier ids path-free.
4. Keep display paths non-authoritative.
Verification:
- relative file with session attachment; relative file with default selection; unique base selection; ambiguous attachment; absolute file under active attachment; stale ledger path does not grant authority; traversal and symlink refusal; blame commit present and missing; render attached and remote-only; provenance plan with catalog identity; provenance export/import typed refusal; repo read capability denial
Gate:
- targeted provider, graph, render, provenance, and repo-I/O tests; no raw catalog path open before lease; cluster verification
### P5-F: Mutation, watcher, repo-I/O, and tool-edge conversion
Ownership:
- `src/tools/projects.rs`; `src/tools/project_catalog.rs`; `src/server/repo_io.rs`; `crates/bbox-artifacts/src/watcher.rs`; `src/server/checkout_access.rs`; `crates/bbox-corpus-index/src/index/tool_edges.rs`; `crates/bbox-indexing/src/index/reindex.rs`
Dependencies:
- P5-E resolver and error pattern; Phase 4 post-commit observer
Mutation:
1. Preserve unregistered `bbox_project_init(path)` bootstrap.
2. For catalog-targeted init follow-up, eject, schema markers, and repo writes, acquire `RepositoryMutation`.
3. Hold publication guard through durable repo writes and central-store publication.
4. Remote-only returns attachment-required.
5. Capability-disabled returns capability-denied.
Watcher:
1. Add `ArtifactWatchAttachment::AttachmentId`.
2. Construct catalog registrations from active attachment ids with `artifact_watching`.
3. Reconcile registrations after catalog commit events.
4. Remove detached or relocated registrations idempotently.
5. Reacquire `ArtifactWatchDiscovery` for every event.
6. Keep `ArtifactWatchAccess` unchanged as the authority trait.
7. Keep durable artifact metadata when no watcher exists.
Repo I/O:
1. Replace Selected/Checkout-only base carriers with native attachment targets in catalog mode.
2. Preserve bridge carrier encoding.
3. Keep read and write kind separation.
4. Revalidate before publication.
Tool edges:
1. Replace lower `ProjectRecord` with pure project id.
2. Keep `local_root` and optional `git_root` as upper-validated ephemeral roots.
3. Keep `bbox-corpus-index` free of `bbox-indexing`.
4. Construct lower access only from active `LocalProjectWalk` leases.
5. Keep those leases alive for the reindex pass.
6. Hold publication guard through sidecar publish.
7. Emit project id plus normalized relative anchor.
8. Diagnose unresolved remote path events and never re-id them.
Verification:
- bootstrap init without catalog; catalog eject remote-only; mutation capability denial; watcher startup by attachment id; duplicate observer event idempotence; detach event publishes nothing; relocation replaces registration exactly once; no-capability attachment installs no watcher; tool edge attached path; tool edge remote diagnostic; tool edge lease revalidation failure; no lower crate dependency cycle; sidecar publish under guard
Gate:
- targeted artifacts, project tools, repo-I/O, and indexing tests; crate dependency acceptance check; concurrency lint; cluster verification
### P5-G: Capability-specific health and invalidation
Ownership:
- `src/doctor.rs`; `src/server/state.rs`; `src/server/resolver_compat.rs`; accepted runtime status; Phase 4 observer consumer
Dependencies:
- P5-A through P5-F; Phase 4 P4-E observer
Mechanics:
1. Keep `CheckoutAccessHealth` fields and durable counters unchanged.
2. Add separate bounded `ProjectRuntimeStatus`.
3. Add accepted-publication doctor section.
4. Add publisher binding and advance-availability section.
5. Add per-checkout overlay-baseline section from last recompute outcomes.
6. Add capability availability by attachment from catalog bits.
7. Add watcher registration state.
8. Add scope bridge and Prior fallback findings.
9. Add read-only `bbox_project_publisher_status`.
10. Keep health path-free.
11. Do not synthesize denied counts for never-attempted operations.
12. Reconcile attachment-dependent status from post-commit events.
13. Preserve accepted content cache on detach and rebind.
14. Invalidate content and overlays on advance.
15. Trigger bounded rescan on observer delivery failure.
Health states:
- accepted current; accepted prior; accepted missing; accepted corrupt; binding attached; binding detached; binding scope refresh required; advance available; advance attachment required; advance capability denied; overlay fresh; overlay baseline unavailable; overlay stale; watcher active; watcher unavailable
Verification:
- remote-only current publication healthy; remote-only checkout capabilities unavailable; Prior fallback degraded; missing pointer unavailable; corrupt project isolated; scope bridge stale; detach preserves content status; rebind changes binding only; advance changes content; duplicate observer delivery; observer rescan fallback; no path in health serialization; observation snapshot unchanged
Gate:
- targeted doctor/status tests; observation schema fixture unchanged; cluster verification
### P5-H: Exit proof and Phase 6 handoff
Ownership:
- facade external-consumer tests; ignored migrated-root producer; bridge parity harness; static acceptance scripts; live bootsmoke driver
Dependencies:
- P5-A through P5-G
Mechanics:
1. Extend the D-030 facade-produced migrated root.
2. Add remote-only accepted project.
3. Add no-pointer project.
4. Add Prior fallback project.
5. Add corrupt current and prior project.
6. Add scope-migration bridge project.
7. Add peer with accepted commit P.
8. Add peer without P.
9. Add all-capabilities attachment.
10. Add repo-knowledge-only attachment.
11. Add no-capability attachment.
12. Add bridge fixture producer and canonical verifier.
13. Make runtime denial probes blocking.
14. Make static ownership lint blocking.
15. Make checkout-open call-site audit blocking.
16. Produce Phase 6 deletion inventory.
Verification:
- three-clause exit proof in section 14; full fault matrix in section 13; bridge parity; isolated catalog bootsmoke; restart convergence; no shared service mutation
Gate:
- `scripts/fmt.sh --check`; targeted nextest while iterating; `cargo nextest run --workspace --profile full` on the cluster; clippy and concurrency lint on the cluster; exact-ref verification

## 9. Governing section 14 adapter conversion table
| Surface | CheckoutAccessKind | Capability bit | Typed refusal | No-attachment degradation |
|---|---|---|---|---|
| Local source walker | `LocalProjectWalk` | `local_code_source` | `error.project_attachment_required` or `error.project_capability_denied` | source unavailable or retained last-good collected view |
| Repo knowledge/gap publisher | published read: none; bind/advance: `PublisherConfigTreeRead`; overlays: `KnowledgeGapOverlayRead` | `repo_knowledge` | `error.project_attachment_required`, `error.project_capability_denied`, or overlay domain error | accepted published content remains; advance and Own unavailable; All omits unavailable peers |
| Git history | `GitHistory` | `git_history` | `error.project_attachment_required` or `error.project_capability_denied` | no current-file overlay; stale commit docs remain labeled |
| Blame | `Blame` | `blame` | `error.project_attachment_required`, `error.project_capability_denied`, or commit mismatch | no blame; corpus and provenance remain |
| Render/file provider | `RenderFileProvider` | `render_output` | `error.project_attachment_required` or `error.project_capability_denied` | no project render or working-tree file read |
| Provenance note import/export | `ProvenanceNoteIo` | `provenance_note_io` | `error.project_attachment_required` or `error.project_capability_denied` | plan generation remains corpus-only; Git-note I/O refuses |
| Init/eject/mutation/refactor | `RepositoryMutation` | `repo_mutation` | `error.project_attachment_required`, `error.project_capability_denied`, or write-gate denial | unregistered init bootstrap remains; catalog-targeted mutation refuses |
| Artifacts/watchers | `ArtifactWatchDiscovery` | `artifact_watching` | no watcher plus bounded capability health | durable artifact metadata remains; filesystem discovery stops |
| Tool/transcript edges | `LocalProjectWalk` | `local_code_source` | bounded `unresolvable_path_event` diagnostic | transcript corpus remains; path edge is skipped and never re-id'd |
The capability mapping is closed. `PublisherConfigTreeRead` and `KnowledgeGapOverlayRead` both ride `repo_knowledge`, as fixed by D-032. No new access kind is added.

## 10. Typed error and degradation vocabulary
### 10.1 Existing resolver codes
- `error.project_selector_unknown`; `error.project_selector_ambiguous`; `error.project_scope_unknown`; `error.project_attachment_required`; `error.project_attachment_ambiguous`; `error.project_capability_denied`; `error.project_catalog_inactive`
### 10.2 Existing checkout codes
- `attachment_not_found`; `attachment_inactive`; `project_mismatch`; `selector_mismatch`; `checkout_identity_mismatch`; `scope_mismatch`; `capability_denied`; `intent_denied`; `conservative_path_gate_denied`; `invalid_root`; `unsafe_relative_path`; `write_intent_required`; `lifecycle_busy`; `denied_by_test_probe`; `observation_unavailable`
### 10.3 Accepted-publication domain codes
Reuse existing:
- `error.accepted_publication_missing`; `error.accepted_publication_invalid_generation`; `error.accepted_publication_invalid_pointer`; `error.accepted_publication_invalid_id`; `error.accepted_publication_byte_limit`
Add:
- `error.accepted_publication_pointer_conflict`; `error.accepted_publication_repair_required`; `error.accepted_publication_scope_advance_required`; `error.accepted_publication_ref_moved`; `error.accepted_publication_global_store_unavailable`
### 10.4 Overlay domain codes
- `error.provisional_overlay_unavailable`; `error.overlay_baseline_unavailable`; `error.overlay_snapshot_stale`; `error.overlay_accepted_content_changed`
### 10.5 Mapping rules
- tool errors remain `err_text` with one stable code prefix; do not add duplicate `ProjectResolveError` variants for every checkout code; Published never returns an overlay error; Own returns the exact overlay domain error; All returns accepted content plus bounded `degraded.overlays`; legacy provenance import/export returns the first typed refusal; file, blame, render, and mutation do not translate missing attachment into project-not-found; diagnostics include project id and attachment id when known; diagnostics omit absolute paths and unbounded raw Git errors

## 11. Bridge parity contract
Bridge mode keeps:
- `ProjectRegistry`; `ProjectRecord`; `PublisherRefStore`; `PublisherRefStore::elect_publisher`; `resolve_authorized_publisher`; `AuthorizedPublisher` with its existing four fields; scope-keyed `PublisherAuthorizationCache`; 250 millisecond TTL; live publisher-root knowledge and gap loading; repo-local recall hydration; existing overlay key and publisher-alternate recompute; existing `BuiltFromStamp` variants and bytes; legacy watcher Selected and CheckoutId carriers; legacy repository I/O carrier encoding; legacy adapter selector behavior; version-1 broad read authority
Catalog-only additions are selected by process authority. No catalog API infers mode from ids, paths, or record shape.
New catalog-only tools:
- `bbox_project_publisher_advance`; `bbox_project_publisher_status`
On bridge they return `error.project_catalog_inactive`.
Parity fixture covers:
- published knowledge and gaps; Own and All overlays; file provider; blame; render; provenance plan and Git-note tools; project list; doctor existing sections; checkout observation snapshot
Allowed bridge-visible additions:
- dormant code and types; catalog-inactive refusal from new tools; empty catalog runtime state not serialized into existing responses
Any other bridge output change requires a new explicit decision.

### 2026-09-05 MCP response amendment

The operator-authorized MCP contract remediation deliberately changes the
presentation of existing bridge responses. Gap lists use bounded summary pages,
explicit detail and continuation, and deterministic created-time/id ordering.
The published, Own, and All view selection, authorization, and BuiltFromStamp
semantics remain the same. The common JSON transport uses compact encoding;
project-list JSON values remain identical. The four affected parity rows
(`published_gaps`, `own_gaps`, `all_gaps`, `project_administration`) were inspected
individually and updated. Other rows and normalization rules remain frozen.
See [the MCP audit](../surfaces/mcp/mcp-response-and-contract-audit.md).

## 12. Cache, invalidation, and health matrix
| Event | Accepted content cache | Binding status | Overlay cache | Watchers | Capability status |
|---|---|---|---|---|---|
| attach | preserve | refresh | preserve unless selection changes | add eligible native attachment | refresh |
| detach | preserve | mark detached if bound | invalidate detached checkout only | remove attachment registration | refresh |
| rebind | preserve | replace binding stamp | preserve content-derived overlays | reconcile old/new watcher relation | refresh |
| advance | replace by new content stamp | replace | invalidate project overlays | preserve registrations | refresh |
| scope migration | preserve old content | mark scope refresh required | invalidate scope-sensitive eligibility | reconcile attachment metadata | refresh |
| Prior fallback | preserve prior content | mark Prior and repair required | use only if accepted stamp matches | unchanged | refresh |
| pointer corruption | preserve last verified cache only for in-flight reads | corrupt | refuse new overlay publication | unchanged | refresh |
| observer delivery failure | no immediate eviction | stale health | no immediate eviction | bounded full reconcile | bounded full refresh |
Health never creates authority. Cache entries never authorize paths.

## 13. Test and validation plan
### 13.1 Accepted runtime unit tests
- current pointer and generation; prior fallback; current and prior corrupt; missing pointer; invalid pointer schema; invalid generation schema; project mismatch; scope mismatch; ref mismatch; commit mismatch; generation id mismatch; generation hash mismatch; pointer hash mismatch; file-count limit; source-file byte limit; lane-byte limit; entry-count limit; total generation limit; deterministic generation id; content and binding stamp separation; protected generation roots
### 13.2 Publish transaction tests
- Establish success; Establish pointer already exists; two concurrent Establish requests; Advance success; two concurrent Advance requests at one catalog epoch; expected generation mismatch; expected pointer SHA mismatch; stale catalog epoch; attachment unknown; attachment inactive; attachment project mismatch; capability denied; full ref missing; full ref moves after preparation; accepted commit missing; scope mismatch; scope bridge success; Prior mutation refusal; one-lane validation failure; generation existing with equal bytes; generation existing with unequal bytes; generation write failure; generation fsync failure; pointer temporary write failure; pointer rename failure; pointer directory fsync failure; read-back verification failure; crash before generation install; crash after generation install; crash before pointer swap; crash after pointer swap; D-033 item 1 final detach window
### 13.3 Catalog published view tests
- remote-only knowledge Published; remote-only gaps Published; restart before first advance serves G1; detach preserves accepted rows; no pointer; Prior fallback; corrupt publication; one corrupt project among healthy projects; unscoped multi-project query; project-id selector; typed scope selector; content cache hit; rebind does not evict content; advance changes content key; scope bridge old stamp; no new `BuiltFromStamp` variant; no checkout acquisition; no publisher store open; no Git call; no recall sidecar read
### 13.4 Overlay tests
- publisher attached; publisher detached; peer contains P; peer lacks P; merge base absent; checkout at P; checkout ahead; checkout behind; checkout diverged; dirty tracked knowledge; dirty tracked gap; untracked knowledge; untracked gap; deleted file; invalid working content; head changes during capture; working fingerprint changes; lease revalidation fails; attachment detaches during capture; accepted content advances during capture; Prior accepted generation; Own error; All omitted peer; Published unaffected; no publisher alternate; structural state not transient-preserved
### 13.5 Adapter tests
For every section 9 row:
- explicit attachment; session attachment; operator default; single active attachment; unique base; ambiguous selection; no attachment; inactive attachment; capability denied; identity mismatch; scope mismatch; safe relative path; traversal refusal; symlink refusal; revalidation failure; publication guard for writes
Additional:
- blame requested commit present; blame requested commit missing; global render without lease; provenance plan without Git-note lease; legacy provenance all-project failure is not partial; unregistered init bootstrap; native watcher attachment id; watcher relocation; remote tool-edge diagnostic
### 13.6 Health tests
- operation counter schema unchanged; no project ids in observation snapshot; accepted Current; accepted Prior; accepted Missing; accepted Corrupt; binding Attached; binding Detached; scope refresh required; advance available; advance attachment required; overlay fresh; overlay baseline unavailable; watcher active; watcher unavailable; observer duplicate; observer failure rescan; health contains no paths
### 13.7 Fault injection
Stable failpoints:
- accepted global store open; pointer read; current generation read; prior generation read; committed source capture; generation encode; generation create; generation write; generation fsync; generation parent fsync; publication lock acquire; pointer expected-token check; final attachment freshness check; pointer temporary write; pointer fsync; pointer rename; pointer parent fsync; installed read-back; content invalidation; overlay invalidation; index convergence request; watcher reconciliation
Every crash test restarts and observes a complete old or complete new pointer. No test permits mixed knowledge and gap epochs.
### 13.8 Fixture strategy
The isolated migrated root is produced by the ignored facade-driving test and verified by the CLI, following [D-030](../../DECISION_LEDGER.md#d-030).
Fixture projects:
- remote-only with valid G1; attached with valid G1; attached peer containing P; attached peer missing P; Prior fallback; no pointer after no-content acknowledgement; corrupt current and prior; scope-migration publication bridge; all capabilities; repo-knowledge only; no capabilities; watcher-capable attachment; non-Git or LegacyLocal bootstrap where applicable
### 13.9 Validation commands
During implementation use the narrowest relevant nextest package expression. Use the project-pinned formatter:
```bash
scripts/fmt.sh --check
```
Mid-cycle workspace gate:
```bash
cargo nextest run --workspace
```
Fold gate on the cluster:
```bash
cargo nextest run --workspace --profile full
cargo clippy --workspace --all-targets
scripts/lint-concurrency.sh
```
No plan milestone is complete until its pushed ref passes the applicable cluster gate.

## 14. Three-clause exit-gate proof
All three proof mechanisms in clause 2 are blocking.
### 14.1 Clause 1: no corpus-only request requires `ProjectRecord`
Create a catalog-only facade with:
- catalog store; accepted-publication runtime; `RecordlessProjectRecordsProvider`; `DenyCheckoutAccess`; no version-1 registry; no legacy publisher store access
Run:
- lexical search; hybrid search; graph inspect; graph path traversal; evidence bundle; entity-ref resolution; project-file provider; storage GC; collected activation and rebuild; published knowledge; published gaps; provenance export plan
The denial seam is field-level, not method-level. `ProjectRecordsProvider` exposes one method, `records_snapshot`, and `ProjectRecordsSnapshot` carries two distinct views: `records` is the attached-only path-bearing compatibility rows, and `corpus_project_ids` is the complete catalog project-id set that seeds corpus identity surfaces (`crates/bbox-corpus-core/src/project_record.rs:324-337`). Panicking on `records_snapshot` would deny both views at once and kill the very corpus paths this clause must prove: schema rebuild and the collected-activation pass reach the id set through `records_provider.records_snapshot().corpus_project_ids` (`crates/bbox-corpus-index/src/index/mod.rs:362` and `:436`), and `src/server/open.rs:711` seeds the edge registered-project set from the same field.

`RecordlessProjectRecordsProvider` therefore returns a live snapshot whose `corpus_project_ids` is the full catalog set derived through the catalog-records projection, whose `authority_epoch` is the catalog epoch, and whose `records` is empty with `omitted_catalog_count` equal to the catalog project count. Empty `records` is the stronger proof: a panic proves only that the accessor was never called, while an empty attached-row view proves no corpus-only path derives behavior from `ProjectRecord` content, and any path that still did would surface a typed refusal or an observable empty result rather than passing by luck.

The checkout authority panics on lease acquisition. Every listed operation succeeds or returns only its content-domain status, and the acceptance test additionally asserts that each one is byte-identical to the same operation run against a provider whose `records` is fully populated. That equality is what forbids silent dependence on the attached-row view.
### 14.2 Clause 2: every remaining checkout open is lease-counted
#### Proof A: runtime denial probe
Install `DenyCheckoutAccess`. Exercise every adapter in the nine-row table. Each checkout-backed operation returns its typed refusal or documented degradation before raw filesystem or Git access. Every corpus-only operation keeps the checkout observation sequence unchanged.
#### Proof B: static ownership lint
Add a blocking acceptance script that rejects, in catalog runtime paths:
- new `ProjectRecord` imports; `canonical_path` reads; direct checkout-root `std::fs`; direct checkout-root Git process calls; `PublisherRefStore` or `PublisherAuthorizationCache`; watcher Selected carriers; lower tool-edge `ProjectRecord`; reverse `bbox-corpus-index` to `bbox-indexing` dependency; new `BuiltFromStamp` variants; project or attachment fields in checkout observations
Allowlist:
- bridge-only; migration-only; explicit compatibility projection; tests
Every allowlisted row carries a Phase 6 deletion or retention reason.
#### Proof C: checkout-open call-site audit
Enumerate every call to:
- `CheckoutAccessBroker::acquire`; `acquire_selected_project_access`; `with_selected_project_access`; file-provider lease helpers; repository I/O authority; watcher discovery authority; tool-edge upper lease construction
For each site record:
- project selector source; attachment selector source; access kind; capability bit; intent; revalidation point; publication guard if writing; typed refusal; remote-only degradation; bridge disposition
The audit is checked into the acceptance test or script input. Unclassified call sites fail.
### 14.3 Clause 3: remote-only projects degrade per capability
Exercise every table row against a project with valid accepted content and zero attachments.
Expected:
- local source walker retains last-good collected view or reports unavailable; published knowledge and gaps serve accepted content; publisher advance returns attachment-required; Own returns provisional overlay unavailable; All returns accepted content plus degraded peers; Git current overlay unavailable, stale history remains labeled; blame returns attachment-required; render/file returns attachment-required; provenance plan succeeds; provenance Git-note I/O returns attachment-required; catalog-targeted mutation returns attachment-required; no watcher is installed; artifact metadata remains; tool path event is diagnosed and never re-id'd; project capability status reports availability without inventing denied acquisition counts
### 14.4 Bridge parity proof
Replay canonical bridge fixtures for:
- publisher authorization; published views; overlays; file provider; blame; render; provenance; project administration; watcher behavior; checkout observations; doctor existing sections
New catalog tools refuse with `error.project_catalog_inactive`. No existing bridge response field or ordering changes.

## 15. Risks and defect closures
### Risk 1: generation work leaks into the publication critical section
Closure: API separates `prepare_publish` from `commit_publish`. Tests assert Git/source callbacks cannot run while the publication lock is held.
### Risk 2: catalog epoch is mistaken for pointer CAS
Closure: Advance requires expected generation id and pointer SHA-256. Concurrent-advance tests share one catalog epoch.
### Risk 3: Establish silently overwrites a pointer
Closure: Establish has no expected pointer token and requires absence inside the lock. Presence is a pointer-conflict error.
### Risk 4: scope migration cannot clear its publication bridge
Closure: Advance validates the catalog current scope rather than requiring the old pointer scope. The old pointer becomes Prior.
### Risk 5: Prior fallback becomes writable
Closure: the runtime status type marks Prior mutation-refusing. Bind, Establish, and Advance all check it.
### Risk 6: rebind evicts identical content
Closure: content and binding stamps are separate. Rebind invalidates only binding status.
### Risk 7: overlay cache identity drifts from governing design
Closure: keep `OverlayKey` unchanged and put freshness in the stamp.
### Risk 8: detached overlay borrows publisher ancestry
Closure: catalog recompute has no publisher-root parameter. Static lint rejects catalog calls to the bridge entry point.
### Risk 9: definitive baseline failure is masked as transient
Closure: catalog result uses a structural unavailable variant excluded from transient preservation.
### Risk 10: lower tool-edge crate depends upward
Closure: crate dependency acceptance check and pure lower carrier.
### Risk 11: watcher selection drifts after attachment changes
Closure: native AttachmentId carriers plus post-commit reconciliation and event-time reacquisition.
### Risk 12: capability health corrupts observation semantics
Closure: project status is separate and observations remain byte-compatible.
### Risk 13: error taxonomy invents an unsupported envelope
Closure: all tool tests assert stable code-prefixed `err_text`.
### Risk 14: remote published reads reopen checkout state for recall stats
Closure: catalog view path has no recall hydration call and runs under `DenyCheckoutAccess`.
### Risk 15: D-033 item 1 is accidentally described as fixed
Closure: status and tests name the residual explicitly. No acceptance criterion claims mutual exclusion with detach.
### Risk 16: legacy provenance becomes silently partial
Closure: mixed attached/remote request test expects the first typed refusal.
### Risk 17: static proof misses runtime opens
Closure: runtime denial, static lint, and call-site audit are all blocking.
### Risk 18: bridge response changes through shared types
Closure: no new `BuiltFromStamp` variants, bridge caches unchanged, and canonical fixture comparison at every runtime milestone.

## 16. Exit criteria and Phase 6 handoff
Phase 5 is complete only when:
1. Remote-only accepted knowledge and gaps serve with zero leases.
2. Catalog published reads never enter legacy publisher authority.
3. The accepted facade is the only crate-external store API.
4. Establish, bind, and advance satisfy their distinct contracts.
5. Advance prepares off-lock and pointer-swaps under lock.
6. Existing advance uses pointer-specific CAS tokens.
7. Prior fallback is read-only.
8. Scope migration serves old truth until new-scope advance.
9. Overlay keys remain unchanged.
10. Catalog overlays never receive alternate object databases.
11. No new `BuiltFromStamp` variants exist.
12. Repo-local recall hydration is absent from remote reads.
13. Every section 14 adapter is converted or verified as already lease-bound.
14. Watchers use native attachment identity.
15. Tool-edge leases stay in the upper crate.
16. Checkout observations remain low-cardinality and schema-compatible.
17. Per-project capability health is separately available.
18. D-033 item 1 is observable and repairable.
19. The three exit proofs are all blocking and green.
20. Bridge parity is exact.
Phase 6 receives:
- every remaining bridge `ProjectRecord` use; every remaining legacy source lane; legacy publisher store deletion inventory; bridge cache deletion inventory; compatibility observation counters; accepted generation GC roots and cleanup status; watcher legacy carrier inventory; repository I/O legacy carrier inventory; static ownership allowlist with deletion reasons; exact bridge parity fixtures

## 17. Recommended implementation order
1. Land P5-A and freeze the public accepted facade.
2. Land P5-B and prove remote published views.
3. Record the Establish decision, then land P5-C.
4. Land P5-D and prove detached-peer overlays.
5. Land P5-E read adapters.
6. Land P5-F mutation, watcher, repo-I/O, and tool edges.
7. Land P5-G health and observer integration.
8. Land P5-H proof and Phase 6 inventory.
Do not combine P5-C pointer mutation with P5-B view wiring. Do not combine overlay algorithm changes with adapter conversion. Do not combine project health with checkout observation schema changes. These boundaries preserve independent rollback, bridge review, and fault localization.
