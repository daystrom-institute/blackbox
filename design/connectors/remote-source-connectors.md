---
title: "Remote Source Connectors"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - connectors
tags:
  - connectors
  - locality
  - collector
  - producer
  - gdrive
  - onedrive
  - sharepoint
  - webdav
  - s3
  - multimodal
  - indexing
brief: "Remote file/document stores become corpus-searchable through a connector satellite on a producer host: it observes the store via change cursors, applies policy, exports provider-native documents into formats the corpus chunkers already claim, and publishes manifest plus blobs over the collector wire. The daemon never fetches remote bytes and never materializes."
date: 2026-08-11
---

# Remote Source Connectors

> **Status: proposed; nothing here is implemented on `beta/blackbox-v2` as of
> 2026-08-11.** What IS implemented, and what this design extends rather than
> invents: the dependency-clean `bbox-code-collector`, the authenticated
> `/internal/code-source/v1/*` manifest-and-blob wire, the content-addressed blob
> cache, immutable generations with stage-then-flip activation behind an
> immutable `CodeReadView`, the `source_kind: local | collected` indexing seam,
> the durable catalog with `ProjectScope::Published | LegacyLocal`, and the
> collector-backchannel onboarding endpoint. The connector satellite, a
> connector-scoped catalog identity, and every adapter below are new work. The
> predecessor's mount substrate and its `crates/bbox-connectors` code (git and
> `local_mirror` adapters, mount store, sync driver) exist only on the diverged
> branches named in Relationship: salvage donor code, not shipped behavior. Phase
> 0 (identity) is a blocking operator decision. Reverify every contract name
> against code before building on this snapshot.

## 1. Thesis

A remote file or document store (Google Drive, OneDrive/SharePoint, WebDAV, an
S3-compatible bucket) becomes corpus-searchable by running a **connector
satellite** on a producer host. The satellite observes one remote scope through
that store's native change cursor, applies a policy engine on enumeration
metadata before fetching anything, exports provider-native documents into formats
the corpus chunker registry already claims, and publishes a manifest of
`(logical_path, content_sha256, size)` at a generation, uploading only the blobs
the server says it lacks.

The corpus side already exists for collected code sources: manifest negotiation
against the blob cache, chunking and indexing from an immutable manifest, one
stage-then-flip activation so readers see exactly one generation.

Two properties are the point:

- **The daemon never fetches remote bytes.** It holds no vendor credential and
  opens no socket to a third-party cloud. The only new network trust edge is
  producer-host-to-vendor, where the OAuth refresh token has to live anyway.
- **The daemon never materializes.** No daemon-local mount root, no project whose
  identity is a filesystem path. Where a connector wants a local cache at all
  (section 12) that cache is **producer working state**: losable, re-derivable,
  never corpus authority.

Everything downstream of byte acquisition exists: the chunker registry already
handles PDF (text plus OCR plus embedded figures), DOCX/PPTX, the XLSX family,
IPYNB, HTML, AV transcripts, and standalone images, with visual payloads
content-hash-addressed in `bbox-visual-store`. A connector's entire job is
delivering bytes with stable logical identity and honest change signals.

## 2. Reconciliation with the locality program

The predecessor was authored before the locality program (the
checkout-plane/corpus-plane split, roughly 926 commits of decomposition). It is
not patchable; its architecture was adjudicated against. The salvage is
substantial and the deaths are load-bearing, so both get named.

### 2.1 What died

- **Materialize-then-index.** Syncing a remote scope into a daemon-local
  materialization root and registering that root as a project.
  `locality-first-decomposition.md` kills the substrate by name ("No
  mount/connector substrate, no clone mirrors, no dead registry entries as
  identity bridges"; "Mirroring is a semantic downgrade dressed as a transport").
  The daemon runs in the cage on an immutable image digest with zero checkout
  filesystem authority.
- **The `git` and `local_mirror` connectors.** Four landed transports cover repo
  mirroring entirely: code collector (current bytes), Git history/provenance
  transport, knowledge-source transport for `.bbox/`, checkout-local blame and
  render. Cloning into the corpus host delivers strictly less (no uncommitted
  state, no worktrees, no dirty-buffer blame) while adding git-version,
  submodule, path-encoding, and ssh-credential archaeology to the daemon image.
- **Daemon-side sync workers.** The predecessor put the sync driver in the daemon
  under the poller substrate and left its actor topology open. Moving the driver
  to the producer dissolves the question: one scan at a time per scope, in a
  single-purpose process.
- **`bbox_mount_register` / `bbox_mount_sync` / `bbox_mount_unregister`.** An
  agent-triggered path that creates a durable source and triggers a third-party
  fetch is the shape `remote-project-onboarding.md` rejected. Lifecycle is
  two-sided operator config; the transport is find-or-create idempotent; callers
  read status and never trigger acquisition.
- **`MountRecord`, `mount_id`, `materialization_root` as identity.** Path-hash
  project identity is minted-or-compatibility-only and absolute paths are
  attachment observations, never identity. A `mount_id` hashing a local root is
  that defect renamed, as the predecessor's own note ("`project_id` stays
  host-scoped") admits.
- **`ProjectSource::RemoteMount { mount_id }`.** Source ownership is the active
  selector in the workspace manifest index (`local:` versus `collected:`), not a
  field on a registry row.
- **The recorded "v2 seam".** The deferred streaming `FileSource` trait shipped,
  better: the collected-source snapshot abstraction with
  `source_kind: local | collected`, content-hash freshness, and
  generation-scoped Tantivy fields. This design consumes that seam rather than
  proposing one.

### 2.2 What survives, relocated to the producer

Each of these was good engineering pointed at the wrong host:

- **The export map** for provider-native documents, reinforced rather than
  weakened by the collector's no-chunker-in-the-producer invariant (section 7).
- **Per-source manifest discipline.** The predecessor's `ManifestEntry`
  (`remote_id`, `remote_version`, logical path, content hash, skipped/pending)
  becomes a producer-side journal plus the wire manifest (section 10).
- **The change-cursor abstraction** per connector, invalidation degrading to full
  re-enumeration, degradation reported.
- **The policy engine**: include/exclude globs, per-file byte caps aligned to the
  shared walker policy, `native_export` toggle, per-scope total-byte cap aborting
  loudly and naming the biggest offenders.
- **Path and name safety plus collision handling**, now on logical manifest paths
  rather than local filesystem paths (section 12). The problem shrinks; it does
  not vanish.
- **The read-only invariant** and least-privilege OAuth scopes, with
  defense-in-depth beyond "the trait has no mutators".
- **The build-versus-adopt analysis**, carried forward with ecosystem claims
  marked as-of-2026-07 and flagged for reverification (section 16).
- **The visual-store no-GC caveat.** Refcount and mark-sweep are still deferred,
  so remote deletions and source removal strand derived payloads.
- **The ownership-marker pattern** for destructive operations (the salvage
  branch's `.bbox-mount-owner`), reused for the producer cache.

## 3. Vocabulary

| Term | Meaning |
|---|---|
| connector | Adapter for one remote store kind (`gdrive`, `graph`, `webdav`, `s3`), implemented inside the satellite |
| connector satellite | Producer-host binary running connectors: policy, export, publication |
| remote scope | The drive, folder subtree, site document library, or bucket prefix one source covers |
| connector source | One configured binding of connector plus remote scope plus policy plus credentials, publishing to one durable corpus scope |
| change cursor | Connector-opaque incremental-enumeration token (Drive changes page token, Graph delta link, etag walk state) |
| logical path | The validated scope-relative slash path a manifest entry carries; derived from the remote name, never trusted from it |
| producer cache | Optional producer-local bytes avoiding re-fetch or re-export; working state, never authority |
| generation | One immutable published manifest plus descriptor; the unit of activation |

A connector source maps to exactly one durable corpus scope. Folder-scoped
sources of the same drive are distinct sources and distinct scopes.

## 4. Architecture

```
producer host                              corpus host (the cage)
remote store
  | list_changes(cursor)
  v
enumeration metadata
  | policy filter (globs, caps, kind)
  v
fetch + native export --> producer cache
  | hash bytes
  v
manifest (path, sha256, size) ---------->  manifest negotiation
  | upload only missing hashes -------->   content-addressed blob cache
  v                                              |
publication status poll <----------------  chunkers, Tantivy, vectors, edges
                                                 |
                                           stage-then-flip activation
                                           (one CodeReadView)
```

The placement rule is the locality rule one axis out. The checkout plane became
"the machine owning the mutable working copy owns acquisition"; the connector
plane is "the machine holding the vendor credential owns acquisition". Both push.
The corpus host stays the only chunker, the only index, and the only authority on
what it still needs. Why the satellite and not the daemon: OAuth refresh tokens
in the cage would give the corpus host third-party egress plus a rotating-secret
write path, both deliberately removed by the decomposition; a connector source is
otherwise just a collected source, needing no new indexing rung, freshness
classifier, or activation path; offline behavior comes free, since only
publication staleness accrues when a vendor API is down and a dead producer falls
back to nothing by existing contract; and vendor archaeology (duplicate sibling
names, export quirks, delta-token expiry, tenant policy) stays in a process
redeployable independently of the immutable daemon image.

## 5. Where the connector satellite lives

A new sibling binary, `bbox-file-collector`, beside `bbox-code-collector`,
reusing the collector's leaf contracts and none of its corpus dependencies. It
depends on `bbox-code-source` (wire structs, error codes, relative-path
validation, manifest ordering and digest algorithms, byte-cap policy, policy
version constant), the dependency-clean `bro_rpc::ServiceToken` loader,
`bbox-config`, `reqwest`, and per-connector vendor SDKs. It must not depend on
Tantivy, `bbox-chunker`, `bbox-corpus-index`, `bbox-indexing`, `bbox-vectors`, the
edge index, `blackbox`, `bro-harness`, V8, or model-provider SDKs; an acceptance
script shaped like `scripts/acceptance-code-collector-deps.sh` checks the
resolved dependency tree and fails the build on any of them.

**Decided: a separate binary, not a connector mode on the code collector.** The
collector's dependency ceiling is its most valuable property, and a connector
satellite necessarily pulls in vendor SDKs, OAuth machinery, and a large
transitive HTTP surface. Fusing them either contaminates that acceptance test or
grows per-connector exceptions until it means nothing. Sharing the leaf crate and
the wire is the right amount of sharing; a host needing both runs two small
services.

The wire is shared but namespaced: connector publication mounts under
`/internal/file-source/v1/*` with the same conversation as the code lane's
`/internal/code-source/v1/*` (begin, paged manifest, complete, missing-hash
cursor, blob PUT, finalize, generation status). Route separation lets producer
grants, limits, and health be reasoned about per lane; store, blob cache,
generation model, and activation are the same code.

## 6. The connector interface

Producer-side, inside the satellite. No corpus type appears in it.

```rust
#[async_trait]
pub trait RemoteSourceConnector: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Validate config plus credentials; return remote identity facts for the
    /// onboarding probe and status surfaces. Fails closed with remediation text.
    async fn validate(&self, source: &ConnectorSource) -> Result<RemoteInfo>;

    /// `cursor = None` is a full walk; `Some` resumes incrementally. Returns
    /// entries plus the next cursor, or a typed cursor-invalidated signal.
    async fn list_changes(
        &self,
        source: &ConnectorSource,
        cursor: Option<ChangeCursor>,
    ) -> Result<ChangeBatch>;

    /// Bounded byte stream, aborting mid-transfer on the streamed byte cap.
    async fn fetch(
        &self,
        source: &ConnectorSource,
        entry: &RemoteEntry,
    ) -> Result<FetchedContent>;
}
```

A `RemoteEntry` carries the connector-stable remote id, the raw remote name path
within the scope, size when known, remote version (etag, revision, or
changestamp), kind (file, dir, native document), and media type when the store
knows it. `fetch` applies the export map for native documents and reports the
exported media type and extension.

Deliberate omissions: no write, delete, or permission method (section 13); no
watch or subscription API (polling via cursors; vendor push notifications are a
latency optimization that changes nothing about the manifest contract).

## 7. Provider-native documents: the export map

Google Docs, Sheets, and Slides (and SharePoint list-shaped content) have no
canonical bytes. Connectors declare an export map onto formats the chunker
registry already claims:

| Native kind | Export | Existing chunker family |
|---|---|---|
| Google Doc | `.docx` | office |
| Google Sheet | `.xlsx` | xlsx |
| Google Slides | `.pptx` | office |
| Drawings and other | `.pdf` | pdf |

The logical path carries the exported extension and `content_sha256` covers the
**exported** bytes, since those are what the corpus chunks. Re-export is gated on
the native document's `remote_version` in the producer journal, so an unchanged
document is never re-exported.

**Export is not chunking, and that is what keeps the dumb-producer invariant.**
Exactly one chunker version exists in the system, on the corpus host, so
satellite deploys can never skew against the index. The satellite asks the vendor
to render a document and receives ordinary bytes in a format the corpus already
claims: no format sniffing, no chunk model, no entity refs, no knowledge of the
registry beyond a static extension mapping. The acceptance test enforces this
mechanically.

Two honest wrinkles, both inherited:

- **Export size is unknown before fetching.** Metadata screening cannot bound an
  export, so export fetches run under a streamed hard cap that aborts oversized
  transfers mid-stream and records `skipped(oversize)`.
- **Exported bytes are not stable against vendor renderer changes.** A
  provider-side pipeline update can change `content_sha256` for a document nobody
  edited. Gating on `remote_version` keeps the satellite from discovering that
  churn on its own, but a full re-enumeration after cursor invalidation will
  re-export and may produce new hashes for unchanged content. That is a real new
  generation and the corpus pays for it. We accept the cost rather than trust a
  hash we did not compute; treating `remote_version` as corpus-side freshness
  authority would import vendor version semantics into the index, which is an
  open question (section 20), not a v1 assumption.

## 8. Identity: minting a durable scope for a non-git source

**RESOLVED by operator decision, 2026-08-12: option B.** Identity is a
grant-time, operator-minted, opaque durable `connector_source_id` written into
both sides of the two-sided config, with option A's provider coordinates
recorded as observations on the catalog record and publication status, never as
identity. Cross-producer convergence is accepted and closable later by an
operator-declared alias. The analysis below is retained because it is why, not
because the question is open.

The catalog keys a project by `ProjectScope::Published(PublishedScope { repo_id,
bbox_root_relpath }) | LegacyLocal`. A producer resolves `PublishedScope` from the
committed `.bbox/config.toml` at local `HEAD`, and the daemon revalidates grant
membership, `repo_id` equality with the presented scope, relpath shape, and
catalog uniqueness. **A Drive folder has none of this**: no commit, no committed
config, no repo id, nothing the daemon can independently recompute from the
producer's claim. Path-hash identity is not an available fallback; reviving it
recreates exactly the dead absolute-path bridge the post-mortem names.

**A. A provider-coordinate scope family.** Add
`ProjectScope::Connected(ConnectorScope { connector_kind, remote_authority,
remote_root_id })`, where `remote_authority` is the vendor tenant or account and
`remote_root_id` is the store's stable id for the scope root (a Drive folder file
id, a Graph drive plus item id, a bucket plus prefix). Identity then derives from
something the vendor guarantees stable, and two producers observing the same
folder converge, which is what `repo_id` buys for git. But "stable" is per-vendor
and softer than a commit hash: folder moves survive, tenant migrations, account
transfers, and shared-drive reparenting do not. WebDAV and S3 have no id concept,
so `remote_root_id` degenerates to a normalized URL plus prefix, which is a path
by another name.

**B. A grant-time synthetic scope.** The operator mints an opaque durable
`connector_source_id` once and writes it into **both** sides of the config: the
daemon's producer grant and the satellite's source config. Identity is the
operator's two-sided declaration, which is already the onboarding trust root, and
provider coordinates travel as *observations* on the catalog record and
publication status exactly as absolute paths travel as attachment observations
today. No vendor identity semantics enter the durable catalog, nothing breaks on
tenant migration, and the daemon's validation story stays honest because grant
membership plus catalog uniqueness is the whole of what it could check. Cost: two
operators independently onboarding the same folder mint two scopes and get two
projects with no mechanical convergence, and a config-transcription error is
undetectable daemon-side.

**C. Reuse `PublishedScope` by committing a config into the producer cache.**
Rejected: makes the cache mandatory (killing the cache-is-optional property),
re-imports path-shaped identity through the back door, requires making someone's
Drive into a git repository, and lets a cache wipe change project identity.

**Leaning: B, with A's coordinates recorded as observations.** It mirrors the
principle the locality program already settled (durable scope is identity,
observed paths are attachment facts) and keeps the daemon from pretending to
verify a vendor claim it cannot recompute. The convergence cost is what
`LegacyLocal` already pays and can be closed later by an operator-declared alias,
which is additive.

So the daemon revalidates: grant membership of the presented scope, catalog
uniqueness, connector kind and remote authority against the grant's recorded
expectation, and the whole manifest contract (shape, path validity, ordering,
digests, hashes, sizes, caps). It cannot verify that the presented coordinates
describe the folder the operator meant. That residual producer trust is the same
class the code-source lane already accepts for the canonical path string, named
here rather than papered over.

**Landed shape (phase 0).** `ProjectScope::Connector(ConnectorScope {
connector_source_id, connector_kind })`. Both halves are operator declarations;
catalog uniqueness and every grant lookup key on `connector_source_id` ALONE, so
a mistyped kind produces a named mismatch instead of a second project under an
owned id. `connector_kind` is a validated opaque token rather than a closed enum,
per the campaign's first invariant.

`connector_source_id` shape rule: opaque, 8 to 128 bytes, lowercase ASCII
alphanumerics plus `_`, `-`, `.`, first and last byte alphanumeric, no `..`.
That refuses path, authority, and case-confusion shapes without mandating a mint
algorithm the operator owns. Recommended form is a short prefix plus a UUID.

Coordinates live in `CatalogSnapshotV2.connector_observations`, keyed by project
id, valid only for a connector-scoped project, refreshed on every onboarding
report and compared for nothing.

**Downgrade story.** The catalog wire version is DERIVED from content, not
chosen: v2 while no connector scope exists, v3 the moment one does. So a
connector-free catalog written by a connector-aware daemon is byte-identical to
a pre-connector write and opens under the old reader unchanged. A catalog that
DOES hold connector scopes fails an older daemon closed. That is deliberate:
opening it minus the projects the old reader cannot represent would orphan
their content and free a durable scope for reuse, so refusing is the only
honest outcome and the remedy is to roll forward.

The refusal has two shapes, and an operator mid-rollback should recognize
either:

- the **startup version probe** (`probe_project_store_mode`, run before any
  project-scoped subsystem starts) reads only the `version` field and refuses
  an unknown one with `error.project_catalog_unsupported_version`. This is what
  a real rolled-back daemon hits; it never parses a project row. The version
  number is load-bearing rather than forensic: it is the entire mechanism by
  which an older build knows to stop;
- a **strict row decode** reached some other way refuses later and differently,
  with serde's `unknown variant 'connector', expected 'published' or
  'legacy_local'`, because the scope enum denies unknown variants.

Both are pinned by test so neither can quietly become a partial success.
Tracked as `gap-0c7ec76c`.

## 9. Onboarding and lifecycle

Two-sided operator config, mirroring `remote-project-onboarding.md`. No agent
tool creates a source and no MCP call triggers a fetch.

1. The operator adds the scope to the daemon's connector producer grant
   (`[[source_connectors.producers]]`: `producer_id`, `token_file`, allowlisted
   scopes, each carrying its expected connector kind and remote authority).
2. The operator adds the source to the satellite config on the producer host
   (scope, connector kind, remote root, policy, secret references).
3. On its next cycle the satellite calls `validate`, probes the remote for
   identity facts, and presents an authenticated request to
   `POST /internal/file-source/v1/catalog/onboard`. The daemon validates it
   against the grant and runs the catalog find-or-create composite. Registration
   is a derived consequence of the operator's two-sided intent.

Request: producer bearer token, the durable scope, and probed remote facts
(connector kind, remote authority, remote root id, display name, quota or
capability flags, declared aliases). Response: the receipt (project id,
created-or-already-present, catalog epoch) plus nomination outcome. The satellite
drives it before each publication cycle and onboards any scope the daemon reports
unknown; retries are safe because the composite is find-or-create.

**Pending-onboarding admission.** The code-collection precedent applies
unchanged: a configured scope with no catalog project is admitted at startup as
pending-onboarding, excluded from every publication lane, acceptable only to the
onboard endpoint. Otherwise "add the scope to the daemon config first" becomes
impossible.

**Non-goals inherited in spirit:** no agent self-service registration; no
MCP-triggered fetch, sync, or export; no relaxation of producer scope grants
(onboarding cannot widen them); no automatic locality-marker coverage for a newly
onboarded scope.

**Rejected alternatives, named:** *MCP register-and-sync tools* grant any MCP
caller durable source creation plus a third-party fetch trigger and require the
daemon to hold vendor credentials, which is the rejected agent-triggered write
path with an added egress. *Executor-mediated acquisition* (the daemon asks a
worker host to fetch in response to a tool call) creates a daemon-trusts-worker
surface and leaves credential selection with the daemon anyway. *Daemon-side
connector with brokered credentials* (short-TTL vendor credentials from the
secrets plane) beats static credentials in the cage and is still wrong: it gives
the corpus host third-party egress and makes vendor archaeology part of the
immutable image.

**Read-only status surfaces.** Freshness, last manifest receipt, last activation,
current generation, file count, logical bytes, skipped counters, cursor
degradations, and health ride existing shapes: a
`bbox_project_publisher_status`-shaped read, the catalog list, and `bbox_doctor`.
Removal is config removal plus an explicit retirement ceremony, never a tool call
that deletes bytes.

**Marker interaction and lane advancement.** A connector source has no local
fallback to close, since there is no daemon-reachable filesystem for that scope,
ever. The lane needs no new locality-marker family, but per the marker rule it
must still choose advancement semantics up front. **Decided: stable producer and
scope authority plus a healthy current generation (the tolerant form), not a
pinned generation.** Pinning suits a one-time cutover proof; a connector source
refreshes on the remote store's schedule, so a pinned marker would wedge on every
ordinary publication. This follows the code-source marker's own correction away
from a permanently pinned evidence generation.

## 10. Freshness, cursors, and generations

Freshness is **manifest content hashes plus generation identity**: not mtime, not
size, not the vendor's version string. `classify_project_file` governs the local
adapter only; connector sources inherit the collected-source contract.

```text
schema_version
connector_policy_version
scope
connector_kind
remote_watermark        // opaque, connector-shaped; display/diagnostic only
cursor_epoch            // increments on every full re-enumeration
manifest_sha256
file_count
logical_bytes
```

`remote_watermark` occupies the slot `head_commit` occupies for code sources but
is explicitly **not** authority. There is no `dirty_fingerprint` analog because a
remote store is always dirty: the manifest digest over the ordered
`(logical_path, content_sha256, size)` entries **is** the fingerprint. The server
computes and checks that digest, derives the generation id itself (producer id,
scope, policy version, cursor epoch, manifest digest), and never accepts a
caller-supplied generation id as authority.

Deletion falls out of the manifest diff. Renames reconcile in the producer journal
as `(same remote_id, new logical_path)`; on the wire a rename is a moved path
whose blob the server already caches, so it costs a manifest entry and no upload.

**Cursor invalidation degrades to full re-enumeration, reported.** Drive and Graph
both expire change tokens. On a typed cursor-invalidated signal the satellite
discards the cursor, increments `cursor_epoch`, and walks fully, recording the
degradation with its cause and cost (entries enumerated, blobs re-fetched,
documents re-exported) on publication status, health, and doctor output. Never
silently absorbed: an operator watching repeated epoch increments is watching a
real problem (revoked consent, tenant policy, a connector bug).

**The producer-side manifest journal** is the salvaged `ManifestEntry`, demoted
from durable daemon state to producer working state:

```text
remote_id        // connector-stable identity, the journal's key
remote_version   // etag / revision / changestamp
logical_path     // validated, scope-relative, wire-shaped
display_name     // faithful remote name, for producer-side status output
export_format    // Some for native-document exports
content_hash     // Some exactly when state = published
size             // Some exactly when state = published
state            // published | skipped(reason) | pending
```

State invariants rather than sentinels: `content_hash` and `size` are `Some`
exactly when `state = published`; `skipped` and `pending` carry `None` plus a
reason. The journal is what a bare cursor cannot be: it maps deletion tombstones
back to logical paths, reconciles renames instead of delete-plus-refetch, lets a
full walk find orphans, and prevents re-export of unchanged native documents.
Losing it costs one full re-enumeration and re-export pass, not data, because the
remote store is the durable backlog. The satellite has no spool, exactly as the
collector has none.

**Honest consequence:** `remote_id` and `remote_version` do not cross the wire in
v1, so the corpus cannot cite a remote document URL as evidence. That is the
remote-provenance open question, not a silent gap.

## 11. Policy and limits

Per source, evaluated on enumeration metadata before any fetch: include/exclude
globs on scope-relative logical paths; `max_file_bytes` defaulting to the shared
per-extension caps in `bbox-code-source`, so nothing is fetched that the corpus
would refuse to index; `native_export = true | false` (default true);
`max_total_bytes` per source, whose breach **aborts the publication loudly**
naming the largest offenders rather than truncating silently; and the streamed
export cap of section 7, the only cap that can fire mid-transfer.

Excluded entries are recorded as `skipped(reason)` and reported in bounded
per-reason counters. Silence is not an option: a policy quietly dropping half a
drive is indistinguishable from a broken connector.

Caps are enforced twice. The satellite enforces them to avoid wasting vendor
quota and bandwidth; the server enforces the same shared policy plus its
configured manifest-cardinality and logical-byte limits, because authentication
proves the configured producer, not the truth of its bytes. Policy version skew
between satellite and server is a typed rejection, not a best-effort merge.

## 12. Logical path safety, collisions, and the producer cache

Remote namespaces are not filesystems and now they are not even feeding one. The
satellite never trusts a remote name as a path.

**Wire manifest paths are validated, not sanitized-and-hoped.** The shared
contract requires non-empty relative slash paths, at most 4096 bytes total and
255 per component, with no empty, `.`, `..`, backslash, NUL, control, or
platform-prefix component; entries strictly increasing by raw UTF-8 path bytes;
no duplicates. Real remote names violate every one of these routinely, so the
satellite derives `logical_path` deterministically: sanitize per component, then
on collision append a short suffix derived from `remote_id`.

**Collision detection stays case-folded and NFC-normalized** even with no local
filesystem involved, for two independent reasons: manifest paths must be unique on
the wire, and downstream display, path-token search, and evidence rendering treat
the logical path as a path. Drive's duplicate-sibling model produces genuine
same-name collisions; case and normalization collapse produce more. The journal
keeps `display_name` faithful for producer-side status output. Honest limit,
carried forward: the index sees the encoded logical path, so path-token search
matches the encoded name wherever a collision suffix was applied. Projecting the
faithful remote name and URL into index metadata is coupled to the
remote-provenance question, not assumed here.

**The producer cache, when a connector wants one.** A connector may keep fetched
or exported bytes locally to avoid re-fetch across scans (large exports, slow
vendor APIs, quota pressure). It is optional per connector and per source, and
deleting it costs a re-fetch pass, never data. Cache paths derive from the content
hash rather than the remote name, which sidesteps local filesystem encoding
entirely; a connector insisting on name-shaped layout owns the full encoding and
containment burden. Containment is verified after encoding, connector-supplied
symlink entries are refused, and the cache holds only regular files and
directories. The satellite stamps the cache root with an ownership marker at
creation and refuses every destructive operation on a root missing it, so a
misconfigured path can never aim deletion at operator data: the salvage branch's
`.bbox-mount-owner` pattern, reused where it still applies.

## 13. Read-only invariant and export posture

**Read-only.** Connectors never mutate the remote: no writes, no deletes, no
permission changes. The trait has no mutating method, which stops callers from
requesting mutation but does not prove an adapter's internals never issue one.
Defense in depth: request read-only OAuth scopes wherever the vendor offers them
(Drive `drive.readonly`, Graph `Files.Read.All` and `Sites.Read.All`),
least-privilege credentials otherwise; assert HTTP-method conformance in adapter
tests so a non-idempotent method reaching the transport is a test failure rather
than a log line; record every remote call class in publication telemetry so an
adapter that starts issuing writes is visible in ordinary observability.

**Export posture.** Corpus-side nothing changes: bytes leave the corpus host only
through the already-designed embedding lanes (text to configured text routes,
pixels only for visual chunk kinds under the opt-in visual route policy).
Producer-side, this design adds exactly one trust edge, producer host to vendor
cloud, authenticated by an operator-consented credential. Record it in the
deployment runbook rather than leaving it implied.

## 14. Credentials

Two credential planes, on two hosts, with no overlap.

**Vendor credentials: producer-side, entirely.** Connector config carries secret
references, never values. OAuth client secrets resolve through the secrets
registry; rotating refresh tokens live behind an explicitly writable `TokenStore`
reference (one unambiguous writable ref, no write-path chains) so rotation has
exactly one durable home. That contract belongs to the campaign sibling
[secrets-provider.md](../operations/config-artifacts/secrets-provider.md); this
design consumes it without restating its mechanics. Dependency: the network
connectors need that design's registry, provider, and `TokenStore` adoption
phases; phases 0 and 1 here need none of them.

```toml
# producer host: connector satellite config
[sources.drive-ops]
connector = "gdrive"
scope = "<durable connector scope from the operator's two-sided config>"
remote_root = "<drive folder id>"
oauth_client_secret = "<secret ref>"
token_store_ref = "<writable token-store ref>"
token_store_writable = true

[sources.drive-ops.policy]
exclude = ["**/Archive/**"]
native_export = true
max_total_bytes = 21474836480
```

**Wire credentials: a ServiceToken producer grant.** Publication authenticates
with a file-sourced bearer loaded through `ServiceToken::load` (owner, mode,
symlink, hardlink, and shape checks), bound server-side to an immutable
`producer_id` plus the grant's allowlist of durable scopes. Tokens never appear in
environment variables, query strings, JSON bodies, MCP arguments, logs, metrics,
or response bodies. The satellite refuses non-loopback `http://` corpus URLs and
disables redirect following, so a credential cannot be forwarded to a different
authority. The `shell_env` non-secret lane invariant is untouched.

The daemon therefore holds zero vendor credentials, where the predecessor held
every connector's OAuth material and performed every fetch. That is the largest
security improvement in this rewrite, and a consequence of the locality axis
rather than an added control.

## 15. Connector catalog

Ordered by intended delivery; each is one adapter behind the trait, so the
catalog grows by demand without corpus changes. Per-vendor auth notes are as
observed 2026-07 and want reverification before implementation.

1. **`gdrive` (Google Drive API).** First, because the export-map payoff is
   largest. Changes API for cursors, `files.export` for native documents, file-id
   addressing (exact under Drive's duplicate-name model). Auth realities that
   shape the flow: Google's device-code grant supports only the narrow
   `drive.file` and `drive.appdata` scopes, not the broad read scopes whole-scope
   indexing needs, so the interactive leg is a loopback or installed-app flow (or
   a service account where domain delegation fits); and an external OAuth app
   must be pushed to Production status or its refresh tokens are revoked after 7
   days in Testing mode (Workspace-internal apps are exempt).
2. **`graph` (Microsoft Graph: OneDrive plus SharePoint).** One connector for
   both, because drives and site document libraries are the same driveItem and
   delta surface. Delta links for cursors; app-only client credentials for org
   tenants, delegated with `offline_access` for personal (refresh tokens carry a
   rolling 90-day window, so a regular publication cadence normally keeps them
   alive, subject to revocation and tenant policy). Device-code is fully
   supported here, unlike Drive's broad scopes.
3. **`webdav` and `s3`.** Etag-walk cursors (no delta APIs); mostly self-hosted
   stores. Low complexity, covers the long tail. Also where section 8's identity
   question bites hardest, since neither has a stable root id.

**`local_mirror` is retired as a connector and survives as a degenerate collector
case.** Many stores maintain a local mirror through their desktop clients (iCloud
Drive, Drive for desktop, OneDrive, Dropbox). On a signed-in producer host that
mirror *is* a local directory, so the right tool is the code collector's own
walker pointed at it: a collector configuration choice needing no vendor SDK,
OAuth, or cursor. Two facts from the predecessor's analysis survive, and belong in
the collector's walk policy rather than here. **Dataless placeholders**: evicted
files carry the APFS dataless flag and block on on-demand download when read, or
fail offline, so a walker must detect the flag and either skip-and-count or
trigger bounded explicit hydration, never fault files in implicitly mid-scan.
**iCloud has no public API**: rclone's `iclouddrive` backend is experimental
web-session SRP auth (real Apple ID password plus 2FA, app-specific passwords
rejected, a trust token expiring roughly monthly, Advanced Data Protection
gated), workable only as a monitored fallback with human re-auth alerts on hosts
without a signed-in Mac. On a signed-in Mac the mirror is the integration.

## 16. Build versus adopt for connector internals

The trait is ours either way; the question is what implements per-store plumbing
inside each adapter. **All ecosystem claims below were verified against
crates.io and vendor documentation as of 2026-07 and are flagged for
reverification before any adapter is written**; versions, backend coverage, and
CVE status all move.

- **Apache OpenDAL** (`opendal` 0.58 as-of-2026-07, Apache-2.0, tokio-native) is
  strong for object stores and WebDAV, and its layer stack (retry, timeout,
  throttle, metrics) is connector-shaped. Hard limits here: its iCloud backend
  was removed for lack of maintainers; it has no SharePoint/Graph backend at all
  (OneDrive support is personal-only); it models no change or delta feeds, so
  cursor machinery needs native APIs regardless; and its Drive path addressing is
  heuristic under duplicate names (most-recently-modified wins), which can
  silently shadow files in a path-addressed manifest. It does not run the
  interactive OAuth first leg but does auto-refresh given a refresh token plus
  client credentials; app registration, consent UX, and the token vault are ours
  either way. Verdict: adopt inside the `webdav` and `s3` adapters where
  etag-walk cursors are the plan anyway, not for the flagship drives.
- **Native SDK crates** carry the flagship adapters, because cursors and export
  live there: `graph-rs-sdk` (3.x as-of-2026-07; the only substantive Rust Graph
  client, covering OneDrive and SharePoint drives and sites, with built-in
  device-code, PKCE, and client-credential flows and auto-refresh; pin majors, it
  breaks), and for Drive either `google-drive3` or raw Drive REST over the
  `oauth2`/`yup-oauth2` crates, using file-id addressing plus the Changes API.
- **rclone as a sidecar** (MIT, 70-plus backends) is confined to a fallback lane
  for stores nothing else reaches. If ever run: drive it through the `rcd`
  localhost JSON API, pinned at or above 1.73.5 (CVE-2026-41179, a critical
  unauthenticated RCE in the RC API affecting 1.48.0 through 1.73.4; reverify the
  current advisory floor), loopback-only with auth, and never as a FUSE mount for
  indexing (macFUSE still needs a kernel extension on Apple Silicon, and mounting
  is the most fragile lane available).
- **`object_store`** (Arrow) is architecturally flat and S3-shaped with zero
  consumer-drive support and no roadmap for it; not applicable beyond the
  object-store legs OpenDAL already covers.

Decision rule: adapters own identity, auth, cursors, and export; adopt a library
per adapter only where it demonstrably covers those or cleanly slots under them.
No adapter's dependency choice leaks past the trait, and none may breach the
satellite's acceptance ceiling.

## 17. Non-goals

- **No daemon-side fetch, export, materialization, or mount root**, not as a
  fallback, not for small sources, not for tests. A fixture connector runs in the
  satellite like every other connector.
- **No agent-triggered acquisition.** No MCP tool creates a source, triggers a
  publication, forces a re-export, or removes bytes.
- **No write path to any remote store.** Read-only is an invariant, not a default.
- **No revival of git or repo mirroring as a connector.** The four landed
  transports own repositories.
- **No API-dataset or business-entity connectors here.** Observing a REST or
  GraphQL dataset and projecting typed facts into the graph is a different
  contract (schema declaration, fact validation, entity identity) owned by the
  reflective-graph program and tracked as `gap-0378c305`. This design is file and
  document trees.
- **No corpus-side vendor version semantics.** `remote_version` is producer
  journal state and a status fact, never index freshness authority.
- **No new chunkers.** A format the registry does not claim falls through,
  counted. Extending the registry is corpus-side chunker work, demand-driven.
- **No transparent source failover.** Producer loss or staleness preserves the
  last activated generation and reports degraded health.
- **No unbounded sources.** Manifest cardinality, logical bytes, per-file bytes,
  concurrent uploads, retained generations, and disk use are capped and enforced
  on both sides.
- **No agent-facing blob upload surface.** Publication is a dedicated
  authenticated internal route, not a general upload service.

## 18. Phases

**Phase 0: identity and catalog admission (blocking).** Settle section 8, land the
additive `ProjectScope` variant with its catalog version bump and strict
deserialization, extend the onboarding composite and validators, and extend
publisher-status and catalog surfaces to render the new scope family. Gate: a
connector-scoped project onboards, lists, and reports with no publication yet; a
tampered or non-granted scope is refused; a catalog downgrade path is proven.

**Phase 1: satellite substrate against a fixture connector (no network, no
OAuth).** Build `bbox-file-collector` with the shared wire client, generation
descriptor, producer journal, policy engine, logical-path derivation, and status
polling, plus a filesystem-backed fixture connector whose "remote" is a local
directory with synthetic ids, versions, and native-document entries. Mount
`/internal/file-source/v1/*` and wire collected activation to a connector-scoped
project. Gate: a fixture source onboards, publishes, activates, refreshes
incrementally, survives a daemon restart mid-publication, converges after a cursor
invalidation, and is searchable through hybrid search with multimodal content (a
PDF plus images) retrievable; the dependency acceptance test passes; no vendor
credential exists anywhere in the test.

**Phase 2: `gdrive`.** OAuth through the secrets layer, Changes cursors,
`files.export` with the export map, streamed export caps. Gate: a folder-scoped
source of a real Drive publishes with Docs and Sheets exported and chunked;
incremental publication fetches only changed entries, verified against vendor API
call counts; cursor resume survives satellite restart; token refresh persists no
plaintext outside the secrets layer; a duplicate-sibling pair and a
case-collision pair both land as distinct reachable documents.

**Phase 3: `graph`.** OneDrive personal plus a SharePoint site document library
behind one connector; delta-link cursors; app-only and delegated auth both
exercised. Gate: as phase 2, against both surface kinds.

**Phase 4: `webdav` and `s3`, plus catalog opening.** Shared etag-walk helper,
OpenDAL adoption evaluated inside these adapters, and a written "how to add a
connector" contract covering the trait, export map, cursor semantics, journal
duties, and acceptance ceiling. Gate: one self-hosted WebDAV and one
S3-compatible bucket publish and refresh; the contract doc suffices for an
implementer who has not read this design.

Phase 1 has no external-service dependency and no OAuth surface, which is what
makes it a safe substrate proof. Phases 2 onward are each one adapter plus its
auth flow.

## 19. Acceptance criteria

- A source onboards through two-sided operator config, publishes, and appears as
  an ordinary corpus project: hybrid search, graph inspection, and evidence
  bundling operate on collected content with no connector-specific branch
  anywhere on the corpus host.
- The daemon opens no socket to a vendor API and holds no vendor credential in
  any state of any source, provable by dependency ceiling plus config audit. No
  credential material appears in daemon config, the catalog, the wire manifest,
  generation metadata, publication status, logs, or metrics.
- Provider-native documents are searchable through the existing office, xlsx, and
  pdf chunkers with **no chunker changes**, and the satellite's dependency tree
  contains no chunker, Tantivy, or corpus-index edge.
- Incremental publication fetches and exports only changed entries, verified
  against vendor API call counts; an unchanged native document is never
  re-exported. An expired cursor degrades to full re-enumeration with cause and
  cost reported on publication status, health, and doctor, never silently.
- Policy exclusion prevents fetching, not merely indexing; per-source total-byte
  caps abort loudly naming the largest offenders; an oversized native export
  aborts mid-stream and is counted.
- Two remote entries colliding under case folding, Unicode normalization, or a
  vendor duplicate-sibling model publish as distinct manifest entries, both
  reachable, with faithful display names retained producer-side.
- Freshness is content-hash based end to end: mutating a document's bytes without
  changing its size republishes; changing metadata without changing bytes does
  not.
- A satellite crash or restart mid-publication resumes without re-uploading blobs
  the server holds or re-exporting documents whose `remote_version` the journal
  records; losing the journal entirely costs one full pass and no data. Deleting
  the producer cache, or disabling it, changes nothing about corpus content, and a
  destructive cache operation refuses a root missing the ownership marker.
- Removing a source leaves no orphaned catalog entry and no half-state: the last
  activated generation stays visible until an explicit retirement ceremony, and
  derived visual payloads persist until the visual store grows GC, documented
  rather than silent.
- Readers see exactly one generation: staged and retained generations are absent
  from lexical, code-symbol, hybrid-vector, and graph discovery, and a request
  begun before an activation retains its complete prior view.

## 20. Open questions

- ~~**Durable scope minting for non-git sources**~~ (section 8). RESOLVED by
  operator decision 2026-08-12 and implemented in phase 0: grant-time
  operator-minted `connector_source_id`, provider coordinates as observations,
  cross-producer convergence accepted and closable by an operator-declared
  alias. See section 8 for the landed shape and the downgrade story.
- **Remote provenance on the wire.** Whether `remote_id`, `remote_version`, and a
  renderable remote URL should reach the corpus as manifest-entry metadata so
  evidence can cite the source document, or whether the logical path is identity
  enough. Coupled to section 12's encoded-path display limit and to
  `gap-616857f8`. A manifest-entry field is a wire version bump: cheap once,
  expensive twice.
- **Export churn from vendor renderer changes** (section 7). Whether a full
  re-enumeration producing new hashes for unedited documents is accepted as a real
  generation (current position), suppressed by trusting `remote_version` as
  freshness authority (which rejects the content-hash principle), or detected and
  reported as a distinct condition so renderer churn is distinguishable from real
  edits.
- **Source-scoped embedding policy.** Whether a source can override visual-route
  participation ("index this drive but never send its pixels to a hosted
  embedder") or whether the global opt-in suffices. Leaning toward a per-source
  `visual_embed = false`: cheap, and it matches the export-posture principle.
- **Quota and backoff across sources.** Per-connector rate limiting is
  connector-internal in v1; two sources sharing one vendor account will need a
  shared per-account limiter on the producer. Precedent: the embed queue's
  conservative byte heuristics.
- **Visual-store garbage collection.** Remote deletion and source removal strand
  derived visual payloads until refcount or mark-sweep GC lands: still true, still
  deferred, now with a second consumer arguing for it.
- **Graph participation for connector-sourced documents.** Whether a document
  tree with no repository, commits, or symbols projects into the project graph the
  way code does or wants its own projection shape. Tracked with the
  reflective-graph program as `gap-5d57d2bb`.
- **Long-tail extraction.** Remote corpora surface formats the registry does not
  claim (email archives, legacy binary `.doc`/`.ppt`). Candidates are a
  compiled-native Tika binding or a supervised Tika server sidecar, adopted behind
  a new corpus-side chunker, never inside a connector. Demand-driven.
- **Eventual satellite consolidation.** Whether `bbox-code-collector` and
  `bbox-file-collector` merge once the producer-host story stabilizes.
  Deliberately not now (section 5).

## 21. Relationship

- **Supersedes** the predecessor `design/connectors/remote-source-connectors.md`
  as authored on `salvage/satellite-arc-20260718` and carried on
  `campaign/reflective-graph-r2-projection`. Its mount substrate, materialization
  root, `MountRecord` identity, daemon sync driver, and `bbox_mount_*` MCP surface
  are retired by name in section 2.1; its export map, manifest discipline, cursor
  abstraction, policy engine, path-safety analysis, read-only invariant,
  build-versus-adopt catalog, and visual-store caveat carry forward in section
  2.2. Its `crates/bbox-connectors` implementation (git and `local_mirror`
  adapters) is salvage donor code, not shipped behavior.
- **Extends** [distributed-code-source-collector-impl.md](../daemon-runtime/distributed-code-source-collector-impl.md):
  reuses its wire conversation, leaf-crate contracts, blob cache, generation and
  activation model, dependency acceptance pattern, and no-spool principle, adding
  a second producer kind whose backlog is a remote store instead of a checkout.
- **Extends** [remote-project-onboarding.md](../daemon-runtime/remote-project-onboarding.md):
  adopts its two-sided operator config, probe-and-present backchannel,
  pending-onboarding admission, and non-goals, applied to a scope family it did
  not anticipate.
- **Continues** [locality-first-decomposition.md](../daemon-runtime/locality-first-decomposition.md):
  that design killed the mount and connector substrate on the checkout axis and
  established push-not-pull. This one revives the remote-store capability on the
  producer axis under the same rule, which is why nothing here asks the daemon to
  reach anywhere.
- **Companion of** [connectors.md](connectors.md) (the hub ordering this
  campaign), [slack-ingestion-connector.md](slack-ingestion-connector.md) (a
  producer-side corpus-ingestion sibling with a different remote shape),
  [reflective-graph-connector-program.md](reflective-graph-connector-program.md)
  (which owns generic schema, fact validation, and API-dataset profiles, and
  treats this design as the file-tree profile), and
  [secrets-provider.md](../operations/config-artifacts/secrets-provider.md)
  (which owns the secret registry, providers, and the writable `TokenStore`
  contract this design's vendor credentials depend on).
- **Companion of** [checkout-identity-and-provisional-knowledge.md](../corpus/knowledge/checkout-identity-and-provisional-knowledge.md):
  its identity principle (durable scope is identity, observed paths are
  attachment facts) is what section 8 applies to a store with no commit to derive
  identity from.
- **Distinct from** the Slack agent bridge (`design/integrations/slack/`), an
  interactive dispatch surface rather than a corpus source. Corpus ingestion from
  Slack is the campaign sibling above.
