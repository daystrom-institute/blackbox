---
title: "Remote Source Connectors"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - connectors
tags:
  - connectors
  - mounts
  - gdrive
  - onedrive
  - sharepoint
  - icloud
  - indexing
  - multimodal
brief: "Pluggable connectors onto remote file/document stores so drives and folders mount as registered, indexable blackbox projects; materialize-then-index architecture riding the existing chunker/multimodal pipeline."
date: 2026-07-14
---

# Remote Source Connectors

Status: proposed

## Thesis

Blackbox should be able to treat a remote drive, or a folder inside one, as a
**mountable project**: registered like a local project, indexed by the
existing pipeline, searchable through `bbox_hybrid_search` and the graph
tools. The unit of work is a **connector**: a pluggable adapter that knows
how to enumerate, fetch, and watch one kind of remote store (Google Drive,
OneDrive/SharePoint, iCloud Drive, WebDAV, S3-compatible, ...).

Two facts discovered while grounding this design shape everything below:

1. **The multimodal problem is already solved downstream.** The chunker
   registry (`crates/bbox-chunker/src/lib.rs`, ordered first-match
   `Vec<Box<dyn SourceFormatChunker>>`) already handles PDF (text + OCR +
   embedded figures), DOCX/PPTX, XLSX-family, IPYNB, HTML, AV transcripts,
   and standalone images, with visual payloads content-hash-addressed in
   `bbox-visual-store` and embedded through the `voyage_visual` route
   (`multimodal-embedding-routing.md`). A connector's job is to deliver
   bytes with stable identity and change signals; everything after that
   exists.
2. **The indexing pipeline is local-fs-hardcoded but cleanly seamed.**
   `scan_project_files` walks with `ignore::WalkBuilder` and reads with
   `std::fs::read` (`crates/bbox-corpus-index/src/index/project_files.rs`);
   freshness is mtime + size + materialization version
   (`classify_project_file`); project identity is a hash of the
   canonicalized local path (`entity_ref::project_id_for_path`, via
   `ProjectRegistry::register_path_locked`,
   `crates/bbox-indexing/src/projects.rs`). There is no file-source trait
   today.

Given (1) and (2), the v1 architecture is **materialize-then-index**: a
connector syncs the remote scope into a local materialization root, and that
root is registered as a normal project. The walker, chunkers, tantivy
schema, graph projection, embedding queues, and code-nav tools all operate
untouched. A streaming `FileSource` abstraction (index-without-materialize)
is recorded as the explicit v2 seam, not built now.

## Vocabulary

| Term | Meaning |
|---|---|
| `connector` | Adapter implementation for one remote store kind (`gdrive`, `graph`, `webdav`, `s3`, `local_mirror`, ...) |
| `mount` | One configured binding: connector + remote scope + credentials + policy, materialized at one local root |
| `remote scope` | The drive, folder subtree, site, or bucket/prefix a mount covers |
| `materialization root` | The local directory a mount syncs into; what gets registered as the project |
| `change cursor` | Connector-opaque token for incremental enumeration (Drive changes pageToken, Graph deltaLink, etag walk, ...) |

A mount maps 1:1 to a registered project. Folder-scoped mounts of the same
drive are distinct mounts and distinct projects.

## Architecture

```
 remote store ──(connector: enumerate/fetch via change cursor)──▶
   materialization root ──(existing walker/chunkers/indexer)──▶
     tantivy + graph + embed queues (text and visual lanes)
```

### Why materialize-then-index (and not a FileSource trait first)

- **Blast radius.** The trait seam touches every direct `fs::read` /
  `WalkBuilder` call in `project_files.rs`, the freshness classifier, and
  the git/language probes in `register_path_locked`. Materialization
  touches none of them.
- **Freshness semantics come free.** The existing classifier compares
  stored mtime/size; a materializer that writes atomically (temp file +
  rename) and preserves content-change-only mtime bumps plugs into it
  exactly.
- **Every downstream tool works.** `bbox_code_symbols`, refactor status,
  `isolate` cells, evidence bundling with real paths, operator `grep` in
  the mount root: all of these want real files.
- **Offline behavior.** Search and graph queries keep working when the
  remote (or its auth) is down; only sync staleness accrues.
- The cost is disk and sync latency, and the disk cost has two honest
  wrinkles. First, visual content is stored twice: once in the mount and
  once as content-hash payloads in `bbox-visual-store`, which currently
  has **no garbage collection** (its module doc records the deferred
  refcount/mark-sweep follow-ups), so remote deletions and mount removal
  strand derived payloads until that lands. Second, policy can skip most
  fetches from enumeration metadata alone, but native-doc **export sizes
  are unknown before fetching**; export fetches therefore run under a
  streamed hard byte cap that aborts an oversized export mid-transfer
  rather than pretending metadata screening was enough. Size-capped,
  filter-scoped mounts keep the remaining cache proportional to what is
  actually indexable.

### The connector interface

Home: a new `crates/bbox-connectors` crate (daemon-side; sync workers run
under the daemon runtime's actor discipline, no blocking calls on tool
paths). Registry follows the house config-alias-with-`type` pattern
(embed providers) plus a typed adapter registry (transcript adapters,
`crates/bbox-corpus-index/src/transcripts/adapters.rs`).

```rust
#[async_trait]
pub trait RemoteSourceConnector: Send + Sync {
    /// Connector kind discriminator, e.g. "gdrive", "graph", "webdav".
    fn kind(&self) -> &'static str;

    /// Validate config + credentials; return remote identity/quota info
    /// for status surfaces. Fail-closed with remediation text.
    async fn validate(&self, mount: &MountConfig) -> Result<RemoteInfo>;

    /// Enumerate entries. `cursor=None` is a full walk of the scope;
    /// `Some` resumes incrementally. Returns entries + the next cursor.
    /// Entries carry: stable remote id, path within scope, size,
    /// remote version (etag/revision/mtime), kind (file/dir/native-doc),
    /// and media type when the store knows it.
    async fn list_changes(
        &self,
        mount: &MountConfig,
        cursor: Option<ChangeCursor>,
    ) -> Result<ChangeBatch>;

    /// Fetch one entry's bytes as a stream. For provider-native documents
    /// the connector applies the export mapping (see below) and reports
    /// the exported media type.
    async fn fetch(
        &self,
        mount: &MountConfig,
        entry: &RemoteEntry,
    ) -> Result<FetchedContent>;
}
```

Deliberate omissions: no `write`/`delete` (read-only invariant, below), no
watch/subscription API in v1 (polling via cursors; push notifications are a
per-connector optimization later).

### Provider-native documents: export to what the chunkers already eat

Google Docs/Sheets/Slides (and SharePoint list-like content) have no
canonical bytes. Connectors declare an **export map** onto formats the
chunker registry already claims:

| Native kind | Export | Existing chunker |
|---|---|---|
| Google Doc | `.docx` | `office.rs` |
| Google Sheet | `.xlsx` | `xlsx.rs` |
| Google Slides | `.pptx` | `office.rs` |
| Drawings/other | `.pdf` | `pdf.rs` |

The materialized filename carries the exported extension; the remote version
still comes from the native document's revision, so re-export happens only
on remote change. This is the single highest-leverage consequence of the
materialize-then-index choice: zero new chunkers for the office-suite
corpus.

### Mounts, identity, and the project registry

New persisted store (sibling of `ProjectStore`,
`crates/bbox-indexing/src/projects.rs`; env override
`BLACKBOX_MOUNTS_PATH`):

```rust
pub struct MountRecord {
    pub mount_id: String,          // hash of connector kind + remote identity + scope
    pub connector: String,         // config alias, e.g. "gdrive-personal"
    pub remote_scope: String,      // connector-shaped scope string
    pub materialization_root: String,
    pub project_id: String,        // the registered project this mount feeds
    pub cursor: Option<ChangeCursor>,
    pub policy: MountPolicy,
    pub created_at: String,
    pub last_sync: Option<SyncSummary>,
}
```

- The default materialization root is
  `<state_dir>/blackbox/mounts/<mount_id>/` (operator-overridable at
  registration for "put it on the big disk" cases).
- Registration flow: create/validate mount → initial sync (or lazy first
  sync) → register the materialization root through the normal
  `ProjectRegistry` path, so `project_id` derivation, alias support, and
  `resolve_project_context` (the single resolver, per
  `crates/bbox-indexing/AGENTS.md`) are all standard.
- `ProjectRecord` gains a `source` field
  (`ProjectSource::LocalFs` default | `ProjectSource::RemoteMount { mount_id }`),
  serde-defaulted so existing stores parse unchanged. Consumers that assume
  git (repo probes, provenance) already handle `repo_id=None`; mounts are
  simply non-git projects. `detect_languages` runs on materialized content
  as-is.
- Unregistering a mount unregisters the project (same attached-state
  refusal semantics as `bbox_project_unregister`) and optionally removes
  the materialization root (explicit flag; default keeps bytes).
- Logical identity across hosts follows the project-taxonomy design's
  alias layer: `project_id` stays host-scoped (it hashes the local
  materialization path); the mount's remote identity (drive id + scope) is
  recorded on `MountRecord` and can back a future cross-host alias
  convergence, mirroring what `repo_id` does for git projects
  (`project-taxonomy-standardization.md`).

### Sync engine and freshness

One shared sync driver owns the loop; connectors only enumerate and fetch.
The driver's durable heart is a **per-mount materialization manifest**,
keyed by remote ID:

```rust
pub struct ManifestEntry {
    pub remote_id: String,        // connector-stable identity
    pub remote_version: String,   // etag/revision/changestamp
    pub logical_path: String,     // scope-relative display path
    pub physical_path: String,    // materialization-root-relative (encoded)
    pub export_format: Option<String>, // for native-doc exports
    pub content_hash: Option<String>,  // sha256 of materialized bytes
    pub state: EntryState,        // materialized | skipped(reason) | pending
}
```

State invariants, not sentinels: `content_hash` (and mtime-bearing
materialization metadata) is `Some` exactly when `state = materialized`;
`skipped` and `pending` entries carry `None`.

The manifest is what a bare cursor cannot be: deletion tombstones map back
to local paths through it, renames reconcile as (same remote_id, new
logical_path) moves instead of delete+refetch, and a full walk diffs
against it to find local orphans. It persists next to the mount record
(same store discipline as `ProjectStore` snapshots).

A sync pass, under a per-mount single-flight lock:

1. `list_changes(cursor)` → change set relative to the scope.
2. Policy filter (globs, size caps, kind allowlist) applied on metadata;
   excluded entries are recorded in the manifest as skipped, never fetched.
3. Fetch changed entries; write atomically (temp + rename) into the
   materialization root; set file mtime only when content actually changed
   (manifest `content_hash` compare). Deletes and renames resolve through
   the manifest.
4. Manifest updates journal per batch: entry states commit as each batch
   completes, and the cursor advances only with its batch's manifest
   commit, transactionally. A crash mid-sync therefore resumes from the
   last committed batch; completed entries are not re-fetched because the
   manifest already records their `remote_version`.
5. Nudge the existing reindex path for the project (the same
   registration-time `ReindexConfig` machinery `bbox_project_register`
   uses, `src/tools/projects.rs`); the walker then sees ordinary
   mtime/size changes and `classify_project_file` does the rest.

Scheduling rides the existing poller substrate (`pollers.rs`,
`BBOX_POLLER_MIN_INTERVAL_SECS` floor): each mount declares
`sync_interval_secs` (default 900), and a manual `bbox_mount_sync` forces a
pass. Full-walk resync (`full=true`) discards the cursor, mirroring
`bbox_reindex(full=true)` semantics. Cursor invalidation (Drive/Graph
expire tokens) degrades to a full walk automatically, with the degradation
reported in `SyncSummary`, never silently ignored.

### Materialization identity and path safety

Remote namespaces are not filesystems; the materializer never trusts a
remote name as a local path:

- **Deterministic physical-path encoding.** `physical_path` derives from
  the sanitized logical name plus, on collision, a short suffix from the
  remote ID. Collisions are broader than Drive's duplicate sibling names:
  the local filesystem may be case-insensitive and Unicode-normalizing
  (APFS), so two distinct remote names can collapse onto one local path.
  Collision detection therefore compares case-folded, NFC-normalized
  candidates against the manifest, not raw strings. The manifest keeps the
  faithful `logical_path` for mount-layer display surfaces (status,
  summaries, remote-URL rendering); the encoding is reversible through it.
  Honest v1 limit: the indexer sees only the encoded physical path (the
  pipeline stays untouched, per the architecture decision), so
  path-token search matches the encoded name where a collision suffix was
  applied; projecting `logical_path` into index metadata is follow-up work
  coupled to the "graph identity for remote provenance" open question, not
  assumed here.
- **Containment.** Every physical path is verified root-contained after
  encoding (no `..` traversal, no absolute components, platform reserved
  names rejected). Connector-supplied symlink entries are refused; the
  materializer only ever creates regular files and directories.
- **Ownership marker.** The driver stamps the materialization root with a
  marker file at creation and refuses destructive operations (mount
  removal with `remove_files`, full-resync pruning) on any root missing
  it, so a misconfigured path can never aim deletion at operator data.

### Policy and limits

`MountPolicy` per mount:

- `include`/`exclude` globs (evaluated on scope-relative paths).
- `max_file_bytes` (defaults aligned with the walker's existing
  `max_bytes_for_path` caps so nothing is fetched that indexing would
  refuse: images cap at `MAX_IMAGE_FILE_BYTES` = the multimodal provider
  limit, documents at `MAX_DOCUMENT_FILE_BYTES`).
- `native_export = true|false` (office-suite export on/off; default on).
- `max_total_bytes` per mount (sync aborts with a loud error rather than
  silently truncating; the error names the biggest offenders).

**Read-only invariant.** v1 connectors never mutate the remote: no writes,
no deletes, no permission changes. The trait has no mutating methods, which
prevents callers from requesting mutation; it does not prove an adapter's
internals never issue one, so the invariant is defense-in-depth: request
read-only OAuth scopes wherever the provider offers them (`drive.readonly`,
Graph `Files.Read.All`/`Sites.Read.All`), least-privilege credentials
otherwise, HTTP-method conformance assertions in adapter tests, and sync
telemetry that records every remote call class.

**Export posture.** Indexing is local; bytes leave the host only through
the already-designed embedding lanes (text to the configured text routes;
pixels only for visual chunk kinds under the opt-in
`[embed.routes.visual]` policy, `multimodal-embedding-routing.md`
"Pixel-export policy"). Mounting a remote store adds no new export path.

### Credentials

Connector auth is entirely delegated to the secrets layer
(`design/operations/config-artifacts/secrets-provider.md`, the prerequisite
this design forced). Connector config carries secret references, never
values:

```toml
[connectors.gdrive-personal]
type = "gdrive"
# OAuth client for a native app flow, or a service-account JSON ref.
oauth_client_secret = "op://blackbox/gdrive-oauth/client-secret"
# Rotating refresh-token home: an explicit writable TokenStore ref
# (file://<name> = managed secrets dir), declared writable.
token_store_ref = "file://gdrive-personal-token"
token_store_writable = true

[connectors.team-sharepoint]
type = "graph"
tenant = "<tenant-id>"
client_secret = "op://blackbox/graph-app/client-secret"
```

OAuth specifics per connector (device-code vs PKCE, refresh cadence) are
connector-internal; the shared rule is that durable tokens live behind
secret refs, with rotating refresh tokens written through the secrets
layer's explicit-writable `TokenStore` contract (one unambiguous writable
ref, no write-path chains), and never in `MountRecord`, mount config,
logs, or fleet.json. The `shell_env` non-secret lane invariant is
untouched. Dependency note: the network connectors (phases 2+) require
the secrets design's phases 1-3 (registry, 1Password provider, and
`TokenStore` adoption); connector phase 1 (`local_mirror`) needs none of
them.

### MCP surface

A dedicated cluster (per the namespace convention: general tool families
get `bbox_*`; `work_*` is reserved and not used here):

- `bbox_mount_register(connector, scope, path?, policy?, project_alias?)` -
  validate, create mount, initial sync (async, like registration-time
  indexing today), auto-register the project.
- `bbox_mount_list()` - mounts with sync state, staleness, last errors.
- `bbox_mount_sync(mount, full?)` - manual sync pass.
- `bbox_mount_unregister(mount, remove_files?, force?)` - list-before-create
  and attached-state refusal semantics mirror the project tools.

`bbox_project_list` output grows the `source` marker so mounted projects
are distinguishable. `bbox_stats`/doctor surfaces report per-mount
freshness alongside index freshness.

## Connector catalog

Ordered by intended delivery; each is one adapter behind the trait, so the
catalog grows by demand without core changes.

1. **`local_mirror` (day one, no network code).** Many stores already
   maintain a local mirror via their desktop clients (iCloud Drive at
   `~/Library/Mobile Documents/com~apple~CloudDocs`, Google Drive for
   desktop, OneDrive, Dropbox). This connector "syncs" by hydrating and
   hardlinking/copying from the mirror under the same policy engine -
   solving the **dataless placeholder** problem (evicted files carry the
   APFS dataless flag and block on on-demand download when read, or fail
   offline; the connector detects the flag and triggers bounded, explicit
   hydration via the CloudDocs daemon, e.g. `brctl download`, instead of
   letting the indexer fault files in implicitly). This is also the only
   sane iCloud path: iCloud Drive has no public API; rclone's
   `iclouddrive` backend is experimental web-session SRP auth (real Apple
   ID password + 2FA, app-specific passwords rejected, trust token
   expiring roughly monthly, Advanced Data Protection gated), workable
   only as a monitored fallback lane with human re-auth alerts on
   Mac-less hosts. On a signed-in Mac the mirror IS the integration.
2. **`gdrive` (Google Drive API).** Changes API for cursors, `files.export`
   for native docs, file-ID addressing (exact under Drive's duplicate-name
   model). Auth realities that shape the flow: Google's device-code grant
   supports only the narrow `drive.file`/`drive.appdata` scopes, not the
   broad read scopes whole-drive indexing needs, so the interactive leg
   is a loopback/installed-app flow (or a service account where domain
   delegation fits), and an external OAuth app must be pushed to
   **Production** status or refresh tokens are revoked after 7 days in
   Testing mode (Workspace-internal apps are exempt from that rule).
   First real network connector because the export-map payoff is largest.
3. **`graph` (Microsoft Graph: OneDrive + SharePoint).** One connector for
   both: drives and site document libraries are the same driveItem/delta
   surface (`graph-rs-sdk` covers both, with built-in flows). Delta links
   for cursors; app-only (client credentials) for org tenants, delegated
   with `offline_access` for personal (refresh tokens carry a 90-day
   rolling window, so a regular sync cadence normally keeps them alive,
   subject to revocation and tenant policy). Device-code is fully
   supported here, unlike Drive's broad scopes.
4. **`webdav` / `s3`.** Etag-walk cursors (no delta APIs); mostly useful
   for self-hosted stores (Nextcloud, Garage/MinIO). Low complexity,
   covers the long tail.

### Build-vs-adopt for the connector internals

The trait is ours either way; the question is what implements the per-store
plumbing inside each adapter. Grounded against the ecosystem as of
2026-07-14 (versions verified against crates.io/vendor docs):

- **Apache OpenDAL** (`opendal` 0.58, Apache-2.0, tokio-native) is strong
  for object stores and WebDAV (s3/gcs/azblob/webdav first-class, dropbox
  and gdrive/onedrive supported with caveats) and its layer stack
  (retry/timeout/throttle/metrics) is exactly connector-shaped. Hard
  limits for this design: its **iCloud backend was removed** (v0.54, lack
  of maintainers), it has **no SharePoint/Graph backend at all**
  (onedrive = personal only), it models **no change/delta feeds** (our
  cursor machinery needs the native Changes/delta APIs regardless), and
  its gdrive path addressing is **heuristic under Drive's duplicate-name
  model** (most-recently-modified wins), which can silently shadow files
  in a path-addressed index. Auth-wise it does not run the interactive
  OAuth first leg but DOES auto-refresh given
  `refresh_token`+`client_id`(+`client_secret`); app registration,
  consent UX, and the refresh-token vault are ours either way. Verdict:
  adopt *inside* the `webdav`/`s3` adapters (where etag-walk cursors are
  the plan anyway), not for the flagship drive connectors.
- **Native SDK crates** carry the flagship adapters because cursors and
  export live there: `graph-rs-sdk` (3.x; the only substantive Rust Graph
  client, covers OneDrive AND SharePoint drives/sites, built-in
  device-code/PKCE/client-credential flows with auto-refresh; pin majors,
  it breaks) and for Drive either `google-drive3` (generated, current) or
  raw Drive REST over the `oauth2`/`yup-oauth2` crates, using **file-ID
  addressing** (exact under duplicate names) plus the Changes API.
- **rclone as a sidecar** (MIT; 70+ backends incl. iCloud) is confined to
  a fallback lane for stores nothing else reaches. If ever run: drive it
  via the `rcd` localhost JSON API, **pinned ≥1.73.5**
  (CVE-2026-41179, a critical unauthenticated RCE in the RC API affecting
  1.48.0-1.73.4), loopback-only with auth, and never as a FUSE mount for
  indexing
  (macFUSE still needs a kernel extension on Apple Silicon; rclone's
  kext-less NFS mount exists but mounting is the most fragile lane).
- **object_store** (Arrow) is architecturally flat/S3-shaped with zero
  consumer-drive support and no roadmap for it; not applicable beyond the
  object-store legs OpenDAL already covers.

Decision rule: adapters own identity, auth, cursors, and export; adopt a
library per adapter only where it demonstrably covers those or cleanly
slots under them. No adapter's dependency choice leaks past the trait.

## v2 seam: streaming FileSource (recorded, not built)

When a mount is too large to materialize (multi-TB team drives), the
alternative is a `FileSource` trait inside the indexer: an enumeration
stream whose entries carry stable id, logical path, remote version, size,
media type, and provenance (the chunkers and the tantivy schema need path
and naming, not just identity), plus `read(id)` returning a bounded async
reader (whole-object `Vec<u8>` would defeat the point at this scale), with
snapshot-vs-incremental semantics and deletion tombstones specified from
day one. `LocalFsSource` wraps today's `WalkBuilder`/`fs::read`; remote
sources stream straight into chunkers. Costs: the freshness classifier
grows a version/etag dimension beyond mtime/size; visual payloads and
code-nav lose real-file backing; every `fs::read` call site in
`project_files.rs` migrates. The trait shape is compatible with the
connector interface above (`list_changes`/`fetch` subsume it), so v1
connectors carry forward. Build only when a concrete oversized-mount
consumer exists.

## Phases

1. **Mount substrate + `local_mirror`.** `bbox-connectors` crate, trait,
   `MountRecord` store + `ProjectSource` field, sync driver + poller
   wiring, policy engine, `bbox_mount_*` tools, `local_mirror` connector
   with dataless-hydration handling. Gate: an iCloud Drive folder and a
   Google Drive desktop-mirror folder mounted, indexed, searchable, and
   incrementally re-synced on this host; multimodal content (PDF + images)
   from the mount retrievable via hybrid search.
2. **`gdrive`.** OAuth (secrets-layer refs), Changes cursors, native-doc
   export map. Gate: a folder-scoped mount of a real Drive with Docs/Sheets
   exported and chunked; cursor resume verified across daemon restarts;
   token refresh with no plaintext persisted outside the secrets layer.
3. **`graph`.** OneDrive personal + SharePoint site library behind one
   connector; delta cursors. Gate: same as phase 2 against both surface
   kinds.
4. **`webdav`/`s3` + catalog opening.** Etag-walk shared helper; document
   the "writing a connector" contract; evaluate OpenDAL adoption inside
   these adapters where its coverage is strong.

Phase 1 has no external-service dependency and no OAuth surface, which is
what makes it a safe substrate proof; phases 2+ are each one adapter plus
its auth flow.

## Acceptance criteria

- A mount registers, syncs, and appears as a normal project: hybrid search,
  graph inspection, and code-nav tools operate on materialized files with
  no connector-specific branches downstream of the sync driver.
- Incremental sync fetches only changed entries (verified against connector
  API call counts), and an expired cursor degrades to a full walk with the
  degradation reported, never silently.
- Policy exclusion prevents fetching (not merely indexing) excluded
  entries; per-mount byte caps abort loudly; native-doc exports run under
  a streamed hard cap that aborts oversized transfers mid-stream.
- Office-suite native documents are searchable via the existing
  office/xlsx/pdf chunkers with no chunker changes.
- Two remote entries whose names collide under case folding or Unicode
  normalization (or Drive duplicate siblings) materialize as distinct
  files, both reachable, with faithful logical paths in the manifest.
- No credential material appears in `MountRecord`, the manifest, mount
  config on disk, logs, or `SyncSummary`; connector auth round-trips
  through the secrets provider layer.
- Removing a mount leaves no orphaned project registry entry, and (when
  requested) no materialized bytes; destructive removal refuses a root
  missing the ownership marker. (Derived visual payloads persist until
  the visual store grows GC; documented, not silent.)
- Daemon restart mid-sync resumes from the last committed manifest batch
  without re-fetching entries whose `remote_version` is already recorded;
  a remote rename reconciles as a move, not a delete plus refetch.

## Open questions

- **Placement of the sync driver's heavy work.** Registration-time indexing
  already split light-lock vs blocking-pool phases
  (`src/tools/projects.rs`); sync workers likely mirror that split, but the
  exact actor topology (one worker per mount vs a shared pool) is a
  daemon-runtime question to settle at implementation.
- **Graph identity for remote provenance.** Whether `RemoteEntry` ids
  should be persisted into the graph (e.g. a `remote_ref` property on
  `project_file` entities) so evidence can cite the remote document URL,
  or whether the materialized path is identity enough for v1.
- **Mount-scoped embedding policy.** Whether a mount can override
  `[embed.routes.visual]` participation (e.g. "index this drive but never
  send its pixels to a hosted embedder") or whether the global opt-in is
  enough. Leaning per-mount `visual_embed = false` override; cheap and
  matches the export-posture principle.
- **Quota/backoff discipline.** Per-connector rate limiting is
  connector-internal in v1; if two mounts share one remote account, a
  shared per-account limiter may be needed (precedent: embed queue's
  conservative byte heuristics).
- **Long-tail extraction.** Remote corpora surface formats the chunker
  registry does not claim (email archives, legacy binary `.doc`/`.ppt`).
  The registry's answer today is "unclaimed files fall through"; if a
  mounted corpus makes the tail matter, the candidates are `extractous`
  (Tika compiled native via GraalVM, no JVM sidecar, but ~18 months
  stale) or a supervised Apache Tika server sidecar, adopted behind a new
  chunker rather than inside connectors. Demand-driven; explicitly not a
  connector concern.
