# bbox-corpus-core — corpus contract bottom (refs, records, ranking math)

Pure types and leaf logic the whole corpus stack links. This crate depends on
no other bbox crate; keep it that way — things live HERE precisely so crates
below bbox-indexing (ingest passes, sidecars) can call them.

## Project identity

- Version-1 `ProjectRecord.project_id` is a HOST-SCOPED realpath hash. The v2
  catalog `ProjectId` is a path-free opaque logical id: migration preserves
  accepted legacy ids, while new records mint strong-random `p_` ids. Never
  infer either meaning from string shape. Exact catalog membership determines
  whether a parsed selector is a v2 project id.
- Catalog snapshots contain no host path or attachment identity. Host-local
  paths, checkout ids, and capabilities live only in the separately validated
  attachment snapshot. A path-bearing `ProjectRecord` is a temporary v1 view
  that can be constructed only from a catalog project plus a cross-validated
  active attachment.
- Published wire authority is the typed `(repo authority,
  bbox_root_relpath)` scope. Bootstrap hints, commit namespaces, recorded
  authority, and logical project ids remain distinct even when legacy bytes
  happen to match.
- `resolve_base_project_for_scope` lives here (not bbox-indexing) so index
  ingest can stamp docs with it. It is the BROAD read gate: descendants and
  ANY worktree of a registered repo alias to the base. The conservative
  managed write gate stays in bbox-indexing. **Do not unify the two gates** —
  the asymmetry is the product: reads may alias an arbitrary user worktree
  harmlessly; writes into one would scatter repo-owned state.
- `ProjectContext.checkout` is present only when the input sat in a
  non-base checkout; `managed = false` checkouts must never receive
  write-side aliasing.
- A full independent clone can opt into managed checkout resolution with
  the exact `.git/blackbox-managed-checkout` marker. The marker only opens
  the gate: the clone's durable `repo_id` must still uniquely match a
  registered base project. Lane tooling owns marker creation; arbitrary
  unmarked clones remain write-isolated.
- The canonical JSON store lock is a stable, no-follow inode. Never truncate
  or replace `<store>.json.lock`: bridge writers and strict catalog
  transactions must contend on that exact advisory lock.
- Host-local lanes that can be reached through mutable checkout paths use
  `NofollowDirectory`: open or create every component without following links,
  hold and lock the directory descriptor, perform leaf I/O relative to that
  descriptor, then verify the path still names the held inode before publish.
- Project-catalog owner snapshots are raw, read-only migration inputs. Owner
  crates decode their own formats and return the non-serializable snapshot
  contract from `project_catalog_snapshot`; bbox-indexing maps it into its
  durable inventory types. Literal selectors never leave the host-local
  snapshot. The task and consultant proposal projections live here only to
  break the root-crate dependency cycle and expose their narrow persisted
  selector surface.
- Owner capture has TWO lanes and they are not interchangeable. The buffered
  lane reads each source whole and spends `max_source_bytes` cumulatively
  across the tree; that is a MEMORY ceiling and is right for the small JSON
  stores, whose fingerprints are over bytes they must hold anyway. The
  streaming lane digests incrementally and hands the owner one line at a
  time, bounded by `max_streamed_source_bytes` (wall time) and
  `max_streamed_line_bytes` (the only real allocation bound there). A
  line-oriented owner whose sources are unbounded by design (edge lanes run
  to gigabytes on a working host) MUST use the streaming lane: under the
  buffered one its first file exhausts the budget, reads back empty, and the
  owner reports a healthy host as `owner_source_unreadable`. Never "fix" that
  by raising the buffered budget, and never move a buffered owner onto the
  streaming lane casually: the streamed digest is byte-identical to the
  buffered one, but the lanes differ in what they refuse.
- A legacy-selector observation names a SET of owner rows, not necessarily one.
  Small stores emit singletons (`legacy_selector`); an owner whose rows
  outnumber what a canonical inventory can hold emits one observation per
  (subsource, selector) with a count and an ordered member commitment
  (`legacy_selector_aggregate` + `LegacySelectorMembersBuilderV1`, folded
  incrementally so the ids are never held). The commitment is over WALK order,
  not sorted order, so re-deriving it proves a verify re-read the same rows
  rather than merely the same set, and it is committed into the row hash so an
  aggregate whose membership moved cannot present unchanged evidence. An
  observation with zero members is refused: it would plan an obligation with
  nothing to apply.
- The member evidence is worth nothing unless it is REDERIVED at the moment of
  writing and again at verification, so it travels all the way into the durable
  ledger and back out through the stamp and read seams, and both refuse with
  `owner_row_members_moved` (a staleness-class code, sibling of
  `owner_source_moved`) when the group they just walked is not the group the
  plan reviewed. Singleton owners get the same check for free through
  `ensure_singleton_member_evidence`: their member set is a pure function of the
  row id. Never add an owner seam that accepts a row id without its evidence -
  a group whose rows changed while staying uniformly stamped is otherwise
  indistinguishable from the reviewed one.
- The durable attachment snapshot is a store real hosts already have, so any
  field added to `LegacyPathLedgerEntry` needs a read path for bytes written
  before it existed: a strict decode turns an additive field into an unopenable
  store, before a migration or backfill can run at all. The member evidence
  reads absence as the singleton it always implied, and normalizes ONLY when
  both halves are missing - a partial or zeroed pair is refused rather than
  laundered into valid-looking evidence, and what we write always emits both
  fields so the next read never has to guess.
- Reconstruction is gated by an ALLOW-LIST of ledger sources
  (`legacy_ledger_evidence_is_reconstructable`), never a deny-list. It may only
  run where the answer is knowable without asking the owner, which excludes any
  GROUP-shaped source: deriving a singleton for one would write evidence no
  refold can reproduce, so every later stamp refuses forever and no fresh
  preflight can repair an already-durable record. Those refuse at decode with
  `error.project_catalog_legacy_evidence_unreconstructable`, whose remedy is
  remigration from the v1 predecessor. A source added later is absent from the
  list and therefore refuses rather than being guessed at; a test in
  bbox-indexing walks the owner set and pins the two lists together.
- `GitOverlaySelector.source` is a closed authority discriminant:
  `Attachment { attachment_id }` or `ProducerTransport { producer_id,
  source_generation_id }`. Producer transport never uses an attachment
  sentinel. The custom reader alone accepts the legacy flat `attachment_id`
  form, only when `source` is absent; new serialization is always typed and
  both/neither forms fail closed.
- A provenance collector snapshot resolves the moving notes ref exactly once,
  then lists and reads notes through that immutable tree. The returned notes
  tip is part of the transport generation commitment; never list by one tip
  and read note bytes through the moving ref again.

## Rerank math (search/rerank.rs)

- `DEFAULT_COMBINED_CAP = 1.75` is EMPIRICAL (gap-39b3ce16 sweep), not a
  vibe: the maximum legitimate boost product today is UserConfirmed 1.35 ×
  temporal clamp 1.25 = 1.6875, so any cap ≥ that never binds, and the old
  1.5 throttled real knowledge promotions (MRR 0.158 → 0.175 by lifting it).
  **If you add or rescale any boost signal, re-derive the max product and
  re-sweep** — protocol in search/metrics.rs header, `rerank_cap` operator
  probe on bbox_hybrid_search.
- Boost formulas need a value-at-zero check: `recall_boost` shipped as
  `ln_1p(1 + count)` — ≥ ln 2 at count 0, so the 0.25 cap saturated for every
  entry and the recall-frequency signal was silently dead. Zero input must
  produce zero boost, graded from there.
- search/metrics.rs (MRR, recall@k) exists because eval/check.rs measures
  only oracle coverage (is the ref present at all). Anything rank-shaped —
  cap sweeps, signal tuning — must be scored with these, not the oracle.
