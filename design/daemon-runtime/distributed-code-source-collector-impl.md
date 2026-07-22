---
title: "Distributed code-source collector implementation plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, collector, code-intelligence, authentication, content-addressed, indexing]
brief: "Introduce an authenticated, scope-bound project-file source feed with corpus-side chunking, generation-safe activation, and an explicit local-walker overlap, without reviving remote mounts or putting corpus dependencies in the producer."
---

# Distributed code-source collector implementation plan

Date: 2026-07-21

Companion design:
[`locality-first-decomposition.md`](locality-first-decomposition.md), sequencing
slice 4.

## 1. Outcome and bounded scope

This slice establishes the first distributed code-intelligence source seam. A
machine that owns a registered base checkout can publish its current project
files to the corpus host. The producer walks, hashes, and uploads raw bounded
bytes. The corpus host remains the sole owner of chunking, Tantivy documents,
embeddings, entity refs, derived workspace edges, and generation activation.

At the end of this slice:

- a dependency-clean `bbox-code-collector` can publish one or more explicitly
  configured base project roots;
- every producer credential is bound server-side to an immutable producer id
  and an allowlist of durable `PublishedScope` values;
- the server negotiates immutable manifests against a content-addressed blob
  cache and requests only missing blobs;
- a completed generation is indexed from its immutable manifest without the
  corpus process opening the producer checkout;
- all code-search and graph readers see one activated project-file generation,
  while staged and retained generations remain invisible;
- a collector-owned scope stops local project-file walking only after its first
  collected generation activates successfully;
- disabling collector ownership performs an explicit, successful local rebuild
  before switching back, rather than silently failing over;
- full rebuild, incremental purge, restart recovery, cache loss, and repeated
  publication converge without duplicate active documents or mixed source
  authority.

This is intentionally an overlap slice on the existing monolith. The corpus
host must already have exactly one locally registered `ProjectRecord` that
resolves to the published scope. The server maps the authenticated scope to
that record and chooses its host-local `project_id`; no request may supply or
select a `project_id`.

The existing registration prerequisite is load-bearing and limits what this
slice claims. It does not create remote-only projects, encode a producer path
as a corpus path, or make the whole daemon relocatable. A later project-catalog
migration must separate durable corpus project identity from local checkout
attachments before the last local registration can disappear. Keeping that
gate explicit avoids recreating the satellite arc's dead absolute-path bridge.

## 2. Non-goals

The following are not smuggled into the file collector:

- Git history, Git objects, blame, or provenance import. The current Git-history
  indexer may continue using the registered local checkout during overlap. A
  collected file generation contains current bytes only and never claims to be
  a Git mirror.
- `.bbox/` knowledge, gaps, config, local state, or secrets. `.bbox/` remains a
  dedicated knowledge/config carrier and is excluded from generic source
  manifests.
- worktrees, dirty editor buffers outside the filesystem, submodules as nested
  repositories, symlink targets, devices, sockets, or other non-regular files.
- model-facing upload tools, MCP upload methods, `/internal/records`, or a
  general arbitrary-blob service.
- unbounded repository support. Request bodies, manifest cardinality, logical
  generation bytes, file bytes, concurrent uploads, retained generations, and
  disk use all have enforced limits.
- transparent source failover. Staleness or producer loss preserves the last
  activated collected generation and reports degraded health.

## 3. Current coupling that must be split

`bbox-corpus-index::index::project_files` currently owns both sides of the
boundary:

1. It walks each `ProjectRecord.canonical_path`, applies ignore and size rules,
   reads bytes, and derives a freshness key from path, mtime, and size.
2. It selects a corpus chunker, builds chunks and entity refs, writes Tantivy
   documents, derives code and workspace edges, and materializes edge
   snapshots.

`scan_registered_project_files` also feeds the global `_meta.json` purge. A
remote generation bolted on after that scan would be deleted by the next pass.
`build_project_file_doc` stores a host absolute path as both `project` and
`file_path`; the local incremental indexer deletes by that path. The edge
sidecar already has immutable snapshot directories and one active workspace
manifest, but ordinary search does not use that manifest to exclude inactive
Tantivy or vector generations. `BBOX_PROJECT_REFS_V2` is still optional.

The implementation therefore needs a source abstraction and an active
generation read contract. An upload endpoint alone would be incomplete.

## 4. Identity and source authority

### 4.1 Durable scope is the wire identity

Wire requests carry only a normalized `PublishedScope`:

```text
(repo_id, bbox_root_relpath)
```

The producer resolves it from the committed `.bbox/config.toml` at local
`HEAD`, using the same recorded or operator-overridden repo-id authority as
checkout-local provenance export. Computed bootstrap ids and `aka_repo_ids`
cannot authorize publication. `bbox_root_relpath` is normalized to `.` or a
slash-separated relative path with no traversal.

The server resolves the scope against current registered project records using
the existing committed-config publisher resolver. Resolution must produce
exactly one record. Zero matches returns `scope_not_registered`; multiple
matches returns `scope_ambiguous`. The token's allowlist does not manufacture a
project registration and request JSON never chooses a local record.

### 4.2 One configured source owner per scope

Add strict daemon configuration equivalent to:

```toml
[code_collection]
enabled = true
max_manifest_files = 250000
max_manifest_logical_bytes = 5368709120
max_open_uploads_per_producer = 2
retained_generations = 2
unreferenced_blob_grace_hours = 168

[[code_collection.producers]]
producer_id = "checkout-host-a"
token_file = "~/.config/blackbox/code-collectors/checkout-host-a.token"
scopes = [
  { repo_id = "<recorded-id>", bbox_root_relpath = "." },
]
```

`producer_id` is a bounded stable label, not a secret and not caller supplied.
At daemon open and SIGHUP reload:

- every token file is loaded with `bro_rpc::ServiceToken::load`, including its
  owner, mode, symlink, hardlink, and shape checks;
- token values must be unique;
- each scope may appear under at most one producer;
- every scope must resolve to exactly one current registered project;
- the whole replacement auth table is built before one atomic swap. A reload
  error retains the previous table and reports the failure.

Removing or changing an assignment is operator authority. It does not make a
request from the former producer valid, and it does not instantly change the
active index generation. Section 10 defines the source transition.

### 4.3 Authentication is outside model and shell authority

Mount dedicated HTTP routes under `/internal/code-source/v1/*`. Every route
requires `Authorization: Bearer <token>`. The header authenticates a producer
before the bounded body is parsed. The body's scope must then be an exact member
of that producer's server-side allowlist before any request data enters durable
state. Comparison uses `ServiceToken::verify`; tokens stay redacted and are
never accepted in query strings, JSON, environment variables, MCP arguments,
logs, metrics, or response bodies.

The collector reads the token file into its own process and does not export it
to child processes. It refuses non-loopback `http://` corpus URLs. Remote
publication therefore requires `https://` at the producer, normally through
the deployment's TLS ingress. Loopback HTTP remains available for tests and a
same-host rollout. Redirect following is disabled so credentials cannot be
forwarded to a different authority.

Authentication proves the configured producer, not the truth of its bytes.
Scope allowlisting, path validation, content hashing, generation ordering, and
server-side chunking remain independent checks.

## 5. Shared contract and dependency direction

Create `bbox-code-source`, a small leaf crate owning:

- versioned wire structs and structured error codes;
- `PublishedScope` and relative-path validation helpers;
- manifest entry ordering, digest, and generation-id algorithms;
- file type and byte-cap policy shared by the local walker and collector;
- the walker-policy version constant;
- bounded producer-id, upload-id, hash, cursor, and header parsing.

It may depend on `bbox-corpus-core`, `serde`, `sha2`, and similarly small leaf
utilities. It must not depend on Axum, Reqwest, Tantivy, `bbox-chunker`,
`bbox-corpus-index`, `bbox-indexing`, `blackbox`, `bro-harness`, V8, or provider
SDKs.

Create `bbox-code-source-store` for corpus-side blob, upload, manifest, and
generation state. It depends on `bbox-code-source` and storage leaf crates, but
not on Axum or the root package. The HTTP adapter in `src/server/code_source.rs`
depends on the store and the existing server state. Corpus indexing consumes
the store through a read-only generation view.

Create the `bbox-code-collector` binary as a sibling runtime. It may depend on
`bbox-code-source`, `bbox-config`, `bbox-corpus-core`, `ignore`, `reqwest`, and
small CLI/runtime crates. It may reuse the dependency-clean
`bro-rpc::ServiceToken` file loader, but no framing or same-host peer-credential
contract. It must not depend on the central store crate or any corpus
implementer. `scripts/acceptance-code-collector-deps.sh` checks the resolved
`cargo tree` and rejects Tantivy, chunker, corpus-index, indexing, embedding,
vector, edge-index, blackbox, bro-harness, V8, and model-provider dependencies.

The existing local walker adopts the leaf policy helpers. This makes extension,
directory exclusion, and input-byte caps one versioned contract while leaving
all format sniffing and chunking corpus-side.

## 6. Manifest and blob protocol

### 6.1 Immutable source generation

One generation descriptor contains:

```text
schema_version = 1
walker_policy_version
scope
head_commit
dirty_fingerprint
manifest_sha256
file_count
logical_bytes
```

`head_commit` is the exact local `HEAD` commit observed at scan start.
`dirty_fingerprint` is SHA-256 over the ordered `(path, content_sha256, size)`
manifest plus the observed HEAD. The manifest digest is SHA-256 over the
canonical length-prefixed encoding of those entries. The server computes and
checks both. A generation id is server-derived from producer id, scope, policy
version, HEAD, dirty fingerprint, and manifest digest. It is never accepted as
authority from the caller.

Manifest entries are strictly increasing by raw UTF-8 path bytes and contain:

```text
relative_path
content_sha256
size
```

Only regular files are representable. Paths must be non-empty relative slash
paths, at most 4096 bytes total and 255 bytes per component, with no empty,
`.`/`..`, backslash, NUL, control, or platform-prefix component. Duplicate paths
are rejected. Hashes are exactly 64 lowercase hexadecimal characters. The
server applies the same extension allowlist and per-extension cap as the local
walker, then applies configured file-count and logical-byte caps to the whole
generation.

### 6.2 Bounded upload conversation

The API is resumable and idempotent:

1. `POST /internal/code-source/v1/uploads` submits the bounded descriptor and
   receives a random server upload id plus manifest page limits.
2. `PUT /internal/code-source/v1/uploads/{id}/manifest/{page}` submits at most
   2,000 entries and 2 MiB JSON. Pages must be contiguous. Repeating a page with
   the same digest is a no-op; conflicting repetition rejects the upload.
3. `POST /internal/code-source/v1/uploads/{id}/manifest/complete` verifies page
   continuity, entry ordering, declared counts, logical bytes, policy version,
   manifest digest, and dirty fingerprint. It persists the immutable manifest
   and returns a cursor-page of missing content hashes.
4. `GET /internal/code-source/v1/uploads/{id}/missing?cursor=...` pages the
   server-owned missing set. The cursor is opaque, generation-bound, and
   rejected after the upload changes state.
5. `PUT /internal/code-source/v1/uploads/{id}/blobs/{sha256}` accepts a raw body
   with an exact `Content-Length`, capped before allocation. The server streams
   to a private temporary file while hashing, verifies hash and size against at
   least one manifest entry, fsyncs, and installs by create-or-verify rename.
6. `POST /internal/code-source/v1/uploads/{id}/finalize` succeeds only when
   every manifest hash is present and verified. It marks the generation ready,
   records it as the producer's newest desired generation for the scope, and
   returns `202 Accepted` plus a status URL.
7. `GET /internal/code-source/v1/generations/{generation}/status` reports
   `ready`, `staging_index`, `active`, `superseded`, or `failed` with bounded
   diagnostics and no paths outside the published scope.

All JSON routes install explicit body limits before deserialization. Upload ids
are unguessable and still require the same producer token and scope binding.
The store permits at most the configured number of open uploads per producer,
one indexing activation per project, and one finalization decision at a time
under a project-scoped lock. A newer server-issued upload ordinal may supersede
an older ready generation; an older request can never reactivate after a newer
one becomes desired.

Upload sessions are durable for restart and expire after 24 hours of inactivity.
Expiry removes only temporary upload state, never installed blobs or an
immutable ready/active generation. The collector has no spool: its checkout is
the durable backlog and a fresh scan can recreate any expired upload.

## 7. Collector filesystem contract

Collector configuration lists explicit project roots. Before every scan it:

1. Canonicalizes the configured path and confines every later operation below
   it.
2. Requires a Git repository and rejects linked worktrees. A monorepo
   subproject is allowed, but its containing Git worktree must be the main
   worktree for that clone.
3. Resolves the committed published scope at `HEAD` and requires it to match the
   configured expected scope. Configuration cannot silently follow a repo-id
   change.
4. Walks with the shared policy, including Git ignore rules, hidden-directory
   exclusion, `.bbox`, `target`, `node_modules`, `_build`, and `.worktrees`.
5. Uses `symlink_metadata`, never follows symlinks, and emits bounded counters
   for skipped symlinks, special files, unsupported paths, oversize files, and
   read races.
6. Opens and hashes each regular file under the confined root. The manifest
   captures the exact bytes hashed, not mtime and size.

When the server requests a missing blob, the collector reopens the path with
no-follow semantics, verifies it is still the same regular file under the root,
streams it while hashing, and uploads it only if its hash and size still match
the manifest. A mismatch abandons the upload and starts a new scan. This closes
the scan-to-upload race without pretending the checkout is frozen. A cached
server blob may activate the previously observed bytes even if the checkout
changes just after scanning; the next scan publishes the new state.

`bbox-code-collector once` publishes each configured root once and waits for a
terminal generation status. `bbox-code-collector run` repeats with bounded
exponential backoff and jitter, one scan per root at a time. Logs report scope
hash, generation prefix, counts, bytes, and state, never bearer values or file
contents. Normal shutdown may abandon an upload because replay is safe.

## 8. Durable corpus store

All state derives from `Config.paths.state_dir`:

```text
code-sources/
  blobs/sha256/<first-two>/<hash>
  uploads/<producer-hash>/<upload-id>/...
  scopes/<scope-hash>/generations/<generation-id>/manifest.jsonl
  scopes/<scope-hash>/generations/<generation-id>/metadata.json
  activations/<project-id>.json
```

Directories and files are private. Blob installation and JSON replacement use
same-filesystem temporary files, file fsync, atomic rename, and parent-directory
fsync. Existing blobs are accepted only after size and hash verification; a
corrupt cache entry is quarantined and requested again. Generation manifests
are immutable after completion.

The active read authority is not `activations/*.json`. It is the edge
sidecar's existing atomic `manifest-index.json`, extended so each workspace row
may carry the active `code_source_generation` and `code_source_selector` beside
its active edge snapshot. One atomic manifest-index replacement therefore
selects both the search generation and graph snapshot. Activation journals are
recovery records, not competing authority.

Blob loss degrades affected ready generations to `missing_blobs`; the next
manifest negotiation requests those hashes again. The server never marks an
incomplete generation active. Garbage collection marks blobs referenced by
open uploads, ready generations, the active generation, and the configured
number of retained prior generations. It sweeps only unmarked blobs older than
the grace period, under a store GC lock, and reports reclaimed bytes. No
automatic sweep may delete the sole active generation manifest.

## 9. Corpus-side source abstraction and indexing

### 9.1 Project-file sources

Refactor project indexing around a read-only source snapshot:

```text
ProjectFileSourceSnapshot {
    source_kind: local | collected,
    project_id,
    published_scope,
    selector,
    snapshot_id,
    head_commit,
    entries: ordered (relative_path, content_hash, size),
    open_bytes(relative_path, content_hash),
}
```

The local adapter walks and opens the registered root. Its selector is
`local:<project_id>` and its snapshot id follows the existing clean/dirty
fingerprint rules. The collected adapter reads one immutable manifest and opens
only verified content-addressed blobs. Its selector is
`collected:<project_id>:<generation-id>`, and its snapshot id is the generation
id folded with the current indexer, chunker, and entity parser versions.

Everything after byte acquisition is shared: format sniffing, chunking,
bounding, symbol table construction, Tantivy document construction, embedding
enqueue, and derived edge construction. Collected paths are joined to the
currently registered canonical root only as a compatibility display path. The
join is lexical after relative-path validation and is never opened. This slice
does not persist a producer absolute path or create a nonexistent project root.

Turn on `ProjectFileV2` and `SymbolV2` for collected generations regardless of
`BBOX_PROJECT_REFS_V2`. Their snapshot id names the exact materialization.
Legacy local behavior remains compatible during overlap.

### 9.2 Active search selector

Add exact Tantivy fields `code_source_selector` and
`code_source_generation`. Every newly built project-file document has both.
The schema version bump performs a full rebuild so local project-file documents
also receive `local:<project_id>` selectors.

All lexical code search, code-symbol search, hybrid search, and project-file
result expansion use one active-selector snapshot. The project-file clause is:

```text
doc_type != project_file OR code_source_selector IN active_selectors
```

This is a Tantivy query constraint, not only a post-filter, so stale generations
cannot crowd active results out of TopDocs. Vector candidates use the same
snapshot to reject inactive ProjectFileV2 and SymbolV2 refs, with bounded
overfetch until the requested active-result limit or candidate exhaustion.
Non-project-file corpus lanes are unaffected.

The daemon publishes an immutable `CodeReadView` containing the active selector
map and an `Arc<EdgeIndex>` built from the same prospective manifest index.
Search and graph handlers capture one view at request start. Existing direct
`state.edge_index` reads migrate to that view. This makes one request internally
generation-consistent without a global request lock.

All code paths that rebuild the in-memory edge index, including explicit-edge
append, background sidecar detection, local project reindex, and startup, must
publish a replacement `CodeReadView` rather than mutate a separate edge-index
lock. Direct inspection of an explicitly supplied historical V2 ref may remain
available, but discovery and result expansion use the captured active view and
cannot surface staged or retired refs.

### 9.3 Generation activation

Finalization does not immediately change readers. The indexing actor performs:

1. Revalidate the desired generation, every referenced blob, materialization
   version, and project/source assignment.
2. Chunk the immutable source snapshot and build new selector-stamped Tantivy
   documents and an inactive edge snapshot. Do not delete the previous active
   selector yet.
3. Commit the staged Tantivy documents in one writer commit. Repeated staging
   is safe because it first deletes only the exact new selector term.
4. Build a prospective `EdgeIndex` from an in-memory manifest index selecting
   the new snapshot, and build the corresponding active-selector map.
5. Under the project activation lock, confirm the generation is still newest,
   atomically replace `manifest-index.json`, then atomically swap the in-memory
   `CodeReadView`. Requests that began earlier retain the old complete view;
   later requests receive the new complete view.
6. Mark the journal active and asynchronously delete project-file documents,
   vectors, and sidecar snapshots older than the retention policy by their
   exact selector or snapshot id.

The persisted manifest index is written before the in-memory swap. A crash
before that write boots the old generation. A crash after it boots the new
generation. Startup reconstructs `CodeReadView` from that one file and resumes
or discards activation journals by checking whether the staged selector exists
and whether the manifest index names it. No recovery decision relies only on a
client acknowledgement.

`manifest-index.json` is a multi-project file, so per-project locks alone are
not sufficient. Every read-modify-write of it routes through one manifest
coordinator owned by the serialized indexing actor. Local snapshot updates,
collected activation, cutback, and background edge rebuilds cannot write the
file independently. The coordinator preserves unrelated workspace rows and
publishes the matching `CodeReadView` after each successful replacement.

Embeddings are not an activation prerequisite. Lexical search and graph become
active together; vectors arrive asynchronously and inactive old vectors are
filtered. Embedding retry already derives from indexed documents.

### 9.4 Reindex and purge integration

An ordinary incremental pass chooses exactly one source per project from the
active manifest and desired source mode. A collector-active project does not
call `scan_project_files` or open its source paths. Collected documents do not
use filesystem `_meta.json` keys, so the global deleted-file sweep cannot purge
them accidentally.

A full rebuild deletes all documents, then reconstructs only:

- the active collected generation for collector-owned projects;
- the current local snapshot for local-owned projects;
- existing non-project-file corpus lanes and provisional knowledge documents.

If a ready generation was staged but not active, a full rebuild may discard its
staged documents. The activation reconciler detects the missing selector and
restages it. The active generation remains reconstructible from immutable
manifests and blobs.

Git-history indexing stays a separately named local phase in this slice. It is
not fed a synthetic checkout path or a collected current-chunk map. Health and
reindex summaries report local-history and collected-current-file work
separately so the remaining checkout dependency is visible.

## 10. Overlap, cutover, and rollback

Each configured scope has a state derived from desired configuration and the
active manifest:

- `local`: no collector assignment; local file walking is active.
- `warming`: collector assignment exists, but no collected generation has
  activated; local walking and local selector remain active.
- `collected`: the active manifest names a collected generation; local
  project-file walking is disabled for that project.
- `cutback_pending`: the assignment was removed or explicitly disabled, but a
  fresh local generation has not activated; the last collected generation
  remains visible.

The first collected activation performs the source switch. A dead or stale
producer never falls back automatically. Health reports last manifest receipt,
last activation, generation, file count, bytes, missing blobs, activation
failure, and staleness against a configurable warning threshold.

Cutback is explicit through configuration. When a collected scope becomes
local-owned, the daemon validates the registered root, builds a complete local
generation through the same staging path, and switches the combined manifest
only after success. Failure leaves the collected generation active and reports
`cutback_pending`. Removing credentials alone never makes half-indexed local
bytes authoritative.

During rollout, local and collected generations may coexist physically, but
the selector and edge manifest expose only one. After at least one successful
collected refresh and one full rebuild rehearsal, the operator may leave the
scope collected. The old local adapter remains for unassigned scopes and
cutback; there is no daemon runtime role or remote/local tool mask.

## 11. Error, concurrency, and observability contract

HTTP errors use stable codes and appropriate statuses:

- `unauthorized` is 401 with no producer or scope detail;
- `scope_forbidden` is 403;
- registration, policy, path, digest, ordering, and state conflicts are 409 or
  422 with bounded diagnostics;
- body, file, manifest, concurrency, or logical-byte limits are 413 or 429;
- transient storage and indexing failures are 503 and preserve prior active
  state.

Handlers do not hold `SharedState` locks across body reads, hashing, filesystem
I/O, chunking, Tantivy commits, or edge-index construction. Blocking work runs
off Tokio workers. Store operations use per-upload and per-project locks with a
fixed order: auth snapshot, upload lock, project activation lock, serialized
manifest coordinator, store GC lock. No code takes those locks in reverse.

Metrics and logs cover authenticated request counts, rejects by code, manifest
entries and logical bytes, cache hit/miss bytes, upload duration, activation
duration, active-generation age, corrupt blobs, GC, and local versus collected
project counts. Labels use producer id and scope hash, never token, absolute
path, repo-id value, or content hash at unbounded cardinality.

## 12. Implementation phases

### Phase 0: Shared policy and index prerequisites

1. Add `bbox-code-source` and move current project-file extension, directory,
   and byte caps behind its versioned policy.
2. Make the local walker use no-follow metadata and the shared policy without
   changing its observable file set.
3. Add source selector/generation schema fields, active-selector query
   constraints, vector filtering, and `CodeReadView` plumbing.
4. Make collected refs unconditionally V2 and add generation-specific delete
   helpers.
5. Prove a schema rebuild preserves local search and graph behavior.

### Phase 1: Durable store and authenticated server API

1. Add strict producer configuration and atomic auth-table load/reload.
2. Add `bbox-code-source-store` with blob, upload, manifest, journal, recovery,
   and bounded GC primitives.
3. Mount the dedicated routes with per-route body limits and bearer
   authentication.
4. Add begin, paged manifest, missing-hash, blob, finalize, and status handlers.
5. Add fault-injection tests at every fsync/rename and finalization boundary.

SIGHUP handling performs the auth-table replacement and schedules any source
transition only after the new resolved config is valid. A failed reload leaves
both the previous auth table and previous desired-source table in force.

### Phase 2: Source-neutral indexing and activation

1. Extract `ProjectFileSourceSnapshot` and adapt the local indexer.
2. Add the collected generation adapter and immutable blob readers.
3. Add writer-actor staging acknowledgement and the combined manifest/read-view
   activation sequence.
4. Integrate active sources into incremental scan, full rebuild, purge,
   embedding, edge rebuild, startup recovery, and storage GC.
5. Split current-file and Git-history phases in summaries and control flow.

### Phase 3: Thin producer and overlap controls

1. Add the collector config, root confinement, committed-scope check, main
   worktree gate, manifest scan, missing-blob upload, status poll, and run loop.
2. Add dependency acceptance and token-redaction tests.
3. Implement local, warming, collected, and cutback-pending transitions.
4. Add health/doctor output and operator documentation.
5. Exercise a local-to-collected-to-local round trip without changing entity
   content or exposing two generations.

## 13. Validation gates

Unit and integration coverage must include:

- scope allowlist, duplicate assignment, zero/duplicate registration, token
  safety, constant-time verify, reload retention, redirect refusal, and HTTPS
  client policy;
- every invalid path form, duplicate/out-of-order manifest entry, policy skew,
  count/byte cap, conflicting page replay, wrong upload owner, stale cursor,
  missing blob, wrong size/hash, corrupt cache entry, and expired upload;
- scan-to-upload mutation, symlink swap, linked-worktree rejection, ignored
  directories, large supported document caps, and unsupported files;
- finalize replay, out-of-order generations, concurrent finalize, restart at
  every upload/activation journal state, blob-cache loss, GC versus active/open
  references, and disk-write failure;
- staged generations absent from lexical, code-symbol, hybrid-vector, inspect
  expansion, and graph discovery; old requests retaining the old read view;
  new requests receiving the new selector and edge snapshot together;
- incremental and full rebuild from an active collected manifest, no local file
  open in collected mode, no global purge of collected docs, materialization
  version bump, and failed activation preserving old results;
- warming behavior, explicit cutback success, cutback failure preserving the
  collected generation, staleness reporting, and no implicit failover;
- collector dependency ceiling and a binary smoke test against a temporary
  HTTPS or loopback server.

Repository gates:

```text
scripts/fmt.sh --check
scripts/acceptance-code-collector-deps.sh
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo nextest run --workspace --profile full
```

Run the full gate through the project lane workflow. The implementation review
starts from a new Kimi session after the branch is committed and pushed. The
same implementation-review session is resumed after each correction until it
returns `PASS`.

## 14. Completion and deferred gates

This slice is complete when one configured base root can publish, activate,
refresh, survive restart/full rebuild/cache loss, and cut back while search and
graph readers expose one generation and the producer dependency ceiling holds.
The daemon must perform no project-file walk for that scope in `collected`
state.

The next decomposition gate remains explicit: replace the overlap-only
requirement for a local `ProjectRecord` with a durable corpus project catalog
and host-local checkout attachments. That design must migrate project
selection, display paths, repo-owned knowledge publication, aliases, Git
history, blame, and any host-path-bearing API together. Only after that gate,
plus the already deferred checkout-local surfaces, may the corpus be called
mount-free or moved off-host.
