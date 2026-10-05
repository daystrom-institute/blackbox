---
title: "Remote knowledge source transport: published candidates"
kind: design
lifecycle: complete
corpus: blackbox-design
topic:
  - daemon-runtime
  - knowledge
  - corpus
  - bro-harness
tags: [locality, knowledge-source, publisher, workspace, cutover]
brief: "Move repo-owned knowledge and gap acquisition to checkout owners without changing accepted-publication or merge-gate semantics. One authenticated source contract carries committed publication candidates, accepted from the configured ref as they finalize; blackboxd validates, stores, and projects them without opening project paths. Reads are published-only, and uncommitted edits stay in the writing checkout."
---

# Remote knowledge source transport

> **Status: complete.** The measured overlap, strict cutover runtime,
> covered-adapter retirement proof, and parent-plan closeout are complete for
> operator-cutover covered Published rows. This consumes the shipped
> accepted-publication store, project-scoped producer grants, typed Git
> transport, checkout identity, and knowledge/gap merge gate. It does not
> reopen those semantics. Every knowledge and gap read serves the accepted
> publication; the transport carries no workspace snapshots.

## 0. Outcome

After this plan lands for a transport-governed published project:

1. a checkout owner can publish one immutable committed candidate containing
   both `.bbox/knowledge` and `.bbox/gaps`;
2. blackboxd accepts that exact candidate into accepted publication, from the
   project's configured ref, without opening a checkout;
3. project-scoped knowledge and gap mutations from a workspace-bound session
   execute inside the owning workspace as durable files, never through a
   blackboxd `RepositoryMutation` lease;
4. every knowledge and gap read serves the accepted publication, and an
   uncommitted edit reaches other readers only through commit, candidate
   publication, and acceptance;
5. strict cutover suppresses blackboxd's project `.bbox` watcher and every
   `PublisherConfigTreeRead`, `KnowledgeGapOverlayRead`, or
   `RepositoryMutation` lease for the covered project;
6. accepted published content survives producer loss; and
7. bridge and uncovered `LegacyLocal` behavior remain exact until their own
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

### 1.2 What still opens a checkout

| Operation | Current owner | Local dependency to remove |
|---|---|---|
| Accepted publication establish/advance | `bbox_project_publisher_advance`, `publisher_publish_probe`, `project_catalog_admin` | Resolve `full_ref`, read committed project config, read committed knowledge/gaps, and revalidate the attachment/ref under the publication lock. |
| Project write routing | MCP `?project=` initialization plus `resolve_project_write` | Convert a daemon-visible path into `ResolvedCheckoutScope`; no remote workspace identity is carried by `WorkerSpawnSpec`. |
| Project knowledge mutation | `bbox_learn`, `bbox_forget`, `prepare_knowledge_write`, and `RepoIoAuthority` | Resolve a daemon-visible checkout and perform repo-owned writes under a blackboxd `RepositoryMutation` lease. |
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
  captures code and Git history without linking index/store crates. It is the initial producer binary for this contract.
- Typed Git transport owns a repository's history while its committed
  activation journal is current. Knowledge is project-scoped and has its own
  publication gate, so it keeps its own marker.

### 1.4 Missing primitive, correcting the older design ledger

`locality-first-decomposition.md` and `remote-worker-boundary.md` describe
workspace identity in `bro-core` as complete. Current code has only `BroId`,
`SessionId`, `TaskId`, and `AtomRef`; `WorkerSpawnSpec` carries task, session,
provider, cwd, environment, messages, and log paths, but no workspace id.

The durable checkout marker already has the required semantics. The missing
work is to type and transport it. `WorkspaceId` is therefore the wire name for
the existing checkout id value, not a second marker or an id derived from cwd.

## 2. Fixed decisions

### KT-D1: One contract for committed candidates

The dependency-clean `bbox-knowledge-source` contract has one descriptor,
`PublicationCandidateDescriptorV1`, for committed knowledge plus gaps (and the
graphs lane) at one full branch ref and exact commit, with bounded
file-manifest, content-addressed blob, canonical hashing, scope,
object-format, and error types. Candidate finalize creates
operator-reviewable evidence.

Rejected: transporting accepted generations directly. Accepted normalization
and pointer authority stay server-side.

Rejected: transporting working-tree snapshots. Reads are published-only, so a
workspace's uncommitted state has no corpus consumer.

### KT-D2: Knowledge and gaps are atomic everywhere

Every descriptor contains both lanes, including an explicit empty manifest.
Finalize validates both and publishes neither on any failure. Generation
identity binds both lane commitments.

Rejected: independent watchers/uploads, because current accepted publication
and merge-gate behavior treats knowledge and gaps as one reviewed change.

### KT-D3: The configured ref is the acceptance gate

A producer uploads and finalizes a `Ready` candidate; it has no request that
moves the accepted-publication pointer. Blackboxd accepts the candidate as it
finalizes when it comes from the project's bound producer, at the accepted
scope, on the configured ref, and passes candidate validation. The first
valid candidate from the owning producer establishes the pointer and makes
its branch ref the configured ref. Merging to that ref is the review gate.
The mechanics live in [publisher-auto-advance.md](publisher-auto-advance.md).

Moves that change the producer, the configured ref, or the scope, and
rollback to an earlier candidate, are operator moves through
`bbox_project_publisher_advance`, selected by immutable candidate generation
id; its attachment arm remains for uncovered projects. The server derives the
project from the candidate's authenticated scope and refuses cross-project,
cross-producer, stale-scope, corrupt, or non-ready candidates, and every
pointer move compare-and-swaps against the pointer it was checked against.

Remote observation changes one freshness fact deliberately: the full ref is
proved stable during producer capture, not re-resolved by blackboxd at pointer
swap. Acceptance serves the exact candidate that finalized; the server never
substitutes a newer one.

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
attachment arm; no automatic rewrite occurs. The first accepted remote
candidate writes V2 and retains the prior V1 arm exactly. Status and
health report binding kind without paths.

Accepted published reads do not require the source generation after pointer
verification, but GC retains the referenced source generation as audit input
while the pointer arm names it.

### KT-D5: Workspace identity is the existing checkout identity over the wire

`bro-core` carries a validated `WorkspaceId` containing exactly 32 lowercase
hex characters. Checkout owners obtain it from `.bbox/local/checkout-id`
through the existing nofollow, create-once implementation, and it serializes
as `checkout_id` wherever checkout identity appears.

`WorkerSpawnSpec`, fleet session summaries, task/session metadata, and the MCP
session-binding registry carry `WorkspaceId` additively. Cwd stays an execution
detail and never becomes a remote identity fallback. `bbox-code-collector`
keeps its main-worktree-only invariant.

### KT-D6: The workspace binding routes writes and render, never reads

A raw model argument, project selector, cwd, producer id, or workspace id
establishes no workspace authority. Managed harness spawn mints an opaque,
short-lived binding between task/session and `WorkspaceId`, holds it in
memory, renews it while the task lives, and revokes it at task end; the
self-MCP config carries it in the redacted `x-blackbox-workspace-binding`
header. MCP initialization authenticates the binding and pins it to the
session.

The binding grants no publication-candidate mutation and contains no producer
token. It authorizes exactly two things: the project render locality exchange
for the named workspace, and write routing, under which the daemon refuses
that session's project knowledge and gap MCP writes with
`error.knowledge_transport_authoritative` because the harness writes them
into its own checkout (KT-D9). It never selects what a read returns. An
external client without a managed binding retains current same-host
attachment resolution during overlap and has no project-mutation authority
after strict cutover.

### KT-D7: Producer auth for publication, workspace auth for locality

Publication-candidate routes use the existing `CodeCollectionProducerConfig`
token and `project_transport_grant`. A `knowledge_transport_enabled` switch
defaults false and requires catalog authority plus enabled code collection.
There is no new producer token file, token table, or whole-repository
widening.

The workspace binding of KT-D6 authenticates only render locality and
session write routing. A producer credential cannot claim a managed
workspace, and a workspace binding cannot create a publication candidate.
Both auth lanes run before body parsing. The server derives project id and
catalog epoch; requests carry only scope and source facts.

### KT-D8: Strict project-scoped cutover, no fallback after authority moves

`KnowledgeTransportCutoverMarkerV1` records per project:

- project id, published scope, producer id, and grant commitment;
- accepted pointer/generation and remote candidate evidence;
- workspace-parity fields, which stay in the row shape with an empty list
  and commitments to an empty list so durable markers decode;
- observation-counter baseline/end, window bounds, and catalog epoch; and
- schema and implementation commitments.

Apply is offline and operator-authorized. After a row validates at startup:

- published advance accepts producer candidates only;
- blackboxd never registers a project watcher or acquires
  `PublisherConfigTreeRead`, `KnowledgeGapOverlayRead`, or
  `RepositoryMutation` for that project;
- producer loss or corrupt remote input degrades without checkout fallback;
  and
- unrelated projects, bridge mode, and uncovered `LegacyLocal` rows keep their
  exact adapters.

A scope migration, producer assignment change, or accepted-source binding
change makes only the affected row pending re-cutover. It never silently
reopens local fallback for a previously covered published project.

Marker rows pin stable producer/scope authority, not an immutable evidence
generation (the same model the code-source marker uses). An
accepted publication that advances through the same authenticated producer
channel stays current, because the transport is the only way content
arrives; the pinned generation and pointer hashes remain in the row as
cutover evidence but do not gate currency. A missing accepted publication or
a non-producer binding still pends re-cutover. Pinning a generation would make
every routine knowledge commit on a covered project wait on a daemon-stopping
offline ceremony, which is untenable under a continuously running collector.

### KT-D9: Read-your-writes stays local to the writing harness

The harness reads and writes its own `.bbox` files directly, and those writes
are durable files in the bound checkout. A network failure cannot prevent a
harness from reading its own files or completing a knowledge edit. A local
edit reaches other readers only through commit, candidate publication, and
acceptance, the same path every repo-owned change takes. A bound session's
project render overlays the checkout's uncommitted `.bbox/knowledge` onto the
published render plan inside the harness
([render-locality-transport-impl.md](render-locality-transport-impl.md)), so
the session's own rendered memory reflects its edits without sending them to
blackboxd.

For a workspace-bound session, the harness owns project-scoped
implementations of `bbox_learn`, `bbox_forget`, and the gap
file/resolve/update family. They link the existing `bbox-knowledge` and
`bbox-gaps` domain code through a checkout-confined repo-I/O adapter and
preserve one-file-per-entry, transaction-pending, dedupe, validation, and
response semantics.

The composite tool binding routes by authority:

- global knowledge mutations keep using the corpus MCP implementation;
- project mutations matching the bound workspace execute locally;
- another project's mutation refuses; and
- an unbound remote session cannot mutate project state.

When an existing project entry needs a seed, the local binding reads it from
the workspace's own working files. It never asks blackboxd to read a source
path. Project render stays explicit and local mutations report
`render_pending=true`.

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

All routes live below `/internal/knowledge-source/v1/` and use producer auth:

```text
POST publication/probe
POST publication/uploads
POST publication/uploads/{id}/manifest/{lane}/{page}
GET  publication/uploads/{id}/missing
PUT  publication/uploads/{id}/blobs/{sha256}
POST publication/uploads/{id}/finalize
GET  publication/generations/{id}/status
```

`lane` is `knowledge`, `gaps`, or `graphs`. Manifest page order is canonical
and pages are contiguous. Missing-blob responses make retries resumable.
Finalize is idempotent for the exact upload and generation, accepts
byte-identical evidence, and conflicts on the same logical generation with
different bytes. Finalize journals advance monotonically and cannot change
upload identity.

The checkout-owner code collector accepts HTTPS or loopback HTTP. Redirects
remain disabled and requests stay pinned to the configured origin. As an
explicit config opt-in, `trusted_encrypted_network = true` admits one
configured plaintext daemon endpoint that the operator has placed behind an
encrypted ACL-bound network; absent the flag, non-loopback URLs must use
HTTPS.

### 3.3 Durable layout

`bbox-knowledge-source-store` owns nofollow persistence under the configured
state root:

```text
knowledge-sources/
  publications/uploads/
  publications/generations/<project>/<generation>/
  publications/generation-index/
  blobs/sha256/<prefix>/<digest>
  journals/
```

Publication candidates are durable review inputs. Store recovery either
completes a committed finalize journal or leaves the prior generation; it
never publishes a manifest without every verified blob. Blob GC keeps a blob
while any retained publication references it and reclaims an unreferenced
blob after the configured grace period.

Before the store opens, blackboxd removes state an older store kept for
workspace snapshots: the `knowledge-sources/provisional/` directory, every
`journals/provisional-*.json`, and the `operator-workspace-bindings.json`
file. Removal is idempotent and bounded to exactly those members; a failure
is logged and never blocks startup, and the store never reads them. Blobs only
those snapshots referenced become unreferenced and the hourly store
maintenance reclaims them after the grace period.

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

## 5. Overlap, parity, and cutover

### 5.1 Shadow publication

Before pointer V2 is used, a remote candidate captured from the current
attachment ref is normalized with the same accepted builder in dry-run mode.
Parity requires identical source manifests, normalized records, hashes,
counts, accepted scope, ref, and commit.

### 5.2 Cutover readiness

Preflight refuses unless:

- current catalog/grant/source bindings are stable;
- the accepted publication is current and a producer candidate has exact
  published parity;
- no upload/finalize journal is prepared;
- observation deltas show every expected local shadow operation and no
  unexplained lease/watcher use; and
- a restart/rebuild rehearsal serves the same published results.

Apply installs the marker only. It does not delete watchers, registry rows,
attachments, V1 pointers, or bridge assets. Runtime classification closes the
local adapter for covered rows. Physical bridge retirement remains a later
operator-approved arc.

## 6. Health and error vocabulary

Stable HTTP errors include:

```text
knowledge_transport_disabled
knowledge_source_scope_forbidden
knowledge_source_candidate_stale
knowledge_source_ref_moved
knowledge_source_manifest_invalid
knowledge_source_blob_mismatch
knowledge_source_generation_conflict
knowledge_source_publication_failed
```

MCP/admin refusals include:

```text
error.accepted_publication_candidate_required
error.accepted_publication_candidate_stale
error.knowledge_transport_authoritative
```

Per-project health separates:

- accepted content integrity;
- source binding kind and producer availability;
- newest ready publication candidate and observation age;
- watcher registration and local lease observations during overlap; and
- cutover category, pending-recutover reason, and no-fallback state.

No health response contains source bytes, tokens, absolute paths, or raw store
errors.

## 7. Ordered implementation slices

### KT-A: Bottom contracts

Status: complete.

1. Add validated `bro_core::WorkspaceId` and additive protocol fields.
2. Add `bbox-knowledge-source` with descriptors, manifests, limits, canonical
   ids, and golden tests.
3. Correct the stale workspace-identity completion claims in parent designs.

Gate: contract unit/golden tests, workspace and fleet protocol round trips,
dependency acceptance, formatter, workspace nextest, clippy, and concurrency.

### KT-B: Store, authenticated intake, and collector capture

Status: complete.

Gate evidence: commit `1419468d049b` passed the 82-test focused KT-B matrix
and cluster workflow `bbox-verify-6qcx2` completed the full nextest, clippy,
and concurrency gates on that exact SHA.

1. Add `bbox-knowledge-source-store` with resumable CAS, journals, recovery,
   and GC.
2. Add separately gated publication routes using `project_transport_grant`.
3. Extend `bbox-code-collector` only with explicit main-worktree
   `published_knowledge` capture. Do not weaken its main-worktree invariant.
4. Ingest as shadow-only; do not change accepted pointers or live views.

Gate: auth-before-parse, scope/cross-producer matrices, malicious manifests,
capture races, crash/restart recovery, and collector dependency ceiling.

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

### KT-D: Local mutations and session binding

Status: complete.

1. Carry `WorkspaceId` through worktree lifecycle, spawn, session registry,
   replay/re-adoption, and the redacted MCP binding.
2. Authenticate the binding at MCP initialize and pin it to the session for
   render locality and write routing only.
3. Add harness-native confined project knowledge/gap mutation tools; keep
   global mutations on corpus MCP.
4. Renew the binding while the task lives and revoke it at task teardown.

Gate: forged/raw workspace refusal, task/session mismatch, published-only
reads for bound sessions, project mutation parity, global forwarding,
cross-project isolation, transaction recovery, reconnect/re-adoption,
teardown, and no token logging.

### KT-E: Measured overlap and strict cutover

Status: complete.

Evidence: code commit `d51ca9595210` passed exact-ref cluster workflow
`bbox-verify-mnnk2`, including the full 6,331-test workspace nextest profile,
workspace clippy, and the concurrency gate. The implementation persists
bounded per-operation and per-target checkout observations, requires exact
readiness, parity, capability-baseline, and blocked-project acknowledgements
through an offline marker/receipt ceremony. Once the marker covers a Published
project, watcher refresh, local read/write/schema-marker acquisition, and
fallback after drift or producer loss remain closed. Bridge, uncovered, and
`LegacyLocal` lanes remain intentionally outside that cutover.

No production marker was applied and no deployed-instance cutover is claimed.
The production daemon remained off; deployment and marker application require
separate operator authorization.

1. Add per-operation observation counters.
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
watcher projection/registration, mutation, recovery,
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
4. Begin the next checkout-local arc, project render, based on the remaining
   dependency map.

## 8. Verification matrix

Minimum end-to-end cases:

- empty, one-file, maximum-sized, malformed, duplicate-id, filename mismatch,
  and cross-lane partial uploads;
- committed candidate at current ref, ref movement during capture, stale but
  explicitly selected candidate, source loss after acceptance, and prior-arm
  fallback;
- SHA-1 and SHA-256 object formats;
- knowledge-only change, gap-only change, and simultaneous pair change;
- accepted generation advances while upload is open, after finalize, during
  view assembly, and across daemon restart;
- same-generation conflict and cross-producer hijack;
- published-only reads for managed bound and unbound sessions, and a bound
  session's project writes refused at the daemon;
- startup removal of retained workspace-snapshot state: idempotent, bounded,
  logged on failure, never read;
- V1 attachment pointer, V2 attachment pointer, V2 producer pointer, prior arm
  of each, source rebind, scope migration, producer removal/restoration, and
  pending re-cutover;
- covered project with portably restored state and zero attachments; all
  knowledge, gap, search, graph, render-input, embedding/index, health, and
  restart checks remain path-free; and
- port 7264/prod-daemon deployment is a separate operator action, never part of
  unit or integration tests.

## 9. Non-goals

- No arbitrary filesystem, Git object, pack, bundle, clone, fetch, ref-update,
  or shell RPC.
- No acceptance authority for a producer or model: acceptance follows the
  configured ref, and the producer cannot choose the ref, scope, or pointer.
- No caller-selected project id, attachment id, catalog epoch, corpus entity,
  or accepted record.
- No new producer credential family.
- No transport of uncommitted workspace state; reads are published-only.
- No global guidance render transport; global render stays operator-host local.
- No project render write from a local mutation; local knowledge mutations
  report render pending.
- No deletion of bridge assets, V1 accepted pointers, checkout registry data,
  or attachments merely because a transport row becomes authoritative.

## 10. Parent-plan effect

This document is the implementation authority for the remote knowledge source
named by [`locality-first-decomposition.md`](./locality-first-decomposition.md).
It consumes the checkout identity contract of
[`checkout-identity-and-provisional-knowledge.md`](../corpus/knowledge/checkout-identity-and-provisional-knowledge.md).
GH-G and KT-A through KT-F are complete. The locality program continues with
project render and the remaining local project-file walk retirement gates.
