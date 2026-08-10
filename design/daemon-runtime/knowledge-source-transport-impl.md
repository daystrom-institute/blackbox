---
title: "Remote knowledge source transport: published candidates and provisional workspaces"
kind: design
lifecycle: complete
corpus: blackbox-design
topic:
  - daemon-runtime
  - knowledge
  - corpus
  - bro-harness
tags: [locality, knowledge-source, provisional, publisher, workspace, cutover]
brief: "Move repo-owned knowledge and gap acquisition to checkout owners without changing accepted-publication, provisional-visibility, promotion, or merge-gate semantics. One authenticated source contract carries operator-accepted committed publication candidates and lease-bound provisional workspace snapshots; blackboxd validates, stores, and projects them without opening project paths."
---

# Remote knowledge source transport

> **Status: complete; implementation through KT-F verified 2026-08-09.** The
> measured overlap, strict cutover runtime, covered-adapter retirement proof,
> and parent-plan closeout are complete for operator-cutover covered Published
> rows. This consumes the shipped accepted-publication store, project-scoped
> producer grants, typed Git transport, checkout identity, provisional overlay
> model, and knowledge/gap merge gate. It does not reopen those semantics.

## 0. Outcome

After this plan lands for a transport-governed published project:

1. a checkout owner can publish one immutable committed candidate containing
   both `.bbox/knowledge` and `.bbox/gaps`;
2. an operator can establish or advance accepted publication from that exact
   candidate without blackboxd opening a checkout;
3. a managed workspace can publish one leased provisional snapshot containing
   its baseline, working knowledge, working gaps, and ancestry witness;
4. project-scoped knowledge and gap mutations execute inside the owning
   workspace, never through a blackboxd `RepositoryMutation` lease;
5. `published`, `own`, and `all` preserve their current meanings, including
   tombstones, content-equality promotion, invalid-own refusal, and peer
   degradation;
6. strict cutover suppresses blackboxd's project `.bbox` watcher and every
   `PublisherConfigTreeRead`, `KnowledgeGapOverlayRead`, or
   `RepositoryMutation` lease for the covered project;
7. accepted published content survives producer loss, while provisional state
   expires or is explicitly retired; and
8. bridge and uncovered `LegacyLocal` behavior remain exact until their own
   separately authorized retirement.

This is a typed source boundary, not a remote filesystem. No route accepts a
path, directory listing, Git pack, object database, caller-selected project
id, caller-computed overlay, or arbitrary corpus record.

## 1. Implementation-start inventory

This section records the checkout dependencies the implementation started
from. The KT-F closeout state is recorded in sections 8 and 11 and in the
governing locality inventory; it must not be read as a current runtime map.

### 1.1 What is already path-free

The accepted read side is already detached from a checkout:

- `crates/bbox-indexing/src/accepted_publication_store.rs` stores one immutable
  generation with project, scope, full ref, accepted commit, knowledge and gap
  manifests, normalized records, hashes, counts, and encoded-byte totals.
- `AcceptedPublicationRuntime` verifies one pointer arm and returns immutable
  accepted content. `published_knowledge_from_accepted` and its gap twin build
  views only from that content.
- An accepted generation contains no attachment path or attachment id. A
  detached publisher blocks freshness and advance, not published reads.
- Knowledge and gaps already share one generation and one pointer swap. A
  partial lane cannot become accepted.

The provisional semantic core is also mostly source-neutral:

- `bbox-knowledge::overlay` and `bbox-gaps::overlay` own published digests,
  baseline-versus-working comparison, tombstones, content-equality promotion,
  stamps, snapshot ids, invalid states, and bounded transient preservation.
- Overlay keys are `(PublishedScope, checkout_id)`. The checkout id is already
  a 128-bit reuse-safe random marker stored in `.bbox/local/checkout-id`; it is
  not path-derived.
- Knowledge and gap views already enforce `published|own|all`, authoritative
  session checkout selection, invalid-own failure, invalid-peer omission, and
  structured degradation.

### 1.2 What still opens a checkout

| Operation | Current owner | Local dependency to remove |
|---|---|---|
| Accepted publication establish/advance | `bbox_project_publisher_advance`, `publisher_publish_probe`, `project_catalog_admin` | Resolve `full_ref`, read committed project config, read committed knowledge/gaps, and revalidate the attachment/ref under the publication lock. |
| Provisional knowledge capture | `refresh_catalog_knowledge_overlay`, `stable_catalog_knowledge_overlay` | Acquire `KnowledgeGapOverlayRead`, read pending transaction state and `.bbox/knowledge`, inspect checkout Git, and revalidate the lease. |
| Provisional gap capture | gap-view twin | Same, for `.bbox/gaps`. |
| Lifecycle | `register_dark_knowledge_checkout`, `reconcile_dark_knowledge_checkouts`, watcher callbacks | Register host paths, watch them, refresh from them, and tear down by checkout registry observation. |
| Session `own` authority | MCP `?project=` initialization plus `resolve_project_write` | Convert a daemon-visible path into `ResolvedCheckoutScope`; no remote workspace identity is carried by `WorkerSpawnSpec`. |
| Project knowledge mutation | `bbox_learn`, `bbox_remember`, `bbox_decide`, link/review/forget, `prepare_knowledge_write`, and `RepoIoAuthority` | Resolve a daemon-visible checkout and perform repo-owned writes under a blackboxd `RepositoryMutation` lease, then refresh the local overlay. |
| Project gap mutation | `bbox_gap`, resolve/update, gap spool recovery, and `RepoIoAuthority` | Resolve and mutate the daemon-visible checkout under the same repository-mutation authority. |

### 1.3 Existing transport and authority substrate

- `ProducerAuthRuntime` is the single bearer-token table. Its
  `project_transport_grant` maps authenticated producer plus `PublishedScope`
  to the server-derived catalog project without widening to repository scope.
- `bbox-git-source` and `bbox-git-source-store` establish the resumable
  descriptor, manifest, content-addressed record, finalize, status, recovery,
  GC, and strict-auth patterns. Knowledge reuses those patterns, not their
  history payloads.
- `bbox-code-collector` is the thin checkout owner. It already verifies a
  configured committed scope, refuses redirects and unsafe remote HTTP, and
  captures code, Git history, and provenance without linking index/store
  crates. It is the initial producer binary for this contract.
- GH-G makes typed Git transport authoritative for covered repositories. Its
  marker and observation taxonomy are a pattern, not a marker to overload:
  knowledge is project-scoped and has separate publication/provisional gates.

### 1.4 Missing primitive, correcting the older design ledger

`locality-first-decomposition.md` and `remote-worker-boundary.md` describe
workspace identity in `bro-core` as complete. Current code has only `BroId`,
`SessionId`, `TaskId`, and `AtomRef`; `WorkerSpawnSpec` carries task, session,
provider, cwd, environment, messages, and log paths, but no workspace id.

The durable checkout marker already has the required semantics. The missing
work is to type and transport it. `WorkspaceId` is therefore the wire name for
the existing checkout id value, not a second marker or an id derived from cwd.

## 2. Fixed decisions

### KT-D1: One contract, two authority lanes

The new dependency-clean `bbox-knowledge-source` contract has two descriptors:

- `PublicationCandidateDescriptorV1` for committed knowledge plus gaps at one
  full branch ref and exact commit; and
- `ProvisionalWorkspaceDescriptorV1` for a leased workspace baseline plus
  working knowledge and gaps.

They share bounded file-manifest, content-addressed blob, canonical hashing,
scope, object-format, and error types. They do not share lifecycle authority:
candidate finalize creates operator-reviewable evidence, while provisional
finalize publishes transient workspace visibility.

Rejected: transporting accepted generations directly. Accepted normalization
and pointer authority stay server-side.

### KT-D2: Knowledge and gaps are atomic everywhere

Every descriptor contains both lanes, including an explicit empty manifest.
Finalize validates both and publishes neither on any failure. Provisional
lease renewal and retirement apply to the pair. Snapshot identity binds both
lane commitments.

Rejected: independent watchers/uploads, because current accepted publication
and merge-gate behavior treats knowledge and gaps as one reviewed change.

### KT-D3: Publication remains an operator mutation

A producer can upload and finalize a `Ready` candidate. It cannot move the
accepted-publication pointer. `bbox_project_publisher_advance` gains a typed
candidate source arm selected by immutable candidate generation id, while its
existing attachment arm remains during overlap.

The operator still supplies mode, project, expected catalog epoch, expected
accepted generation/pointer tokens, and audit reason. The server derives the
project from the candidate's authenticated scope and refuses cross-project,
cross-producer, stale-token, stale-scope, corrupt, or non-ready candidates.

Remote observation changes one freshness fact deliberately: the full ref is
proved stable during producer capture, not re-resolved by blackboxd at pointer
swap. The candidate id, observed ref tip, capture time, producer id, and source
commit are visible before acceptance. The operator selects that exact evidence;
the server never substitutes a newer candidate.

### KT-D4: Pointer V2 separates content from source binding

Accepted content remains byte-for-byte the current immutable generation shape.
The mutable pointer evolves additively:

```text
AcceptedPublicationSourceBindingV2 =
  Attachment { attachment_id }
  Producer {
    producer_id,
    source_generation_id,
    source_generation_sha256,
  }
```

Current and prior arms each carry their own binding. V1 pointers decode as the
attachment arm; no automatic rewrite occurs. The first operator-accepted
remote candidate writes V2 and retains the prior V1 arm exactly. Status and
health report binding kind without paths.

Accepted published reads do not require the source generation after pointer
verification, but GC retains the referenced source generation as audit input
while the pointer arm names it.

### KT-D5: Workspace identity is the existing checkout identity over the wire

`bro-core` gains a validated `WorkspaceId` containing exactly 32 lowercase hex
characters. Checkout owners obtain it from `.bbox/local/checkout-id` through
the existing nofollow, create-once implementation. Entity refs and overlay
keys continue serializing the value as `checkout_id` for compatibility.

`WorkerSpawnSpec`, fleet session summaries, task/session metadata, and the MCP
session-binding registry carry `WorkspaceId` additively. Cwd stays an execution
detail and never becomes a remote identity fallback. `bbox-code-collector`
keeps its main-worktree-only invariant; managed-workspace capture belongs to
the workspace-bound harness/CLI path, not to a widened collector scan.

### KT-D6: `own` requires a daemon-minted session/workspace binding

A raw model argument, project selector, cwd, producer id, or workspace id does
not establish `own`. Managed dispatch receives an opaque, short-lived binding
between task/session and `WorkspaceId`; the self-MCP config carries it in a
redacted header. MCP initialization validates the binding and installs the
workspace overlay key as authoritative session context.

The binding grants no publication-candidate mutation and contains no producer
token. It authorizes only the named workspace's provisional upload, renewal,
retirement, project-scoped mutation context, and `own` view. An external client
without a managed binding retains current same-host attachment resolution
during overlap and has no remote `own` or project-mutation authority after
strict cutover.

### KT-D7: Provisional input carries source facts, never overlay conclusions

The producer sends:

- scope and `WorkspaceId`;
- exact accepted generation and accepted commit observed before capture;
- checkout head and Git object format;
- a complete ancestry witness containing object id plus ordered parent ids for
  the union of commits reachable from checkout head and accepted commit;
- baseline knowledge/gap manifests and bytes at the claimed merge base;
- working knowledge/gap manifests and bytes;
- transaction-pending observations before and after capture; and
- stable-capture fingerprints, counts, bytes, and commitments.

The server validates the graph, independently computes the merge base, checks
the claimed base, validates all JSON/filename/id rules, projects accepted
digests from the currently verified generation, and computes overlay values and
snapshot ids itself.

The ancestry witness contains no commit messages, changed paths, trees, blobs,
packs, refs, or object database. It is bounded typed source evidence. The
baseline and working file bytes are producer attestations under the same
project-scoped credential, exactly as typed Git history is producer-attested.

### KT-D8: Refactor overlay computation into a pure source-neutral core

Knowledge and gap crates gain a pure catalog entry point accepting:

```text
accepted identity + accepted file digests
checkout/workspace id + head + verified merge base
baseline source map + working source map
```

The existing checkout adapter performs Git/path acquisition and calls that
core. The remote adapter verifies the descriptor and calls the same core. This
is what makes parity compare snapshot ids and values rather than two bespoke
implementations.

### KT-D9: Provisional snapshots are leased transient state

One ready provisional generation has `observed_at`, `lease_expires_at`, and a
monotonic producer/workspace sequence. The default lease is short and bounded
by server config; renewal names the exact generation. A newer generation
atomically supersedes the prior pair.

The provisional probe returns both the optional live generation and the next
durable sequence. The store derives that sequence under its mutation lock from
the immutable per-workspace sequence assignments, not from the live pointer,
so lease expiry or explicit retirement cannot reset the producer to sequence
one. A finalized sequence is single-use: an exact open upload remains
resumable, but a new upload cannot reuse an already-assigned sequence or an
installed generation identity.

The store may persist ready payloads under the corpus state root so daemon
restart does not create a false empty interval, but it is a transport cache,
not canonical knowledge. Startup drops expired, corrupt, grant-revoked,
accepted-generation-stale, and superseded generations. An explicit retire
endpoint removes visibility immediately. Expiry makes `own` unavailable and
omits the peer from `all` with bounded degradation; it never transient-preserves
past the lease.

### KT-D10: Producer auth for publication, workspace auth for provisional

Publication-candidate routes use the existing `CodeCollectionProducerConfig`
token and `project_transport_grant`. A new `knowledge_transport_enabled`
switch defaults false and requires catalog authority plus enabled code
collection. There is no new producer token file, token table, or
whole-repository widening.

Provisional routes use the short-lived daemon-minted workspace binding from
KT-D6. A producer credential cannot claim or overwrite a managed workspace,
and a workspace binding cannot create a publication candidate. Both auth lanes
run before body parsing. The server derives project id and catalog epoch;
requests carry only scope and source facts.

### KT-D11: Strict project-scoped cutover, no fallback after authority moves

`KnowledgeTransportCutoverMarkerV1` records per project:

- project id, published scope, producer id, and grant commitment;
- accepted pointer/generation and remote candidate evidence;
- each parity workspace id and matching local/remote knowledge plus gap
  snapshot ids;
- observation-counter baseline/end, window bounds, and catalog epoch; and
- schema and implementation commitments.

Apply is offline and operator-authorized. After a row validates at startup:

- published advance accepts producer candidates only;
- remote provisional snapshots are the only shared provisional source;
- blackboxd never registers a project watcher or acquires
  `PublisherConfigTreeRead`, `KnowledgeGapOverlayRead`, or
  `RepositoryMutation` for that project;
- producer loss, expired provisional state, or corrupt remote input degrades
  without checkout fallback; and
- unrelated projects, bridge mode, and uncovered `LegacyLocal` rows keep their
  exact adapters.

A scope migration, producer assignment change, or accepted-source binding
change makes only the affected row pending re-cutover. It never silently
reopens local fallback for a previously covered published project.

### KT-D12: Read-your-writes stays local to the writing harness

The harness continues reading and writing its own `.bbox` files directly.
Remote provisional publication exists for shared `own`/`all` corpus views and
cross-session visibility; it is not placed on the local edit path. A network
failure cannot prevent a harness from reading its own files or completing a
knowledge edit. The corpus view reports its last accepted provisional source
state explicitly.

### KT-D13: Project mutations become confined harness-native tools

For a workspace-bound session, the harness owns project-scoped implementations
of `bbox_learn`, `bbox_remember`, `bbox_decide`, link/review/forget, and the gap
file/resolve/update family. They link the existing `bbox-knowledge` and
`bbox-gaps` domain code through a checkout-confined repo-I/O adapter. They
preserve one-file-per-entry, transaction-pending, supersession, dedupe,
validation, and response semantics.

The composite tool binding routes by authority:

- global knowledge mutations keep using the corpus MCP implementation;
- project mutations matching the bound workspace execute locally;
- another project's mutation refuses; and
- an unbound remote session cannot mutate project state.

When an existing project entry needs a seed, the local binding builds the same
`own` view from accepted pointer metadata, commit P in the workspace object
database, and working files. It never asks blackboxd to read a source path.

After a successful local mutation, provisional capture is scheduled
immediately. Sync failure does not roll back the local write; the tool reports
`provisional_sync_pending` and bounded background retry continues. Project
render remains explicit and `render_pending=true`, preserving the separate
project-render locality arc.

## 3. Wire and store contract

### 3.1 Contract crate

`crates/bbox-knowledge-source` is pure serde plus validation and hashing. It may
depend on `bro-core`, `bbox-corpus-core`, and hashing/serde crates. It never
opens a filesystem, invokes Git, serves HTTP, writes accepted publication,
loads a catalog, computes a view, or links daemon/runtime crates.

Canonical ids use versioned length-prefixed encodings, never JSON hashes.
Every struct denies unknown fields. Limits cover file count, per-file bytes,
per-lane bytes, graph nodes/edges, manifest pages, open uploads, and total
generation bytes.

### 3.2 Routes

All routes live below `/internal/knowledge-source/v1/`. Publication routes use
producer auth; provisional routes use workspace-binding auth:

```text
POST publication/probe
POST publication/uploads
POST publication/uploads/{id}/manifest/{lane}/{page}
GET  publication/uploads/{id}/missing
PUT  publication/uploads/{id}/blobs/{sha256}
POST publication/uploads/{id}/finalize
GET  publication/generations/{id}/status

POST provisional/probe
POST provisional/uploads
POST provisional/uploads/{id}/ancestry/{page}
POST provisional/uploads/{id}/manifest/{class}/{lane}/{page}
GET  provisional/uploads/{id}/missing
PUT  provisional/uploads/{id}/blobs/{sha256}
POST provisional/uploads/{id}/finalize
POST provisional/generations/{id}/renew
POST provisional/generations/{id}/retire
GET  provisional/generations/{id}/status
```

`class` is `baseline` or `working`; `lane` is `knowledge` or `gaps`.
Manifest and ancestry-page order is canonical and pages are contiguous.
Missing-blob responses make retries resumable. Finalize is idempotent for
the exact upload and generation. Finalize journals advance monotonically and
cannot change upload identity. Startup recovery may reconstruct a committed
provisional journal only for the legacy duplicate-finalize failure shape where
the immutable generation, sequence assignment, generation index, and exactly
one original Ready upload all agree.
byte-identical evidence and conflicts on the same logical sequence with
different bytes.

The generic source client accepts HTTPS or loopback HTTP. A managed harness
may additionally accept its daemon-authored endpoint over HTTP only through an
explicit trusted-daemon constructor: the remote fleet contract already
requires that endpoint to live behind an encrypted ACL boundary. Redirects
remain disabled and requests stay pinned to the configured origin. Arbitrary
callers do not inherit this exception. The checkout-owner code collector has
the same shape as an explicit config opt-in: `trusted_encrypted_network = true`
admits one configured plaintext daemon endpoint that the operator has placed
behind the same encrypted ACL-bound network (the accepted cage subnet route);
absent the flag, non-loopback URLs must use HTTPS.

### 3.3 Durable layout

`bbox-knowledge-source-store` owns nofollow persistence under the configured
state root:

```text
knowledge-sources/
  publications/uploads/
  publications/generations/<project>/<generation>/
  provisional/uploads/
  provisional/generations/<project>/<workspace>/<generation>/
  blobs/sha256/<prefix>/<digest>
  journals/
```

Publication candidates are durable review inputs. Provisional generations are
restartable cache entries bounded by leases. Store recovery either completes a
committed finalize journal or leaves the prior selected generation; it never
publishes a manifest without every verified blob.

## 4. Published candidate flow

1. The collector resolves the configured full `refs/heads/*` ref to commit P.
2. It verifies the committed project identity at P equals configured scope.
3. It captures bounded committed knowledge and gaps through stable Git object
   access, never the working tree.
4. It re-resolves the full ref and restarts if it moved.
5. It uploads/finalizes one candidate and polls `Ready`.
6. Status exposes the candidate id, P, ref, producer, age, counts, bytes, and
   hashes without bodies or paths.
7. The operator dry-runs and then establishes/advances the pointer from that
   exact candidate using normal epoch and pointer compare-and-swap tokens.
8. Server-side accepted normalization, generation installation, pointer swap,
   read-back, cache publication, restart verification, and prior fallback stay
   owned by `AcceptedPublicationRuntime`.

## 5. Provisional workspace flow

1. The workspace-bound harness/CLI reads/ensures its reuse-safe checkout marker
   and treats it as `WorkspaceId`.
2. It pins accepted generation and P from corpus status.
3. It observes no pending knowledge transaction, captures H, builds the
   H-plus-P ancestry witness, computes B, and reads baseline files at B.
4. It captures working knowledge and gaps with nofollow readers.
5. It rechecks pending state, H, B, and fingerprints twice. Movement restarts
   capture; a permanently busy workspace reports transient source failure.
6. It uploads/finalizes the atomic pair. The server recomputes B and both
   overlay snapshots, then atomically selects them for the workspace.
7. The producer renews the exact generation while the workspace is admitted.
   Worktree release sends retire before deletion; lease expiry is the crash
   fallback.
8. A managed MCP session bound to the workspace receives `own`; `all` sees all
   live valid workspace generations for the project.
9. A harness-native project mutation writes locally first, then triggers the
   same stable capture without sending the mutation operation to blackboxd.

## 6. Overlap, parity, and cutover

### 6.1 Shadow publication

Before pointer V2 is used, a remote candidate captured from the current
attachment ref is normalized with the same accepted builder in dry-run mode.
Parity requires identical source manifests, normalized records, hashes,
counts, accepted scope, ref, and commit.

### 6.2 Shadow provisional

For an admitted same-host checkout, upload and local watcher capture use the
same checkout marker. The remote descriptor and local checkout adapter call the
same pure overlay core. Parity requires identical knowledge and gap snapshot
ids, stamps, values, tombstones, fingerprints, and degradation classification.

The window includes additions, edits, deletions, content-equal promotion,
pending-transaction deferral, accepted-generation advance, checkout movement,
daemon restart, producer restart, worktree teardown, and lease expiry.

### 6.3 Cutover readiness

Preflight refuses unless:

- current catalog/grant/source bindings are stable;
- the accepted publication is current and a producer candidate has exact
  published parity;
- every selected workspace has current atomic knowledge/gap parity;
- no upload/finalize journal is prepared;
- no invalid or expired selected remote generation exists;
- observation deltas show every expected local shadow operation and no
  unexplained lease/watcher use; and
- a restart/rebuild rehearsal serves the same published/own/all results.

Apply installs the marker only. It does not delete watchers, registry rows,
attachments, V1 pointers, or bridge assets. Runtime classification closes the
local adapter for covered rows. Physical bridge retirement remains a later
operator-approved arc.

## 7. Health and error vocabulary

Stable HTTP errors include:

```text
knowledge_transport_disabled
knowledge_source_scope_forbidden
knowledge_source_candidate_stale
knowledge_source_ref_moved
knowledge_source_manifest_invalid
knowledge_source_blob_mismatch
knowledge_source_generation_conflict
knowledge_source_ancestry_incomplete
knowledge_source_merge_base_mismatch
knowledge_source_accepted_generation_stale
knowledge_source_workspace_sequence_stale
knowledge_source_lease_expired
knowledge_source_transaction_pending
knowledge_source_publication_failed
```

MCP/admin refusals include:

```text
error.accepted_publication_candidate_required
error.accepted_publication_candidate_stale
error.provisional_workspace_binding_required
error.knowledge_transport_authoritative
```

Per-project health separates:

- accepted content integrity;
- source binding kind and producer availability;
- newest ready publication candidate and observation age;
- each provisional workspace generation, sequence, lease, accepted generation,
  and knowledge/gap outcome;
- watcher registration and local lease observations during overlap; and
- cutover category, pending-recutover reason, and no-fallback state.

No health response contains source bytes, tokens, absolute paths, or raw store
errors.

## 8. Ordered implementation slices

### KT-A: Bottom contracts and pure overlay core

Status: complete.

1. Add validated `bro_core::WorkspaceId` and additive protocol fields.
2. Add `bbox-knowledge-source` with descriptors, manifests, ancestry witness,
   limits, canonical ids, and golden tests.
3. Extract source-neutral knowledge/gap catalog overlay functions and prove the
   current checkout adapters remain byte-identical.
4. Correct the stale workspace-identity completion claims in parent designs.

Gate: contract unit/golden tests, checkout-adapter parity tests, workspace and
fleet protocol round trips, dependency acceptance, formatter, workspace
nextest, clippy, and concurrency.

### KT-B: Store, authenticated intake, and collector capture

Status: complete.

Gate evidence: commit `1419468d049b` passed the 82-test focused KT-B matrix
and cluster workflow `bbox-verify-6qcx2` completed the full nextest, clippy,
and concurrency gates on that exact SHA.

1. Add `bbox-knowledge-source-store` with resumable CAS, journals, recovery,
   GC, and lease expiry.
2. Add separately gated publication routes using `project_transport_grant` and
   the provisional workspace-grant auth seam; KT-D wires production grant
   issuance into managed session lifecycle.
3. Extend `bbox-code-collector` only with explicit main-worktree
   `published_knowledge` capture. Do not weaken its main-worktree invariant.
4. Ingest as shadow-only; do not change accepted pointers or live views.

Gate: auth-before-parse, scope/cross-producer matrices, malicious manifests,
capture races, crash/restart recovery, expiry, and collector dependency ceiling.

### KT-C: Operator-accepted remote publication

Status: complete.

1. Land pointer V2 source binding with strict V1 decode.
2. Extend status, dry-run, establish, advance, prior fallback, rebind, and GC.
3. Accept only explicit ready candidate ids under existing operator CAS.
4. Prove accepted reads and restart are attachment-free after V2 acceptance.

Gate: V1/V2/current/prior/corrupt matrices, candidate staleness, epoch and
pointer races, source loss, rollback, and public tool docs.

Evidence: code commit `adcbaf807b8a` passed the exact-tip workspace default
profile (6,290 tests), full profile (6,295 tests across 71 binaries), workspace
clippy with zero errors, and the concurrency lint over 183 tool handlers in a
claimed cluster lane. Automated verifier `bbox-verify-ttxkd` was
infrastructure-red before compilation because the newest chained
`base-bbox` clone had no free space; this is a verifier-base retention defect,
not a KT-C gate failure.

### KT-D: Remote provisional views, local mutations, and session binding

Status: complete.

Evidence: code commit `fe5d2f0d948c` passed exact-ref cluster workflow
`bbox-verify-5jhp6`, including the full workspace nextest profile, clippy, and
the concurrency gate. The implementation carries redacted workspace bindings
through spawn and fleetd re-adoption, performs initial and post-write stable
capture from the checkout owner, serves remote `own`/`all` overlays, confines
project mutations to the harness, forwards global mutations, rejects ambiguous
merge bases, retires stale generations after accepted-publication advance, and
retires the selected generation on task teardown.

1. Carry `WorkspaceId` through worktree lifecycle, spawn, session registry,
   replay/re-adoption, and redacted MCP binding.
2. Select live remote generations into knowledge and gap overlay stores.
3. Bind `own` only through the managed session capability.
4. Add harness-native confined project knowledge/gap mutation tools and stable
   post-write capture; keep global mutations on corpus MCP.
5. Implement renewal, retire, expiry, accepted-advance invalidation, and
   restart restoration.

Gate: forged/raw workspace refusal, task/session mismatch, own/all semantics,
project mutation parity, global forwarding, cross-project isolation,
transaction recovery, sync-pending retry, reconnect/re-adoption, expiry,
teardown, and no token logging.

### KT-E: Measured overlap and strict cutover

Status: complete.

Evidence: code commit `d51ca9595210` passed exact-ref cluster workflow
`bbox-verify-mnnk2`, including the full 6,331-test workspace nextest profile,
workspace clippy, and the concurrency gate. The implementation persists
bounded per-operation and per-target checkout observations, requires exact
readiness, parity, capability-baseline, and blocked-project acknowledgements
through an offline marker/receipt ceremony, and restores remote `own` and
`all` selection durably after restart. Once the marker covers a Published
project, watcher refresh, local read/write/schema-marker acquisition, and
fallback after drift or producer loss remain closed. Bridge, uncovered, and
`LegacyLocal` lanes remain intentionally outside that cutover.

No production marker was applied and no deployed-instance cutover is claimed.
The production daemon remained off; deployment and marker application require
separate operator authorization.

1. Add per-operation observation counters and shadow comparison reports.
2. Implement offline preflight/apply/verify marker ceremony.
3. Close watcher plus read and mutation checkout-lease acquisition for covered
   rows.
4. Prove no fallback under producer loss, grant change, scope migration,
   accepted-source change, or remote corruption.

Gate: full parity matrix, declared zero-local-observation window, remote-only
restart/rebuild smoke, doctor, cluster full verify, and explicit operator
authorization.

### KT-F: Adapter retirement and parent-plan closeout

Status: complete.

The KT-F audit found no second, independently deletable "covered" publisher or
watcher implementation after KT-E. Runtime classification already routes a
covered Published row away before publisher binding/attachment advance,
watcher projection/registration, overlay capture, mutation, recovery,
schema-marker, or startup carrier acquisition. The remaining local adapter
bodies are shared only by bridge, uncovered, and `LegacyLocal` compatibility
lanes; deleting them here would retire behavior outside this plan's authority.

Closeout therefore records the covered adapter as an already-retired
executable route, not a separately deletable implementation. The watcher and
publisher regression tests are non-vacuous: each first drives a real uncovered
checkout lease, then installs coverage and proves the covered path adds no
checkout-broker operation. The covered publisher returns
`error.knowledge_transport_authoritative`; the covered watcher projects no
carrier. The broker's strict capability policy remains as defense in depth.

1. Remove covered catalog publisher/watcher acquisition code only after KT-E.
2. Preserve bridge and uncovered `LegacyLocal` adapters until their own gate.
3. Update the locality inventory, knowledge design status, operations docs,
   and governing all-adapter closeout report.
4. Begin the next checkout-local arc, blame or project render, based on the
   remaining dependency map.

## 9. Verification matrix

Minimum end-to-end cases:

- empty, one-file, maximum-sized, malformed, duplicate-id, filename mismatch,
  and cross-lane partial uploads;
- committed candidate at current ref, ref movement during capture, stale but
  explicitly selected candidate, source loss after acceptance, and prior-arm
  fallback;
- workspace at P, ahead of P, behind P, diverged from P, missing P, incomplete
  ancestry, multiple merge bases, shallow capture, SHA-1, and SHA-256;
- tracked edit, untracked add, deletion/tombstone, content-equal cherry-pick,
  knowledge-only change, gap-only change, and simultaneous pair change;
- pending transaction before capture, during capture, and after first stable
  read; repeated movement exhausts capture without publishing partial state;
- accepted generation advances while upload is open, after finalize, during
  view assembly, and across daemon restart;
- workspace sequence replay, same-sequence conflict, cross-producer hijack,
  lease renewal race, explicit retire, expiry, and worktree path reuse with a
  fresh marker;
- `published`, default `own`, explicit `own`, and `all` for managed bound,
  unbound, expired, invalid-own, and invalid-peer sessions;
- V1 attachment pointer, V2 attachment pointer, V2 producer pointer, prior arm
  of each, source rebind, scope migration, producer removal/restoration, and
  pending re-cutover;
- covered project with portably restored state and zero attachments; all
  knowledge, gap, search, graph, render-input, embedding/index, health, and
  restart checks remain path-free; and
- port 7264/prod-daemon deployment is a separate operator action, never part of
  unit or integration tests.

## 10. Non-goals

- No arbitrary filesystem, Git object, pack, bundle, clone, fetch, ref-update,
  or shell RPC.
- No automatic knowledge acceptance by a producer or model.
- No caller-selected project id, attachment id, catalog epoch, corpus entity,
  accepted record, overlay value, tombstone, or promotion result.
- No new producer credential family.
- No branch-private replacement for shared provisional visibility.
- No global guidance render transport; global render stays operator-host local.
- No project render write or blame relocation in this arc; local knowledge
  mutations continue reporting render pending.
- No deletion of bridge assets, V1 accepted pointers, checkout registry data,
  or attachments merely because a transport row becomes authoritative.

## 11. Parent-plan effect

On ratification, this document becomes the implementation authority for the
remote knowledge source named by
[`locality-first-decomposition.md`](./locality-first-decomposition.md). It
consumes the semantics of
[`checkout-identity-and-provisional-knowledge.md`](../corpus/knowledge/checkout-identity-and-provisional-knowledge.md)
without superseding that design. GH-G and KT-A through KT-F are complete and
are no longer the next locality arc. The locality program continues with the
typed blame boundary, followed by project render and the remaining local
project-file walk retirement gates.
