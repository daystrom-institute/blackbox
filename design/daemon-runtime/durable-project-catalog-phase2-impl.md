---
title: "Durable project catalog Phase 2 implementation plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, project-identity, catalog, resolver, administration, attachments, phase-2]
brief: "Implement the shared catalog/attachment resolver, the complete proof-split administration vocabulary, converted register/rename/unregister/init/eject semantics, and project-id fields plus bounded compatibility ledgers on path-keyed stores, proving the v2 runtime path only against isolated migrated state."
---

# Durable project catalog Phase 2 implementation plan

Date: 2026-07-24

Governing design:
[`durable-project-catalog-impl.md`](durable-project-catalog-impl.md), especially
sections 5, 7, 13, 15 (Phase 2), 16, and 17.

This is the executable plan for Phase 2 only. It refines the governing design;
it does not replace or narrow it. The independent reviewer must read this
document, the complete governing design, the Phase 1 implementation plan,
every governing companion listed there, `DECISION_LEDGER.md`, the current
code, and the fixed baseline `monolith-decomposition-pre-attempt-2`.

## 1. Required outcome

Phase 2 creates the runtime selection and administration layer of the catalog
cut without applying v2 bytes to configured operator state:

1. one shared catalog/attachment resolver with the two stopping points of
   governing section 5.3, and every project-selector surface routed through
   it;
2. a bootable v2 runtime path: strict store-version mode selection at daemon
   startup, a catalog-backed checkout-access authority, and a catalog-backed
   compatibility projection for not-yet-converted consumers, proven only
   against isolated migrated state;
3. the complete administration vocabulary split by proof per D-004: MCP
   surfaces for attachment-proved operations (attach, detach, promotion,
   attachment-proved scope migration, publisher binding, default-attachment
   selection) and offline `blackbox project-catalog` subcommands for proofless
   authority (catalog add/import, alias accept/reject, operator-attested
   scope migration, retire/delete), all riding the Phase 1 journaled pair
   transaction;
4. converted `bbox_project_register`, `bbox_project_rename`,
   `bbox_project_unregister`, `bbox_project_init`, and `bbox_project_eject`
   semantics: composite register with find-or-create and typed
   `scope_promotion_required` / `scope_migration_required` refusals, rename as
   attachment relocation, unregister as detach, and audited
   `LegacyLocal -> Published` promotion, active in catalog mode while bridge
   mode preserves version-1 behavior;
5. stable `project_id` fields, write-time stamping, and dual-read on the
   versioned path-keyed logical-store owner set, with the append-only
   host-local `LegacyPathBinding` ledger integrated into attachment
   relocation, without rewriting execution targets.

The exit gate is fixed by governing section 15 Phase 2: id, alias, and scope
corpus queries work with no attachment; path operations resolve exactly one
valid attachment; ambiguity and unknown paths fail closed. All of that is
proved only against isolated migrated v2 state. The configured daemon remains
the version-1 bridge for the whole phase (D-002), and live bridge behavior
stays at observable parity with the Phase 1 head except for the enumerated
defect corrections in section 4.3.

## 2. Survey of the current tree

### 2.1 Landed Phase 0 and Phase 1 substrate

The tree at `fcb2dc0c` contains everything Phase 2 builds on:

- the complete typed model in `bbox-corpus-core/src/project_catalog.rs`:
  `ProjectId` (`p_` + 32 lowercase hex, catalog-collision-checked mint, legacy
  ids accepted by parse), `AttachmentId`, `ScopeMigrationId`,
  `LegacyPathBindingId`, `RepoHistoryId`, `CommitNamespace`,
  `CatalogSnapshotV2` (projects, repo histories, ambiguous namespaces, scope
  migrations, origin), `CorpusProject`, `ProjectScope::{Published,
  LegacyLocal}`, `ScopeMigrationRecord` with
  `ScopeMigrationKind::{Promotion, RelpathMove, RepoAuthorityChange}` and
  `ScopeMigrationAuthorityProvenance::{AttachmentProved, OperatorAttested}`,
  `AttachmentSnapshotV1` (attachments, scope-migration proofs, legacy path
  bindings), `CheckoutAttachment` with kind/status/capabilities, and the
  strict duplicate-key-rejecting codecs;
- the cross-validation join `validate_catalog_attachments` returning
  borrow-scoped `ValidatedCatalogAttachments` / `ValidatedCheckoutAttachment`,
  the only sanctioned pairing of the two stores;
- the journaled pair-transaction owner
  `bbox-indexing/src/project_catalog_store.rs`:
  `ProjectCatalogStore::{open_existing, initialize_empty, snapshot,
  transact}`. `transact(expected_epoch, closure)` is the entire regular
  mutation API: the closure edits private clones, cannot change version,
  epoch, or origin, and commits under epoch compare-and-swap with journal
  recovery and poisoning on unrecoverable state;
- the migration engine and facade
  (`ProjectCatalogMigrationFacadeV1::{preflight, apply_rehearsal, verify}`),
  hash-bound report/resolution artifacts, receipts, and the offline
  `blackbox` CLI (`src/bin/blackbox.rs`) with the versioned envelope of
  D-020. Apply installs only into an isolated rehearsal root in this phase
  window (D-002);
- the compatibility view `ProjectRecord::from_catalog_attachment`
  (`bbox-corpus-core/src/project_record.rs`), which refuses detached,
  cross-project, or scope-mismatched joins and never fabricates a path for a
  remote project, plus the non-serializable
  `ProjectCatalogCompatibilityProjectionV1` produced by facade verification.
  P1-D section 7.2 fixes this as the contract Phase 2 must use when it
  replaces `ProjectRegistry` in the isolated v2 runtime path;
- the checkout-access broker (`bbox-indexing/src/checkout_access.rs`) with the
  `CheckoutAccessAuthority` trait, the closed nine-kind access vocabulary,
  `ValidatedCheckoutLease`, `CheckoutAttachmentSelector` including the
  Phase-0 compatibility `LegacyPath` lane, `CheckoutAccessSourceLane` with
  `is_compatibility()` retirement telemetry, and the sixteen-code
  `CheckoutAccessErrorCode` vocabulary. `V1CheckoutAccessAuthority` and
  `DenyCheckoutAccess` are the existing implementations;
- the process-lifetime migration lock
  (`bbox-indexing/src/project_catalog_migration_lock.rs`): the bridge
  `ProjectRegistry::open` holds it shared for its lifetime, which is what
  makes an exclusive offline apply fail while any daemon runs.

### 2.2 Live selector reality

Nothing in `src/` reads `CatalogSnapshotV2`. Runtime authority is
`ProjectRegistry` over `projects.json` (`LegacyProjectStoreV1`), with
`pub type ProjectStore = LegacyProjectStoreV1` in
`bbox-indexing/src/projects.rs` and the registry plus `StorePersister` held in
`src/server/state.rs`.

Selector resolution today is fragmented across roughly twenty canonical
helpers and eight bespoke bypasses. The canonical spine:

- `resolve_project_context(raw, records, ResolveIntent)` in
  `bbox-indexing/src/projects.rs`: exact `project_id` or exact
  `canonical_path`, then unique registered alias, then absolute-path
  resolution with a deliberate asymmetry between the broad Read gate
  (`resolve_base_project_for_scope`: descendants and any worktree) and the
  conservative Write gate (`resolve_managed_fleet_worktree`);
- `ProjectRegistry::resolve` (legacy exact arm without worktree aliasing),
  `fleet_worktree_scope_and_dir`, `unique_alias_index` (fail-closed on
  duplicate claims), `sync_declared_aliases` (startup alias rewrite from
  committed config);
- the daemon-side wrappers in `src/tools/scope.rs`:
  `resolve_project_write_scope` / `resolve_project_write` (a
  `RepositoryMutation` lease probe whose `AttachmentNotFound` arm returns the
  raw selector verbatim as the durable scope key: the load-bearing
  unregistered-write pass-through) and `rescope_project_filter_value` (maps
  checkout aliases to base canonical paths for store filters, otherwise
  preserves substring semantics);
- the version-1 access bridge `checkout_access_v1.rs`
  (`resolve_legacy_path`, `unique_project`, `select_scope_project`,
  `resolution_roots`) and the daemon adapters in
  `src/server/checkout_access.rs` (`project_id_for_published_scope`,
  `with_selected_project_access`, and friends).

The bespoke bypasses Phase 2 must converge (labels reused throughout this
plan):

- B1 `base_project_filter_id` in `bbox-corpus-index/src/index/search.rs`:
  reimplements id/path/alias matching against `load_project_records` read
  straight off disk, with no path arm, used by `bbox_search`/`bbox_cite`
  filter clauses, `bbox_sessions_list`, and `work_tool_calls`;
- B2 `resolve_project_filter_path` in
  `bbox-mcp-tools/src/mcp_tools/hybrid_search.rs`: bare eight-hex
  pass-through, then `resolve_project_context(Read)`, then a
  `project_id_for_path` hash fallback that mints a valid-looking eight-hex
  for unregistered paths;
- B3 `unique_project` in `src/tools/graph.rs`: exact project-id equality
  only, with `error.project_not_registered` / `error.project_ambiguous`;
- B4 the hand-rolled matcher in `bro_slack_bind` (`src/tools/config.rs`);
- B5 `resolve_scope_path` in `src/orchestration/mcp.rs`: raw path, registry
  never consulted;
- B6 the storage tools (`storage_health`, `storage_gc`,
  `storage_migration`): `ProjectRegistry::resolve` then raw pass-through on
  miss;
- B7 packet project matching by exact string equality
  (`bbox-packets/src/lib.rs`);
- B8 `bbox_mcp_surface` passing `p.project` raw while the `/mcp?project=`
  wire head resolves through `resolve_project_context(Read)` and then falls
  back to the literal for parity.

Failure behavior is correspondingly inconsistent: unknown selectors silently
degrade to substring filters on read paths, become durable raw scope keys on
write paths, mint hash-derived eight-hex filters in hybrid search, pass raw
into sidecar file paths in `bbox_edge_compact`, and hard-fail only in the
admin lifecycle tools and the knowledge/gap lanes where the checkout-identity
arc already cut the path fallback.

Store keying: coordination and knowledge stores key project scope by absolute
canonical path strings. `project_ref_counts` and `migrate_project_refs` in
`src/server/routes.rs` enumerate eleven stores (knowledge, threads, notes,
pins, packets, slack channel bindings, slack proposal links, teams,
whiteboards, pollers, crons). The Phase 1 inventory vocabulary
`LegacyPathStoreKindV1` names fourteen logical owners (knowledge, gap,
thread, note, pin, roadmap, packet, task, proposal, slack binding,
whiteboard, artifact, provenance, transcript edge). The deltas are live
defects or vocabulary gaps Phase 2 must reconcile explicitly:

- gaps and roadmap rows are silently orphaned by `bbox_project_rename` and
  invisible to the `bbox_project_unregister` force gate;
- webhooks carry `default_project_dir` with no rename coverage at all;
- teams, pollers, and crons are rewritten on rename but are execution-target
  stores outside the ledger vocabulary, exactly as governing section 7.3
  prescribes (they must keep their execution paths and are not logical
  identity owners);
- edge sidecar, vectors, and artifact on-disk layout are already id-keyed;
  Tantivy carries both a literal `project` path field and `project_id` /
  `base_project_id` term fields.

### 2.3 Missing Phase 2 authority

- No shared resolver exists; `CatalogProjectContext` /
  `AttachedProjectContext` from governing section 5.3 are not yet defined in
  code.
- No mutation vocabulary exists above `transact`: no attach, detach,
  promotion, scope migration, alias acceptance, publisher binding, catalog
  add, retire, or catalog list/get, on either the MCP or CLI surface.
- The daemon has no store-version mode selection: startup unconditionally
  opens the version-1 registry.
- No `V2CatalogCheckoutAccessAuthority` exists.
- Coordination stores carry no `project_id` fields (knowledge, gaps, threads,
  notes, pins, roadmap, packets, whiteboards, slack proposal links); nothing
  stamps new rows; nothing dual-reads.
- The `LegacyPathBinding` ledger is written only by migration; attachment
  relocation does not append to it because attachment relocation does not
  exist yet.

### 2.4 Fixed baseline and comparison state

The fixed comparison baseline remains the annotated tag
`monolith-decomposition-pre-attempt-2` = `254cabf0`. Phase 1 completed at
`fcb2dc0c`. The implementation review scope for this phase remains
`monolith-decomposition-pre-attempt-2..HEAD`.

## 3. Non-goals and phase boundary

Phase 2 does not:

- apply v2 bytes to configured operator state, bind a v2 daemon to configured
  state, or weaken the isolated-rehearsal-only apply guard (D-002);
- build the path-free index, source-neutral project input, relative-path
  schema, or history/quarantine generation materialization (Phase 3);
- convert collector grants, activation writers, auth-swap separation, or
  cutback state transitions to catalog scope (Phase 4);
- wire accepted-publication generations, publisher advance with new
  generation creation, catalog-keyed knowledge/gap views, or per-checkout
  overlay-baseline degradation into live views (Phase 5). Phase 2 implements
  publisher binding administration only, as bounded in section 7.7;
- delete the version-1 compatibility lanes, the eight-hex special cases, the
  `LegacyPath` attachment selector, direct `load_project_records` consumers,
  or the path fallback (Phase 6 and the observation-gated cut of governing
  section 7.3);
- implement the destructive-retire discharge workflow for projects with live
  references, `bbox_repo_history_namespace_resolve`, or ambiguous-namespace
  attribution;
- add authenticated operator identity to the MCP transport (D-004);
- convert dispatch/orchestration execution targets (`cwd`, `project_dir` on
  bro, team, workflow, poller, cron, webhook, badgey, atom surfaces) into
  catalog selectors; they remain execution-path data per governing section
  7.3;
- change vector storage (route-keyed, project identity rides entity refs);
- reopen multi-machine dispatch routing or further machine-daemon splits.

Bridge-mode live behavior changes are limited to the enumerated list in
section 4.3. Everything else observable stays at parity.

## 4. Runtime authority model for the bridge window

### 4.1 Store-version mode selection

`src/server/open.rs` gains one explicit fork before any project-scoped
subsystem starts:

1. Read the configured projects path with a bounded strict probe:
   - absent file with no sibling catalog-family artifact: bridge mode; the
     version-1 registry creates its store as today. The sibling probe is
     defined negatively so the enumeration cannot rot: with the catalog
     absent, the presence of any code-owned catalog-family artifact beside
     the projects path other than the two lock files (`projects.json.lock`
     and the lifetime migration lock) is a half-pair state. Today that set
     is the attachment snapshot, transaction journal, committed migration
     marker, migration receipt, migration assets, and transaction
     stage/backup artifacts; the probe derives it from the store owner's
     code-owned path roles rather than a probe-local list. Half-pair
     startup refuses closed with a typed diagnostic instead of letting the
     bridge mint a fresh v1 store beside v2 authority state (the same
     never-choose-one-file-of-a-pair rule the strict open enforces);
   - a strict decode as `LegacyProjectStoreV1` (version 1): bridge mode,
     identical to today's startup;
   - a strict decode as `CatalogSnapshotV2` (version 2): catalog mode;
   - anything else (unsupported version, corrupt JSON, oversize): fail daemon
     startup closed with a typed diagnostic, before server routes bind, per
     governing section 6.1.
2. Bridge mode constructs today's runtime exactly: `ProjectRegistry` holding
   the shared lifetime migration lock, `StorePersister`,
   `V1CheckoutAccessAuthority`.
3. Catalog mode:
   - acquires the shared process-lifetime migration lock, then
     `ProjectCatalogStore::open_existing`, which enforces strict pair
     validation, journal recovery, and the `CatalogOriginV2` /
     migration-marker binding;
   - constructs the v2 project authority described in section 4.2 and the
     `V2CatalogCheckoutAccessAuthority` of section 6.3;
   - never runs `sync_declared_aliases`: committed aliases are nominations in
     catalog mode (D-005), reported, never auto-accepted;
   - never constructs the version-1 registry or its persister.

Mode is fixed for the process lifetime. There is no dynamic downgrade or
upgrade; a store that changes underneath the daemon is exactly the corruption
class the strict open already fails closed on at next start.

Catalog mode never auto-initializes state. `initialize_empty` remains an
explicit offline/test entry point; a daemon started against an empty
directory is bridge mode by the absent-file rule. During Phase 2 the only
catalog-mode roots are isolated rehearsal roots produced by the Phase 1 CLI
or test fixtures; configured operator state cannot become v2 because apply
still refuses non-isolated destinations. The mode fork itself is therefore
live-inert: on configured state it always selects the bridge arm, and the
bridge arm is byte-identical in behavior to today's startup.

`SharedState` carries the authority as one closed enum (naming final):

```text
ProjectAuthority =
    Bridge {
        registry: Arc<RwLock<ProjectRegistry>>,
        persister: StorePersister<ProjectRegistry>,
    }
  | Catalog {
        store: Arc<ProjectCatalogStore>,
    }
```

Consumers never match on this enum directly outside the seams defined in this
plan (resolver backend construction, checkout-access authority construction,
admin dispatch, compatibility projection). Everything else consumes the
resolver, the broker, or the projection.

### 4.2 One resolver, two backends

The shared resolver is one engine with a closed backend enum, not two
parallel resolvers. The engine owns the governing section 7.1 order; the
backend supplies membership, alias, scope, and attachment data:

- the v1 backend reads a pinned snapshot of `ProjectRecord` rows plus the
  checkout registry and reproduces today's observable semantics exactly,
  including the Read/Write gate asymmetry and every compatibility lane, each
  tagged for telemetry;
- the v2 backend reads a pinned `ProjectCatalogState` and implements the
  strict semantics: exact catalog membership, unique accepted alias, explicit
  scope parameter, exact-or-deepest attachment containment, fail-closed
  ambiguity, and no identity manufacture for unknown paths.

Typed-value integrity: the v1 backend never synthesizes a `CorpusProject`,
`PublishedScope`, or `CheckoutAttachment` from version-1 hints. Resolver
outputs wrap a closed identity view (section 5.3) whose v1 arm carries the
`ProjectRecord`-derived data under its own type, so serde or string-shape
forging of v2 authority from v1 data is impossible by construction.

### 4.3 Live-behavior parity contract

Routing live surfaces through the engine (milestone P2-E) must be
observation-equivalent on bridge mode. The plan treats parity as a tested
property, not an intention: a selector-corpus fixture (section 9.4) runs
every converted surface class against the legacy helpers and the engine-v1
backend and requires identical outcomes.

The complete list of deliberate bridge-mode behavior changes; everything
else is parity:

1. gap and roadmap rows gain rename migration coverage in
   `migrate_project_refs`, and gap/roadmap/webhook/artifact-metadata
   references join `project_ref_counts`, closing the silent-orphan and
   force-gate blind spots of section 2.2;
2. webhooks gain rename coverage for `default_project_dir` (execution-target
   rewrite, same class as pollers/crons);
3. new rows in the section 8 owner stores are additionally stamped with
   `project_id` when resolution yields one (purely additive field);
4. resolver-lane telemetry counters appear in doctor/health (additive
   observability, same pattern as the Phase 0 checkout-access counters);
5. typed error payloads may carry an additional stable `code` where today's
   failure is an anyhow string, with the human-visible message preserved;
6. per D-031, surfaces whose bespoke resolvers matched only exact
   id/path/alias (B1 corpus-search filters, the B4 slack-bind matcher, the
   B6 storage-tool resolvers) now resolve worktree and descendant paths to
   the base project through the engine's broad Read gate, exactly like every
   canonical-spine surface. The former exact-only misses were the documented
   invisible-worktree defect class (gap-72fd5932's silent-empty-results
   trap), not a preserved semantic; the literal lanes themselves are
   unchanged.

Explicitly not changed on bridge mode: the unregistered-write pass-through of
`resolve_project_write`, the substring filter lanes, the hybrid-search hash
fallback, storage-tool raw pass-through, `bbox_edge_compact` raw ids, the
`/mcp?project=` literal fallback, and the eight-hex special cases. Those are
version-1 compatibility semantics; they are tagged and counted in Phase 2 and
deleted in Phase 6.

## 5. Milestone P2-A: shared resolver and selector contract

### 5.1 Ownership and files

- `crates/bbox-corpus-core/src/project_selector.rs` (new): pure selector
  request/outcome/error types and the classification helpers that need no
  store access. Pure types live in corpus-core so lower crates can consume
  resolver outputs without depending on `bbox-indexing`, mirroring the
  catalog model split of governing section 6.1.
- `crates/bbox-indexing/src/project_resolver.rs` (new): the engine, the two
  backends, and the equivalence test harness.
- `src/tools/scope.rs`: the daemon wrappers are reimplemented over the engine
  in P2-E; P2-A only lands the engine and its tests.

### 5.2 Selector model and resolution order

```text
ProjectSelectorRequest {
    selector: Option<String>,          // raw caller string, if any
    scope: Option<PublishedScope>,     // explicit scope-accepting APIs only
    session_checkout: Option<SessionCheckoutRef>,
    intent: ResolveIntent,             // existing Read | Write
    class: SelectorClass,
}

SelectorClass = Selection | Filter
```

`SelectorClass` is the taxonomy the caller table of section 9 assigns to
every surface, fixed at the call site, never inferred:

- `Selection`: the caller needs one project (writes, admin, graph project
  ops, leases). Unknown or ambiguous selectors fail closed on the v2 backend
  and follow the tagged legacy lanes on the v1 backend.
- `Filter`: the caller narrows a query (transcript/hybrid search, store list
  filters). A selector that resolves narrows by identity; one that does not
  resolve keeps the surface's documented literal semantics (the permanent
  unregistered-cwd substring lane of governing decision 9) and never
  manufactures identity.

Resolution order implemented by the engine, per governing section 7.1:

1. exact catalog `ProjectId` membership (parse, then membership; never
   string-shape);
2. exact unique accepted alias;
3. exact `PublishedScope` when the request carries the explicit typed scope;
4. exact or deepest-contained active attachment path (v1: registered root or
   descendant per the intent gates);
5. linked-worktree / managed-clone mapping through an attachment (v1: the
   existing worktree gates);
6. session cwd only as a legacy fallback through the session attachment.

Equal-depth attachment, duplicate alias, duplicate scope, and duplicate id
outcomes fail closed with distinct codes. Unknown absolute paths never
manufacture identity on the v2 backend.

### 5.3 Resolution outputs

```text
ProjectResolution =
    Catalog(CatalogProjectContext)
  | Attached(AttachedProjectContext)
  | LiteralFilter { raw: String, lane: CompatibilityLane }

CatalogProjectContext { project: ResolvedProjectIdentity }

AttachedProjectContext {
    project: ResolvedProjectIdentity,
    attachment: ResolvedAttachment,
}

ResolvedProjectIdentity =
    V1Compat { record: ProjectRecord }          // bridge backend only
  | Catalog  { project: Arc<CorpusProject> }    // catalog backend only

ResolvedAttachment =
    V1Compat { checkout: CheckoutContext-shaped roots }
  | Catalog  { attachment_id: AttachmentId, roots, capabilities }
```

Accessor methods (`project_id()`, `display()`, `filter_terms()`,
`canonical_store_key()`) give callers a backend-independent view.
`LiteralFilter` is only reachable for `SelectorClass::Filter` requests; a
`Selection` request never receives it. Corpus-only callers stop at
`CatalogProjectContext`; path callers take `AttachedProjectContext` and then
acquire a lease through the existing broker; the resolver itself returns no
filesystem authority and no lease, preserving the section 9 lease boundary
of the governing design.

`canonical_store_key()` defines the durable scope key written into
path-keyed stores, and it preserves v1's key-to-base invariant (durable
writes key to the base project so every checkout sees them): on the v1
backend it is the record's `canonical_path` exactly as today, regardless of
which checkout resolved; on the v2 backend it is the `checkout_project_dir`
of the project's active `Base`-kind attachment whenever one exists, even
when resolution arrived through a `Worktree` or `ManagedClone` attachment.
Only when the project has no active base attachment does the key fall back
to the resolving attachment's `checkout_project_dir`; rows written in that
state still carry `project_id`, so dual-read matches them by id. The store
key is identity data and is distinct from the write target: repo-owned and
overlay writes keep targeting the writing checkout's own directory exactly
as the checkout-identity arc defined (the threads durable-key versus
record-dir split, generalized). This keeps store keys byte-compatible
across the bridge window while section 8 adds the id field beside them.

### 5.4 Failure vocabulary

`ProjectResolveError { code: &'static str, detail: String }` with bounded
detail (reusing the catalog error bounding conventions) and this closed code
set:

| code | meaning |
|---|---|
| `error.project_selector_unknown` | no arm matched a Selection request |
| `error.project_selector_ambiguous` | duplicate id/alias/scope or equal-depth attachment |
| `error.project_alias_conflict` | alias claimed by multiple projects |
| `error.project_scope_unknown` | explicit scope owned by no project |
| `error.project_attachment_required` | resolution stopped at catalog for a path operation |
| `error.project_attachment_ambiguous` | multiple active attachments, no session/explicit/default selection |
| `error.project_capability_denied` | selected attachment lacks the required capability |
| `error.project_catalog_inactive` | catalog-only operation invoked on the bridge |

Lease-time failures keep the existing `CheckoutAccessErrorCode` vocabulary;
the resolver does not duplicate it. Error details never echo more than the
bounded selector and never include a second host path.

### 5.5 V1 backend equivalence

The v1 backend is extracted from, not written beside, the current logic:
`resolve_project_context`, `resolve_base_project_for_scope`,
`fleet_worktree_scope_and_dir`, and `unique_alias_index` become the internals
of the v1 backend (or are called by it verbatim), so there is one
implementation of the version-1 semantics after P2-E, not two. Equivalence
tests fix the contract before any caller moves:

- a selector corpus covering: exact id, exact path, unique alias, duplicate
  alias, registered root, descendant path, linked worktree, managed fleet
  worktree, nested monorepo root, unknown absolute path, unknown relative
  string, empty, and oversized selectors;
- for each corpus entry and each intent, the engine-v1 outcome must equal the
  legacy helper outcome (same matched project, same checkout dir, same None).

### 5.6 V2 backend semantics

The v2 backend implements the strict arms over a pinned
`ProjectCatalogState`:

- id arm: `ProjectId::parse` then exact `catalog.projects` membership;
- alias arm: exact match against accepted `operator_aliases` only, unique
  across the catalog (nominated aliases never resolve);
- scope arm: exact `PublishedScope` equality against `Published` projects;
- path arms: exact `checkout_project_dir` match, then deepest containing
  active attachment; any equal-depth tie between distinct active attachments
  fails closed (the cross-project `(checkout_id, project_root_relpath)`
  exclusivity invariant makes the common shapes unrepresentable, and the
  resolver does not assume the residue is empty);
- Write intent additionally requires the conservative write gate revalidation
  at lease time, unchanged from the broker's current contract;
- session-cwd fallback resolves only through an attachment whose checkout id
  matches the session's authoritative checkout.

Multiple active attachments for one project leave corpus reads unambiguous
(arm 1-3 outcomes are `CatalogProjectContext`) and make path selection
require a session-pinned attachment, an explicit `attachment_id`, or exactly
one operator-selected default with the needed capability, per governing
section 7.1. The default-attachment selection surface is section 7.3 of this
plan.

### 5.7 P2-A tests and gate

- unit tests: order, ambiguity, class taxonomy, error codes, bounded details;
- v1 equivalence corpus (section 5.5);
- v2 semantics tests over hand-built catalog/attachment fixtures, including
  monorepo siblings, worktree attachments, detached attachments (never
  resolve), and no-attachment remote projects (resolve to catalog contexts);
- property test: no request with `class = Selection` ever yields
  `LiteralFilter`; no v2 resolution ever yields a `V1Compat` identity.

Gate: `scripts/fmt.sh --check`, single-crate checks and targeted nextest for
`bbox-corpus-core` and `bbox-indexing` locally, commit and push, full cluster
verification on the pushed ref, and the P2-A bridge bootsmoke of section 12
(no daemon behavior change expected; the smoke asserts the bridge still
boots and serves).

## 6. Milestone P2-B: v2 runtime path and access authority

### 6.1 Startup mode selection and refusal set

Implement section 4.1 in `src/server/open.rs` with these explicit refusals,
each a typed startup error emitted before any route binds:

- v2 catalog whose strict pair open fails (corrupt, dangling attachment,
  duplicate scope/alias, epoch mismatch between files);
- v2 catalog with `MigratedV1` origin and a missing or mismatched committed
  migration marker (marker loss is distinguishable from fresh v2 per
  governing section 5.1);
- v2 catalog present while the legacy `projects.json` shape also decodes
  (cannot happen at one path; the probe decodes once and the version field
  decides; a file that decodes as neither fails closed);
- absent catalog with any sibling catalog-family artifact present
  (attachment snapshot, journal, committed marker, migration receipt,
  migration assets, stage/backup artifacts; lock files excluded): the
  section 4.1 half-pair refusal;
- unsupported version numbers.

The mode-selection test matrix covers every row above plus the two healthy
modes, with dedicated half-pair rows for the receipt and assets artifacts
and a row proving a healthy migrated store (catalog present with retained
receipt/assets) boots catalog mode normally.

The daemon never rewrites, migrates, or "repairs" project state at startup in
either mode. Recovery of an interrupted pair transaction is the store owner's
journal recovery inside `open_existing`, unchanged, with one bounded
refinement recorded as D-029: a migration-kind journal in the terminal
committed state is verified registry-free (installed pair images against the
journal's new hashes plus the existing origin/marker binding), because every
successfully migrated root retains its committed journal as a GC root and a
bare `open_existing` on such a root is exactly the catalog-mode daemon open.
Non-terminal migration journals still refuse without the complete code-owned
participant registry.

### 6.2 Catalog-backed compatibility projection at runtime

Catalog mode must feed the many consumers that still take `ProjectRecord`
rows (indexing passes, providers, knowledge/gap views, doctor) without
letting them near the catalog types. Add a runtime projection owner beside
the store:

- built from the current `ProjectCatalogState` exclusively through
  `validate_catalog_attachments` + `ProjectRecord::from_catalog_attachment`,
  the exact P1-D section 7.2 contract: one row per attached project, no row
  for remote-only projects, plus an `omitted_catalog_count`;
- rebuilt only on catalog epoch change (admin commits publish the new state
  and invalidate the projection atomically with the in-memory snapshot swap);
- never persisted: the projection is in-memory derived state. In catalog
  mode nothing writes `LegacyProjectStoreV1` bytes, and `StorePersister` for
  projects does not run.

"Unconverted consumers are oblivious" is not free: several startup- and
lifecycle-reachable consumers read `projects.json` from disk or take
`ProjectRegistry` by type, and none of them can call the projection or the
resolver across the crate boundary. P2-B therefore includes the injection
refactor that gives them constructor-supplied typed inputs. The daemon-side
carrier is one shared snapshot value:

```text
ProjectRecordsSnapshot {
    records: Arc<Vec<ProjectRecord>>,
    omitted_catalog_count: u64,
    authority_epoch: u64,
}
```

published by the project authority and injected into the enumerated
consumers. Freshness is unrepresentable-by-construction, not disciplined:
`ProjectRegistry` gains an internal mutation counter bumped by every
`&mut self` mutation (register, rename, unregister, alias sync, language
backfill, and any future mutator, including the startup-internal ones), and
the bridge snapshot is derived on read whenever the counter disagrees with
the cached snapshot's `authority_epoch`, so no call site can forget to
republish. In catalog mode `authority_epoch` is the catalog epoch and the
snapshot is the projection above, republished on epoch change. The
enumerated consumers:

- `TranscriptIndex::open_or_create_with_code_source_store_path` and its
  internal `load_active_code_selectors(projects_path)`: the selector load
  takes the snapshot (or a selector list derived from it by the caller)
  instead of reading `projects.json` off disk;
- `refresh_active_code_selectors`: same injected input on every refresh;
- `build_index` in `bbox-corpus-index/src/index/search.rs`: the
  registered-project guard consumes the injected snapshot;
- `IndexWriterActor::spawn_for_with_checkout_access`: accepts a snapshot
  provider (republished on change) instead of
  `Arc<RwLock<ProjectRegistry>>`;
- `build_startup_edge_index` in `src/server/open.rs`: takes the snapshot
  rather than `&ProjectRegistry`;
- `ProviderContext.projects` in `bbox-providers`: same snapshot-provider
  seam.

`bbox_corpus_core::project_record::load_project_records` keeps existing for
offline/CLI/test use, but after P2-B it has no daemon-runtime caller: every
runtime consumer receives injected snapshots, preserving the crate
dependency direction (the same seam shape section 9.2 gives the B1 search
filter). The P2-B gate asserts catalog-mode boot performs zero v1-shaped
disk reads of `projects.json`: the fixture boot would fail loudly if one
remained (v2 bytes do not decode as `ProjectStoreView`), and a
callers-audit test pins the absence of daemon-runtime `load_project_records`
call sites.

Identity-set seeding is deliberately wider than the attached-only record
rows, because the record rows exist for path-bearing consumers while the
active-selector map and the edge registered-project set are corpus identity
surfaces. The snapshot therefore carries, beside `records`, the complete
catalog project-id set:

```text
ProjectRecordsSnapshot {
    records: Arc<Vec<ProjectRecord>>,          // attached projects only
    corpus_project_ids: Arc<BTreeSet<String>>, // every catalog project
    omitted_catalog_count: u64,
    authority_epoch: u64,
}
```

In catalog mode the active-selector map (`load_active_code_selectors` and
its refresh) and `build_startup_edge_index`'s registered-project set are
seeded from `corpus_project_ids`, and the effective-manifest selector
override is gated on catalog membership rather than attached-record
membership, so a remote-only project's migrated collected generation stays
selected, searchable, purge-protected, and edge-readable with zero
attachments. On the bridge, `corpus_project_ids` equals the record id set,
which is byte-identical to today's derivation, so bridge behavior is
unchanged. This is a deliberately bounded pull-forward of exactly the
seeding rule of governing section 10.3 ("seed code selectors, vector
selectors, edge registered-project sets from the catalog"): Phase 3 keeps
ownership of source-neutral input, the relative-path schema, history
materialization, and rebuild; Phase 2 only stops the identity gate from
hiding already-materialized collected state, which is the "skipped, hidden
from readers" failure of governing section 4. A dedicated test boots the
migrated fixture and asserts the remote-only project's collected documents
are served by code search and its edges readable, with zero lease
acquisitions.

Startup ordering preserves the repo-owned-store invariant: the projection
and snapshot exist before knowledge/gap loading so committed repo files load
exactly as they do today under bridge mode (the `src/server/open.rs`
load-before-save ordering is unchanged in both modes).

### 6.3 V2 checkout-access authority

`V2CatalogCheckoutAccessAuthority` implements the existing
`CheckoutAccessAuthority` trait over the catalog state:

- `Selected` and `AttachmentId` selectors resolve through active attachments
  with catalog cross-validation; `CheckoutId` maps through active attachments
  for that checkout; `LegacyPath` resolves through the shared resolver's v2
  path arms and is counted on the compatibility lane exactly like today, so
  the retirement telemetry keeps one meaning across modes;
- capability checks read `CheckoutAttachment.capabilities`; a capability is
  revalidated at acquisition (conservative read/write path gate, checkout-id
  marker match, recorded scope match against `validated_scope`), never
  granted merely because the directory exists, per governing section 5.2;
- detached attachments and detached projects deny with the existing error
  codes (`AttachmentInactive`, `AttachmentNotFound`);
- `DenyCheckoutAccess` continues to work unchanged for remote-only tests.

The broker construction in `src/server/state.rs` selects the authority by
mode. Everything downstream of the broker (leases, guards, observations,
health) is untouched.

### 6.4 Isolated v2 runtime fixture

Add one reusable integration fixture (root-crate `tests/`) that:

1. builds a version-1 fixture state (projects, knowledge, coordination rows,
   publisher pin, collected activation) in a tempdir;
2. runs the real CLI preflight and apply into an isolated rehearsal root
   through the public facade;
3. opens the daemon runtime against that root in catalog mode (in-process
   server harness, throwaway state, isolated port for the live variant).

This fixture is the substrate for every "prove only against isolated
migrated v2 state" obligation in this plan, and its assertions accumulate
across milestones up to the exit-gate proof of section 10.

Catalog-mode fixtures and smokes in this phase run with `code_collection`
disabled, or with producer assignments limited to attached projects: the
collector's `build_snapshot` acquires leases per attached project and
resolves configured scopes against records, and remote-only configured
scopes cannot resolve until Phase 4 converts grant resolution to catalog
scope. The section 10 zero-lease assertion is stated for and only holds
under disabled collection; enabling collection against a catalog-mode
daemon is Phase 4 territory.

### 6.5 P2-B tests and gate

- mode-selection unit tests over all decode outcomes, including every
  refusal in section 6.1;
- projection tests: attached-only rows, omitted count, epoch-change rebuild,
  remote-only project excluded, byte-parity of projected rows against the
  facade verification projection for the same fixture;
- authority tests: every `CheckoutAccessKind` acquires against a valid
  attachment and denies against detached/missing/scope-mismatched ones; the
  compatibility-lane counters record `LegacyPath` resolutions;
- injection-seam tests: every enumerated section 6.2 consumer builds and
  runs from an injected snapshot; a callers-audit test proves
  `load_project_records` has no daemon-runtime call site; catalog-mode boot
  on the fixture root performs zero v1-shaped disk reads of `projects.json`
  (boot succeeds where any surviving read would fail the decode);
- identity-seeding tests: bridge `corpus_project_ids` equals the record id
  set (derivation parity), and on the migrated fixture the remote-only
  project's collected documents are served by code search and its edges are
  readable with zero attachments and zero lease acquisitions;
- bridge freshness test: a registry mutation through any mutator makes the
  next snapshot read observe the new state (mutation-counter derivation);
- fixture round trip of section 6.4 with catalog-mode open succeeding and
  bridge-mode open of the same root refusing (version mismatch).

Gate: local targeted tests, commit/push, full cluster verification, and the
P2-B bootsmoke: bridge smoke unchanged, plus the isolated catalog-mode
daemon boots on the rehearsed root, serves `/admin/runtime-metrics`, resolves
an id and an alias with zero attachments, and fails closed on an unknown
absolute path (section 12).

## 7. Milestone P2-C: administration vocabulary

### 7.1 Ownership, proof split, and audit model

`crates/bbox-indexing/src/project_catalog_admin.rs` (new) implements every
operation as: validate inputs -> build post-images in a `transact` closure ->
return a typed receipt `{ epoch, catalog_sha256, attachments_sha256, ... }`.
No operation writes either file directly; no operation holds any lock across
filesystem probing (probes run before `transact`, revalidation runs inside
the closure against the cloned snapshots).

Surface split (D-004):

- MCP tools (attachment-proved, delegated-automation callable): attach,
  detach, register composite, promote, attachment-proved scope migration,
  publisher bind/rebind, default-attachment selection, catalog list/get
  (read-only), alias nomination listing (read-only);
- offline CLI (proofless authority, exclusive lifetime lock, daemon
  stopped): catalog add/import, alias accept/reject, operator-attested
  unattached scope migration, retire/delete. Read-only list/get also exists
  on the CLI for operability.

Epoch discipline is explicit per tool class. The dedicated admin tools
(attach, detach, promote, scope migration, publisher bind,
default-attachment) require `expected_catalog_epoch` and a bounded
`audit_reason`; a stale epoch is a typed refusal. The converted lifecycle
composites (register, rename, unregister) take no caller-supplied epoch:
they are operator-idempotent composites whose preconditions are revalidated
inside the transaction closure, so they read the current snapshot, CAS on
exactly that snapshot in one attempt, and surface a typed refusal on a
concurrent mutation rather than retrying with a fresh epoch.

Authority-changing operations additionally require explicit operator
acknowledgement flags that agents pass through but never default or infer (`acknowledge_repo_authority_change` on repo-authority scope
migration; `acknowledge_unattached_scope_migration` exists only on the CLI
surface). Durable audit lives where the governing design places it: scope
transitions write `ScopeMigrationRecord` (+ attachment proof when proved);
other admin mutations are journaled epoch-bumping transactions whose
receipts carry hashes, and their `operator_invocation`-class data is bounded
in the tool response and logs, not duplicated into a parallel audit store
(D-012 rationale).

On the bridge, every catalog-mutating MCP admin tool returns
`error.project_catalog_inactive` (tool-surface registration is unconditional
so the tool docs and roster stay stable; visibility filtering remains
possible but is not relied on for authority, per D-004).

### 7.2 Catalog add/import and list/get

- CLI `blackbox project-catalog add --scope <repo_authority> <relpath> |
  --legacy-local --display-name <name> [--alias <a>...]`: creates a project
  with a minted id, `Published` scope (unowned scope required) or
  `LegacyLocal`, optional initial operator aliases (uniqueness enforced),
  `repo_history` per governing section 5.1 (recorded-authority record
  find-or-create for published; minted local record for legacy-local with a
  `local_` namespace). This is the explicit operator action that creates
  remote-only projects; producer traffic and config reload never create
  projects.
- `bbox_project_catalog_list` / `bbox_project_catalog_get`: complete
  inventory including remote-only projects, path-free responses (ids,
  scopes, aliases, nominations, display names, repo-history references,
  attachment counts and ids, but never attachment paths in list; `get` may
  include this host's attachment paths since attachment data is host-local
  operator data, clearly separated in the response shape).
- legacy `bbox_project_list` keeps its attached-`ProjectRecord` rows and, in
  catalog mode, adds `omitted_catalog_projects` plus a pointer to the new
  list tool. Bridge mode reports zero omitted.

### 7.3 Attach, detach, and default attachment

`bbox_project_attach { project, path, kind, expected_catalog_epoch,
audit_reason }`:

- resolves the target project (Selection class);
- canonicalizes and probes the path off-lock: checkout-id marker
  (`ensure_checkout_id` semantics: reuse existing, mint via the shared
  `.bbox/local` lock if absent), committed-scope resolution at `HEAD` when
  the checkout records authority, conservative gate, kind detection (base /
  linked worktree / managed clone);
- inside the transaction revalidates: project exists at the expected epoch;
  active-uniqueness `(project_id, checkout_id, project_root_relpath)`;
  cross-project exclusivity of `(checkout_id, project_root_relpath)`; for
  `Published` projects the resolved committed scope must equal the catalog
  scope exactly, including the relpath (strict cross-validation enforces
  `validated_scope` presence and equality for attached rows, so a checkout
  without committed recorded authority cannot attach to a `Published`
  project at all; a checkout resolving a different scope receives the exact
  `scope_migration_required` / `scope_promotion_required` refusal of section
  9.1 rather than an attachment); for `LegacyLocal`, `validated_scope` is
  absent by construction and an explicit project id is required when no
  scope can prove identity (reattachment rule of governing section 7.2);
- capabilities are derived from observed checkout shape at attach time and
  recorded; acquisition still revalidates per capability.

`bbox_project_detach { attachment_id, expected_catalog_epoch, audit_reason }`
marks the attachment `Detached` with `detached_at`, leaves every logical
store, entity ref, and generation untouched, and reports (in later phases:
degrades) publisher freshness when the detached attachment is
publisher-bound. Census and watcher deregistration is scoped to the detached
attachment's `(checkout_id, scope/project)` pair, never the whole checkout:
a monorepo checkout carrying sibling attachments for other projects keeps
the siblings' census rows, watcher coverage, and overlay discovery intact
(the census key is composite for exactly this reason). Detach never deletes
catalog data (governing decision 8 boundary).

`bbox_project_default_attachment { project, attachment_id | clear,
expected_catalog_epoch, audit_reason }` records the operator-selected default
local-source attachment used by path operations when no session pin or
explicit selector is present. The default is host-local attachment data
(stored on the attachment snapshot side), never catalog data.

### 7.4 Promotion

`bbox_project_promote { project_id, attachment_id, proposed_scope,
expected_catalog_epoch, audit_reason }` per governing section 7.2:

- requires the exact project id (register refusals hand it to the operator);
- revalidates the designated attachment and its committed authority to the
  proposed scope; every other active attachment of the project must
  revalidate to the same scope or the operation refuses
  (`error.project_selector_ambiguous` class with per-attachment diagnostics);
  the designated attachment cannot overrule siblings;
- proves the scope unowned; an owned scope refuses and points to the offline
  compatibility workflow, never merges;
- in one pair transaction: flips the same record to `Published(scope)`,
  writes the `ScopeMigrationRecord { kind: Promotion, provenance:
  AttachmentProved }` with the new epoch, writes the matching
  `ScopeMigrationAttachmentProof`, and performs the repo-history authority
  transition of governing section 5.1 (create recorded record if none;
  change `LocalProject`/`LegacyNamespace` authority to `Recorded` preserving
  `RepoHistoryId`, primary namespace, compatibility namespaces; conflicting
  sibling authority blocks);
- preserves every project-id-keyed store untouched (they key by id, which
  does not change).

### 7.5 Scope migration

`bbox_project_scope_migrate { project_id, expected_old_scope, new_scope,
kind: relpath_move | repo_authority_change, attachment { attachment_id },
dry_run, acknowledge_repo_authority_change?, expected_catalog_epoch,
audit_reason }` implements the attachment-proved channel: same checkout
identity, committed config resolving to the new scope, all sibling
attachments agreeing or detached, target scope unowned, and the pair
transaction writing catalog scope + `ScopeMigrationRecord` + proof +
host-local path bindings. A repo-authority change requires the explicit
acknowledgement and records the former authority as a non-authoritative
compatibility note on the history record, preserving the established primary
namespace.

`blackbox project-catalog scope-migrate --operator-attested
--acknowledge-unattached-scope-migration --reason <bounded>` implements the
offline channel for zero-attachment projects: refuses if any active
attachment exists, requires the exclusive lifetime lock, writes the record
with `OperatorAttested` provenance (reason mandatory, enforced by strict
validation) and no proof row. Both channels accept only `relpath_move` and
`repo_authority_change`; promotion is never operator-attested, and strict
validation makes an `OperatorAttested` promotion record unrepresentable, so
an unattached `LegacyLocal` project cannot promote.

Bridge generations: when the project has an active collected generation or
an accepted publication pointer, the transaction records
`code_bridge_generation` / `publication_bridge_generation` on the migration
record; clearing them is Phase 4/5 behavior and out of scope here. Phase 2
must only guarantee the record is written and survives recovery, and that
startup validation treats the exact record as the only allowed
catalog/activation scope disagreement (asserted by the isolated fixture, not
by new live wiring).

Fault injection: both channels are exercised at every pair-transaction crash
boundary; recovery yields the complete old or complete new catalog/attachment
epoch with its matching migration record and proof, never one without the
other (governing section 17 rows).

### 7.6 Alias nomination lifecycle

- Nomination ingestion (catalog mode): register/attach reads the committed
  `.bbox/config.toml` aliases at the resolved commit and records well-formed,
  non-colliding entries into `nominated_aliases` (bounded count and bytes,
  validation per governing section 5.1). Startup and reload do not rewrite
  aliases; they report pending nominations. Bridge mode keeps today's
  materializing `sync_declared_aliases` behavior untouched.
- `blackbox project-catalog alias accept --project <id> --alias <a>` /
  `alias reject ...`: explicit local catalog-authority actions (D-005);
  accept enforces uniqueness against every id and accepted alias, moves the
  entry to `operator_aliases`; reject drops the nomination. Both are
  epoch-bumping pair transactions with unchanged attachment post-image.
- Read surfaces (`catalog_get`, register/attach responses) list pending
  nominations and the exact epoch-checked accept command string (the epoch
  check holds across a daemon stop because the epoch is durable pair state;
  a nomination accepted against a stale epoch refuses and the operator
  re-reads).
- A missing or changed committed declaration never revokes an accepted alias.
- Channel choice is deliberate and is steady-state policy, not a Phase 2
  stopgap: alias acceptance changes host-wide selector authority with no
  repository proof, which is exactly the class D-004 keeps off model-facing
  routes ("operations without that proof remain local rather than trusting a
  request boolean as identity"), and D-005 names acceptance an explicit
  local catalog-authority action. The operational cost is bounded because
  nominations are rare, batched acceptance is one daemon stop, and rejected
  or pending nominations impair nothing (selectors keep working through ids,
  accepted aliases, and paths). The revisit trigger is D-004's: an
  authenticated operator capability on the transport, not convenience.

### 7.7 Publisher binding administration (bounded)

Phase 2 implements binding administration only; generation creation, lane
validation, and live view wiring remain Phase 5:

`bbox_project_publisher_bind { project_id, attachment_id,
expected_catalog_epoch, audit_reason }` rebinds the publisher attachment
only. It validates that the attachment belongs to the project, revalidates
its scope, and requires the new attachment's object database to contain the
pointer's current `accepted_commit` (the containment a later advance and
overlay recomputation need). The pointer rewrite changes `attachment_id`
and nothing else: `full_ref`, `accepted_commit`, `accepted_scope`, the
accepted generation, and every payload byte are unchanged, so the strict
startup agreement between pointer and immutable generation
(`verify_generation_binding`: ref, commit, scope, generation id, hashes)
holds identically before and after. Changing the full ref or the accepted
commit is exclusively the Phase 5 atomic advance path, which writes a new
generation and swaps ref, commit, and generation together; Phase 2 offers no
operation that can make the pointer disagree with its generation. Rebinding
after detach therefore restores a live publisher selection for the later
advance while the existing accepted generation keeps serving. When no
pointer exists (no G1 was seeded and the migration recorded
`no_published_content_acknowledged`), bind refuses with a typed error naming
the migration disposition; inventing a pointer without a generation would
violate the pointer/generation agreement invariant of governing section
13.1.

Pointer writes use the accepted-publication store lock and atomic-replace
conventions established by the Phase 1 G1 machinery; the pointer is not a
catalog participant, so this operation is not a pair transaction, and its
receipt carries the pointer hash. Crash safety: single atomic pointer swap.
Regression test (required): bind to a new attachment, then strict selected
verification must serve the exact same accepted generation and hashes.

### 7.8 Retire and delete

`blackbox project-catalog retire --project <id> [--execute]` (CLI-only,
exclusive lock):

- inventories, through the Phase 1 owner-snapshot capture surface, every
  reference class: active/retained collected generations, accepted
  publication pointer/generations, attachments (active or detached rows),
  entity refs in index/vector/edge owners, project-scoped knowledge/gap/
  coordination rows, artifacts, and producer assignments (from the effective
  source manifest);
- default mode reports the bounded per-class counts and refuses;
- `--execute` succeeds only when every external reference class is zero
  (detached attachment rows, the project's own scope-migration audit chain,
  and stale mapped path bindings whose store rows are gone do not block).
  It then removes, in one pair transaction, the project, its
  now-unreferenced `LocalProject` history record if any, its scope-migration
  records with their matching attachment proofs, all of its attachment rows,
  and its mapped legacy-path bindings; strict cross-validation forbids
  leaving any of those behind (dangling-project, dangling-migration, and
  dangling-proof rules), so partial removal is not representable. The
  journal, its backups, and the migration receipts remain the historical
  audit for a retired project. Producer assignments must have been removed
  first (governing decision 12); any nonzero class points to the
  class-specific discharge surface. The full destructive-retire discharge
  workflow is out of scope (section 3).

### 7.9 MCP registration, docs, and bridge behavior

New tools: `bbox_project_catalog_list`, `bbox_project_catalog_get`,
`bbox_project_attach`, `bbox_project_detach`,
`bbox_project_default_attachment`, `bbox_project_promote`,
`bbox_project_scope_migrate`, `bbox_project_publisher_bind`. All are async
handlers using `spawn_blocking` for store work, registered unconditionally
with `tool_docs.rs` stanzas (missing stanzas fail tests), and all mutating
ones return `error.project_catalog_inactive` on the bridge. New CLI
subcommands (`add`, `list`, `get`, `alias accept|reject`,
`scope-migrate --operator-attested`, `retire`) reuse the single versioned
envelope, exclusive-lock, and receipt conventions of D-020/D-021; help and
version stay side-effect-free.

The Phase 2 boundary for these CLI subcommands is enforced by construction,
and deliberately so: every mutating subcommand acquires its authority
through `ProjectCatalogStore::open_existing` on an explicitly selected v2
root, so pointing one at configured operator state (a v1 store) fails
closed on the version probe. That implicit enforcement is intentional
D-002 posture; an implementer must not "fix" any of these subcommands into
a path that can create, migrate, or mutate v2 state at the configured
projects path. Creating v2 state at configured paths remains exclusively
the Phase 6 apply.

### 7.10 P2-C tests and gate

- per-operation unit tests over hand-built states: success, epoch CAS
  failure, every refusal named in sections 7.2-7.8;
- promotion/scope-migration fault injection through the store's existing
  `FaultPoint` seam: complete-old or complete-new with matching
  record/proof;
- multi-attachment promotion agreement and sibling-conflict refusals;
- multi-attachment monorepo detach: detaching one project's attachment
  leaves the sibling project's census row, watcher, and overlay discovery
  untouched;
- alias accept/reject uniqueness matrix (vs ids, accepted, nominated);
- publisher bind refusal without pointer; bind rewrites binding only
  (payload hashes unchanged);
- retire inventory counts against a populated fixture; execute refusal and
  success paths;
- CLI envelope tests for each new subcommand (parser, exclusivity with a
  running daemon via the shared lock, JSON envelope, exit codes);
- isolated fixture (6.4) extended: attach -> write -> detach -> reattach
  round trip; register-refusal handoff into promote; end-to-end
  attachment-proved scope migration on the migrated root.

Gate: local targeted tests, commit/push, cluster verification, and the P2-C
bootsmoke: on the isolated catalog-mode daemon, an MCP attach/detach round
trip and a promotion succeed against throwaway checkouts; on the bridge
smoke, `bbox_project_attach` returns `error.project_catalog_inactive` and
the version-1 surfaces behave identically to the Phase 1 head.

## 8. Milestone P2-D: project-id fields, dual-read, and ledger integration

### 8.1 Versioned owner set and field additions

The logical owner set for this phase is exactly the fourteen
`LegacyPathStoreKindV1` kinds. Concretely:

| owner | store | change |
|---|---|---|
| Knowledge | `KnowledgeEntry` | add `project_id: Option<String>` (serde default), stamp on write |
| Gap | `Gap` | same |
| Thread | `Thread` | same (beside required `project`) |
| Note | `Note` | same |
| Pin | `Pin` | same (beside `project` and `project_alias`) |
| Roadmap | `RoadmapItem` | same |
| Packet | `Packet` | same |
| Whiteboard | `Whiteboard` | same |
| SlackBinding | `SlackChannelBinding` (has `project_id`), `SlackProposalLink` | normalize stamping; add field to proposal links |
| Task | task records | additive `project_id: Option<String>` stamped only when ambient resolution succeeds; `cwd` remains untouched execution history |
| Proposal | consultant proposals | dual-read only; modern rows already id-keyed |
| Artifact | artifact metadata | already carries `project_id`; `project_path` becomes explicitly display-only (no read path consults it for identity) |
| Provenance | provenance/edge anchors | already id-keyed; assert with tests, no field change |
| TranscriptEdge | edge sidecar rows | already id-keyed; assert with tests, no field change |

Stamped values are the resolving authority's project id: the eight-hex
record id on the bridge, the catalog id in catalog mode. Both remain valid
across migration because migrated ids are preserved byte-for-byte
(governing decision 1), which is what makes bridge-time stamping durable
rather than throwaway.

### 8.2 Dual-read resolution

Every owner store's project-scope predicate becomes: match on `project_id`
when both the row and the query carry one; otherwise match on the existing
path key exactly as today; and, in catalog mode only, additionally match a
path-only row whose literal path maps through the host-local
`LegacyPathBinding` ledger (`Mapped`, root or contained-subdirectory) to the
query's project id. The ledger arm is what keeps pre-migration path-only
rows visible after attachment relocation stops rewriting them, and it is
the read-side consumer governing section 7.3 built the ledger for. Bridge
mode has no ledger and uses the first two arms, which is exactly today's
behavior. Query-side ids come from the resolver (`Selection`/`Filter`
outcomes carry `project_id()`); rows without ids are the legacy lane. No
store rewrites existing rows; no read path drops a row it would have
matched before. The path predicate is removed only at the
observation-gated cut (Phase 6+), which requires the mappable ledger
complete, empty ambiguity quarantine, every unmappable row classified
`UnscopedLegacyPath`, and a clean compatibility-read observation window
(governing section 7.3); this plan changes none of those cut criteria.

Unregistered writes: on the bridge, surfaces that permit them keep writing
the raw scope key with `project_id: None` (today's behavior); in catalog
mode those rows are the typed unscoped lane by definition (no id, no
catalog authority, exact/raw query semantics), and knowledge/gap writes keep
their existing checkout-identity requirement. No new mechanism is invented;
classification and authority semantics attach to the absence of an id.

### 8.3 Execution targets stay paths

Teams `project_dir`, poller/cron/webhook `default_project_dir`, workflow
ambient cwd, task `cwd`, render output locations, and watcher roots remain
execution-path data. None of them gains identity semantics in this phase.
Rename relocation keeps rewriting them on the bridge so scheduled and
dispatched work keeps executing in the moved checkout (including the webhook
fix of section 4.3).

### 8.4 Ledger integration and coverage reconciliation

- `migrate_project_refs` gains gaps, roadmap, and webhooks;
  `project_ref_counts` gains gaps, roadmap, webhooks, and artifact metadata
  references, so the unregister force gate sees the full picture. Store
  enumeration order and per-store counts land in both tool responses.
- Attachment relocation (catalog mode `bbox_project_rename`, section 9.1)
  appends `LegacyPathLedgerEntry { historical_path, source_store,
  source_row_id, inventory_epoch, status: Mapped }` rows for the relocated
  root inside the same pair transaction that rewrites the attachment, and
  does not rewrite owner-store rows: dual-read plus the ledger keep old rows
  resolving. Bindings are append-only through the compatibility epoch;
  bounded counts (not paths) surface in doctor and the migration ledger
  reporting.

### 8.5 P2-D tests and gate

- per-store serde round trip (old rows decode, new field optional), stamp-on
  -write, dual-read matrix (id+id, id+path, path-only, cross mismatch);
- rename coverage tests: gap and roadmap rows follow a bridge rename;
  webhook `default_project_dir` follows; force-gate counts include the new
  stores;
- ledger append test on the catalog-mode relocation path (with section 9.1);
- parity: bridge-mode fixtures assert no read result changes for rows
  without ids.

Gate: local targeted tests, commit/push, cluster verification, P2-D bridge
bootsmoke with the milestone assertion that a knowledge write and a note
write under a registered project carry stamped ids and remain readable by
path selector.

## 9. Milestone P2-E: composite conversion and selector routing

### 9.1 Register, rename, unregister, init, eject conversion

The five lifecycle tools dispatch on `ProjectAuthority`. Bridge arms call
the existing version-1 logic unchanged (plus the section 4.3 fixes). Catalog
arms implement governing section 7.2:

- `bbox_project_register` becomes the compatibility composite: resolve
  committed scope; find by validated scope and active attachment
  (idempotency is scope+attachment, not path hash); create `Published` on a
  newly recorded scope (operator invoked registration on that checkout is
  the authority) or `LegacyLocal` with a minted id for unrecorded/non-Git
  checkouts; then attach in the same pair transaction. Newly committed
  authority on a `LegacyLocal` attachment returns `scope_promotion_required`
  with the exact project id and proposed scope; an existing project
  validating to a different scope returns `scope_migration_required` with
  the exact dry-run command; neither creates a second project. Nomination
  ingestion per section 7.6. The post-register enrichment pipeline
  (artifacts, provenance import, watchers, kb roots, transcript backfill)
  runs behind the same capability leases in both modes.
- `bbox_project_rename` becomes attachment relocation: same checkout id and
  same validated `PublishedScope` after the move, committed in one pair
  transaction that updates the attachment path fields and appends the
  section 8.4 ledger rows. Markerless `LegacyLocal` rename refuses with the
  checkout-init / detach-and-reattach instruction; path existence and inode
  reuse never prove sameness. Relpath moves and repo rebinds refuse with the
  scope-migration pointer. Catalog-mode rename does not rewrite owner-store
  rows.
- `bbox_project_unregister` becomes detach (section 7.3) and leaves logical
  state; its response points at retire for catalog deletion. `force`
  semantics on the bridge are unchanged.
- `bbox_project_init` stays a filesystem workspace initializer in both
  modes; in catalog mode it reports promotion as the required next action
  when it records new authority on an attached `LegacyLocal` project.
- `bbox_project_eject` requires the `RepoMutation` capability lease in both
  modes (it already does) and gains no catalog semantics beyond resolving
  through the shared resolver.

### 9.2 Caller conversion table

Every project-selector surface routes through the engine via three daemon
wrappers, reimplemented in place so call sites keep their names:
`resolve_project_write` (Selection/Write), a new `resolve_project_selection`
(Selection/Read, replacing ad hoc `resolve_project_context` + bespoke arms),
and `rescope_project_filter_value` (Filter). Conversion by family, with the
class taxonomy fixed here:

| family | surfaces | class | notes |
|---|---|---|---|
| corpus search | `bbox_search`, `bbox_cite`, `bbox_sessions_list`, `work_tool_calls` | Filter | B1 retired: `bbox-corpus-index` search gains a typed pre-resolved filter input `{ project_id: Option<String>, literal: String }`; resolution moves to the daemon/mcp-tools boundary (dependency direction forbids calling the engine from `bbox-corpus-index`). Literal lane semantics preserved verbatim. |
| hybrid/graph search | `bbox_hybrid_search`, `bbox_discover_seed_entities` | Filter | B2 converted to the engine; the v1 arm keeps the eight-hex pass-through and hash fallback as tagged compatibility outcomes; the v2 arm never mints identity. |
| knowledge | learn/remember/decide, `bbox_knowledge`, render/absorb/bootstrap, knowledge/gap views | write=Selection, list=Filter | wrappers reimplemented on the engine; existing fallback-cut and checkout-identity guards unchanged. |
| gaps | `bbox_gap`, `bbox_gaps`, resolve/update | same as knowledge | |
| coordination | threads (x2 + roadmap promote), notes, pins, inbox, roadmap, whiteboards | write=Selection, list=Filter | the three direct `fleet_worktree_scope_and_dir` call sites route through the engine's worktree arm. |
| graph/provenance | `bbox_ref_size`, `bbox_blame`, provenance export/import, `bbox_edge_compact` | Selection | B3 folded into the engine id arm. `bbox_edge_compact` keeps raw-id behavior on the v1 arm (tagged) and fails closed on unknown ids on the v2 arm. |
| admin/storage | lifecycle tools (9.1), storage tools, `bbox_mcp_surface` | Selection (storage: Filter) | B6 raw pass-through preserved on v1 as documented compatibility, tagged; B8 aligned with H1: both resolve, both fall back to the literal on the v1 arm only. |
| slack/orchestration config | `bro_slack_bind`, `bro_mcp` project scope | Selection | B4/B5 route through the engine; unregistered-path storage behavior preserved on v1, tagged. |
| HTTP | `/mcp?project=` | Filter | resolves via the engine; literal fallback v1-only, tagged. |
| dispatch plane | `bro_*`/`work_*`/control-plane `cwd`/`project_dir` | none | out of scope: execution targets (section 3); only the existing ambient-pin resolution moves onto the engine wrapper it already uses. |

Compatibility-lane tagging: every v1-arm outcome that the v2 arm would
refuse (raw pass-through, hash fallback, literal fallback, raw sidecar id)
increments a per-surface compatibility counter surfaced through
doctor/health beside the existing checkout-access lane counters. These are
the observations the Phase 6 cut will consume.

### 9.3 Bespoke resolver retirement

B1-B8 as listed in section 2.2 are each either reimplemented on the engine
(B2, B3, B4, B5, B8), moved above the crate boundary (B1), or preserved as
tagged v1 compatibility semantics behind the engine (B6, the hash arm of
B2). No bespoke selector code path survives outside the engine after P2-E;
`resolve_project_context` and its helpers survive only as the internals of
the v1 backend.

### 9.4 Parity harness and observability

The section 5.5 selector corpus is extended surface-by-surface as callers
convert: for each converted surface, a fixture asserts identical bridge-mode
observable behavior before/after conversion (same results, same store
mutations, same error strings for the anyhow lanes, same counters modulo the
new telemetry). The corpus includes the cross-checkout key-convergence
cases: a path-only legacy row written under the base path must be returned
to a worktree-pinned session, and a worktree-pinned write must land under
the base store key and be visible to a base-pinned query, in both modes
(bridge via the v1 key-to-base behavior, catalog via the section 5.3 base
rule plus the section 8.2 ledger arm). The parity harness is a test-only
crate-level utility, not a runtime feature.

### 9.5 P2-E tests and gate

- conversion parity fixtures for every family in the table;
- catalog-mode behavior tests for the Selection surfaces: unknown and
  ambiguous selectors fail closed with section 5.4 codes; Filter surfaces
  keep literal semantics without identity;
- register/rename/unregister/init/eject catalog-arm tests: composite
  find-or-create, both refusal handoffs, relocation with ledger append and
  no store rewrite, detach semantics, markerless-rename refusal;
- lint: `scripts/lint-concurrency.sh` and clippy stay clean (no new sync
  handlers, no blocking calls in tool modules).

Gate: local targeted tests, commit/push, full cluster verification, P2-E
bootsmokes (section 12), then the exit-gate proof of section 10.

## 10. Exit-gate proof

Extend the section 6.4 isolated fixture into the phase acceptance test,
executed in CI (integration test) and live (catalog-mode bootsmoke):

1. migrated root contains: an attached published project, an attached
   `LegacyLocal` project, a remote-only published project with an active
   collected generation and zero attachments, a project with two attachments
   (base + worktree), and migrated coordination/knowledge rows;
2. id, accepted-alias, and explicit-scope selectors resolve for the
   remote-only project with zero attachments and zero lease acquisitions
   (assert via observation counters and under `DenyCheckoutAccess` where
   applicable): hybrid-search and transcript-search filters resolve to the
   project id and return the migrated collected-code results;
   `bbox_project_catalog_get` returns the complete record; knowledge and gap
   list surfaces resolve the selector and return their typed
   no-authority/empty outcome with response stamps and without a lease.
   Serving accepted published knowledge/gap content for an attachment-less
   project is the Phase 5 view wiring and is explicitly not asserted here;
3. path operations (render, blame target selection, knowledge write) on the
   two-attachment project require a session pin, explicit attachment id, or
   configured default, and succeed with exactly one attachment selected;
4. unknown absolute paths, unknown ids, duplicate aliases, and equal-depth
   ambiguity fail closed with the section 5.4 codes; no operation
   manufactures a catalog identity;
5. admin round trips from section 7.10 all hold on the same root;
6. the configured-operator-state guard still refuses apply outside an
   isolated rehearsal root, and the bridge daemon at the same commit passes
   the full parity harness.

Rows of the governing section 17 "Resolution and administration" matrix
discharged in Phase 2: convergence of id/alias/scope/path/worktree/clone
selectors; same-scope multi-attachment behavior; equal-depth and alias
fail-closed; detach preservation; rename scope-immutability and
markerless-refusal; promotion preservation/refusal; register
`scope_migration_required`; migration record/proof pairing and fault
injection; unattached-migration refusal conditions; attached-list
omitted-count reporting. Rows deferred with their owning phase: bridge
generation clearing (Phase 4/5), publisher advance and view keying
(Phase 5), path-fallback removal, compatibility-read windows, and the
four-step producer re-scope live drill (Phase 6).

## 11. Concurrency and security rules

- The resolver is pure over pinned snapshots (`Arc<ProjectCatalogState>` or
  a registry snapshot); it takes no locks and performs no filesystem I/O.
  Path canonicalization for selector input happens in the daemon wrappers on
  blocking-safe paths, exactly as today.
- Admin operations probe filesystems before `transact` and revalidate inside
  the closure; no lock is held across Git, filesystem walking, or operator
  wait. Lock order remains: process-lifetime migration lock,
  `projects.json.lock`, auxiliary participant locks in deterministic order.
  The accepted-publication pointer keeps its own store lock; publisher bind
  orders it after the catalog read and holds no catalog lock while writing
  the pointer.
- New MCP handlers are async, use `spawn_blocking` for store and filesystem
  work, and pass `scripts/lint-concurrency.sh` and the clippy
  disallowed-methods gate.
- Acknowledgement flags are operator authority: agents pass them through
  from operator input and never default or infer them (RX-V1 discipline,
  D-004). Tool responses echo which acknowledgements were consumed.
- Epoch CAS is mandatory on every mutating admin surface; a stale epoch is a
  typed refusal, never a retry-with-fresh-epoch inside the daemon.
- Error details, receipts, list responses, and counters remain path-redacted
  by the existing conventions: catalog responses never carry attachment
  paths except in the explicitly host-local sections of `catalog_get` and
  the legacy attached list, and no new error embeds more than the bounded
  selector.
- CLI mutations require the exclusive lifetime lock (daemon stopped) and
  keep help/version side-effect-free. All durable file opens keep no-follow
  semantics.
- Catalog mode never writes `LegacyProjectStoreV1` bytes; bridge mode never
  writes catalog bytes. The only writer of v2 state on this host remains the
  offline CLI against isolated roots plus catalog-mode daemons opened on
  such roots in tests and smokes.

## 12. Live bootsmoke protocol for every milestone

The Phase 1 ten-step protocol is unchanged (build, `which stablesign`,
stable-sign the exact binaries, unused isolated port,
`scripts/dev-isolated-daemon.sh`, listening log with throwaway paths, HTTP
probes, MCP initialize, milestone assertion, graceful shutdown and Trash).
Catalog-mode smokes additionally produce the isolated migrated root first
and verify it with the stable-signed `blackbox` CLI. Per D-030, the root is
materialized by the ignored facade-driving producer test (byte-identical to
the production apply ceremony) rather than by CLI preflight/apply against a
synthetic layout: the CLI preflight source is config-shaped by design, and a
synthetic config-shaped fixture cannot supply live publisher git evidence
(`publisher_git_evidence_missing`). The CLI preflight/apply envelopes were
live-smoked against real state in P1-D; the smoke's CLI role here is
`verify --root` on the produced root. Do not copy, replace,
sign, restart, or signal the production or persistent dev service; any
future need for the persistent dev instance requires the read-only scope
check and explicit operator approval.

Milestone assertions:

- P2-A: bridge boots and serves; a version-1 register/list round trip is
  unchanged.
- P2-B: catalog-mode daemon boots on the rehearsed root; id and alias
  resolve with zero attachments; unknown absolute path fails closed; bridge
  smoke unchanged.
- P2-C: MCP attach/detach and promote succeed on the isolated catalog-mode
  daemon; `bbox_project_attach` on the bridge returns
  `error.project_catalog_inactive`.
- P2-D: bridge writes stamp `project_id` and dual-read serves both selector
  forms.
- P2-E: full exit-gate live assertions of section 10 on the catalog-mode
  daemon plus the bridge parity harness.

## 13. Bookend protocol

### Before implementation

1. Finish this plan.
2. Start a fresh Kimi plan-review session via
   `scripts/kimi-review.sh plan-review` (the fixed lens already reads every
   `durable-project-catalog-phase*-impl.md`).
3. Treat every verdict other than exact `PASS` as `REVISE`; repair and
   `plan-resume` the same session with the fixed broad prompt.
4. Commit and push the clean plan milestone. No implementation begins
   before the exact `PASS`.

### After implementation

1. Finish all milestone commits, bootsmokes, and exact-ref cluster gates
   (P2-A through P2-E are committed and pushed separately, each cluster
   verified).
2. Start a fresh Kimi implementation-review session via
   `scripts/kimi-review.sh review`. The fixed scope remains
   `monolith-decomposition-pre-attempt-2..HEAD`, not Phase 2 files.
3. Repair every finding, rerun relevant local and live gates, commit and
   push, rerun exact-ref cluster verification, and resume the same review
   session until the final verdict is exactly `PASS`.

If Kimi is genuinely disrupted, the GLM 5.2 fallback of the Phase 1 plan
applies unchanged: same read-only limits, fixed baseline, complete diff, and
no-narrowing resumes in one persistent session.

Autonomous material decisions during implementation are recorded in
`DECISION_LEDGER.md` as D-029 onward, same envelope as existing entries.

## 14. Reviewer checklist

The plan reviewer must reject this plan unless it proves:

- Phase 2 is dependency-correct: the resolver contract lands before the v2
  runtime path, the runtime path before administration, store fields before
  the conversion that relies on dual-read, and no milestone smuggles a live
  cut of configured operator state;
- the mode fork cannot activate v2 semantics on configured state while apply
  refuses non-isolated roots, bridge startup remains byte-equivalent in
  behavior, and the absent-catalog-with-sibling-artifacts half-pair state
  refuses closed instead of minting a fresh v1 store;
- the catalog-mode daemon actually boots: every startup- and
  lifecycle-reachable consumer of `projects.json` bytes or `ProjectRegistry`
  has a defined injection seam, and no daemon-runtime path reads the v1
  store shape from disk;
- catalog-mode identity seeding covers the full catalog project set so
  remote-only collected state is never hidden by an attached-only gate,
  bounded to governing section 10.3's seeding rule and no more of Phase 3;
- one resolver engine serves both backends, version-1 semantics survive as
  the extracted v1 backend rather than a second implementation, and the
  Read/Write gate asymmetry is preserved;
- v1 data cannot forge v2 typed authority through the resolution outputs;
- the Selection/Filter taxonomy covers every surface in the section 9.2
  table, unknown/ambiguous Selection fails closed on v2, and the permanent
  literal-filter lane carries no identity or filesystem authority;
- every administration operation is proof-split per D-004, epoch-CAS'd,
  acknowledgement-disciplined, and rides the journaled pair transaction with
  fault-injected recovery to a complete old or new state including its
  migration record and proof pairing;
- promotion validates every sibling attachment, refuses owned scopes, and
  preserves ids, stores, and repo-history stability per governing sections
  5.1 and 7.2;
- publisher administration in this phase cannot create or mutate accepted
  generations, fabricate a pointer, or change the pointer's ref/commit
  binding, so the strict pointer/generation startup agreement holds across
  every Phase 2 operation;
- alias authority follows D-005: nominations never self-activate, acceptance
  is a local catalog action, accepted aliases survive config changes;
- retire/delete refuses while any reference class is nonzero and is
  CLI-only;
- store field additions are additive with dual-read, execution targets keep
  their paths, ledger appends are transactional with relocation, and the
  bridge parity exceptions are exactly the enumerated defect fixes;
- the exit-gate proof covers the governing Phase 2 exit text and names which
  section 17 matrix rows are discharged versus deferred;
- bootsmokes are stable-signed, isolated, and per-milestone; milestone
  commits and cluster gates are explicit; the implementation review is fresh
  and complete-baseline;
- no report, fixture, receipt, or response leaks host paths beyond the
  explicitly host-local operator surfaces, and nothing in the plan requires
  authenticated operator identity the transport does not have.
