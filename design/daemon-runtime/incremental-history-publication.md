---
title: "Incremental typed-history publication and activation-journal evolution"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [git-history, incremental-publication, activation-journal, force-push, edge-index, embedding, vector-gc]
brief: "Reconcile the typed Git-history publication design with per-commit cost: keep complete logical snapshots at the wire and P3 layers, publish commit-lane deltas derived corpus-side from two verified P3 manifests under a strict admission guard with a live-lane pre-probe, drive per-project edge work from resolved target-identity sets, give committed activations a sibling overlay-revision ledger with bounded settle semantics, and fix generation vector tombstoning for overlapping inventories."
---
# Incremental typed-history publication and activation-journal evolution
Date: 2026-08-23 (revision 4; adversarial review rounds 1-3 incorporated; round-3 verdict: ratify with amendments, both amendments applied below: the deferred rooted-aware tombstone outbox in IH-D5c and the typed `HistoryRecoveryRootV1` in IH-D5).
Baseline: branch `beta/blackbox-v2` at `e8e37c68`.
Amends: [`git-history-provenance-transport-impl.md`](git-history-provenance-transport-impl.md) (GH slice), the governing section 11 rules it certifies, and the P3-F vector tombstone rule; the complete amendment table is section 6.
Driving gap: `gap-a7d80bb2` (impact critical, blocks_class_of_work). Gating thread: `thread-78d7563a` (embedding re-enable is sequenced behind this design).

## 1. Problem and evidence

The typed-history publication design is correct about trust and recovery but
broken about cost: any genuine HEAD advance on a history-enabled repo
republishes that repository's ENTIRE consolidated `(repo_id, doc_type=commit)`
tantivy lane, rewrites every member project's materialized git edge sidecar,
and thereby forces full EdgeIndex rebuilds. One commit pays the whole-repo
worst case every time.

Production evidence (2026-08-08 through 2026-08-23):

- `rh_5f9fcb37` (transcript-search, 3,303 commits) re-committed activation
  continuously under active development; corpus doc count oscillated between
  604,168 and 607,693 as the lane was deleted and re-emitted.
- Every lane replacement re-enqueued a ~3,591-doc embed residue per 5-minute
  sweep; across repos the boot-time residue re-seeded to exactly 51,593 docs
  and fully re-drained each cycle. On voyage-code-3 pricing this dominated
  billing at roughly 50-60 USD/day.
- Edge sidecar churn kept the EdgeIndex watcher rebuilding a ~1.5M-edge graph
  near-continuously (rebuilds up to 375s), and the daemon heap climbed to its
  48Gi limit (5 OOMKills over the incident window).

The 2026-08 patches (snapshot vector reuse `e8e37c68`, outcome-level overlay
currency, code-source pre-upload currency probe `a3dbeb11`) removed the
SELF-AMPLIFICATION: no-op re-activation loops and runaway provider spend on
unchanged content. They did not touch the per-commit worst case. That cost is
why the transcript-search and pg-flare history lanes are paused and the embed
sweeper is off (thread-78d7563a); this design is the gate for turning them
back on.

**What this design delivers, stated precisely** (narrowed per round-2
finding 4): per-commit cost for the COMMIT LANE and its embedding enqueue,
elimination of no-op churn (selector-only drift touches nothing), and
edge-sidecar writes bounded to genuinely changed per-project edge sets. A
HEAD advance that activates new code snapshots for member projects still
recomputes those projects' complete touched-file edge sets and still
triggers a full EdgeIndex rebuild per changed sidecar batch: touched-file
edges bind snapshot-specific targets, and the EdgeIndex has no incremental
build. True per-commit EDGE cost requires incremental or content-addressed
edge segments, rebuild coalescing, or a stable file-target indirection, and
is explicitly follow-up work. The rollout therefore carries a sustained-
cadence soak gate (section 8) proving rebuild duration, heap, and backlog
stay bounded at realistic commit cadence.

## 2. The load-bearing rules being reconciled

Three certified rules interlock to force the worst case. Each was adopted for
a real reason; the proposal must preserve the reason while removing the cost.

**R1: whole-lane replacement.** `run_history_generation_publication`
(`crates/bbox-indexing/src/index/writer_actor.rs`) deletes the exact
`(repo_id, doc_type=commit)` lane before re-emitting the complete generation
inventory. Rationale (stated inline and in `bbox-indexing/CLAUDE.md`): a
force-pushed source can legitimately REMOVE commits, and entity-only upsert
would strand the removed docs forever. The rationale is about deletion
correctness, not about re-emitting unchanged docs.

**R2: complete snapshots, never cursor deltas.** Governing section 11
(`G11-B`) and GH-FD-4: every history generation is a complete self-contained
snapshot; cursor deltas were rejected because force-push, first upload,
replay, and GC would depend on UNTRUSTED PREDECESSOR STATE held by the
producer. The rejection is about trusting a producer's claim of "what
changed", not about the corpus deriving a delta from two snapshots it already
verified itself.

**R3: journal immutability.** `save_activation_journal`
(`crates/bbox-git-source-store/src/lib.rs`) refuses any write whose
`immutable_projection()` differs from the stored journal's. The projection
includes `code_selectors`, `overlays`, and `overlay_clears`, so ANY
overlay-selector drift on a committed journal is unexpressible: the only
legal path is a terminal journal followed by a fresh `Prepared` journal,
which re-runs the full activation transaction (GH plan sections 7.3 steps
1-11) including lane replacement. Rationale: recovery probes compare live
state to one immutable plan; a mutable plan makes crash recovery ambiguous.
The rationale is about each recovery probe having exactly one plan to check
against, not about the plan being frozen forever.

Three facts constrain any delta scheme (the first two were mis-modeled by
revision 1, the third surfaced in round 2):

- A physical commit document is NOT a pure function of the P3 generation
  row. `CommitDocumentOwnerV1`
  (`crates/bbox-corpus-index/src/index/schema_replacement.rs`) re-derives
  `project`, `project_id`, and `file_path` at re-emission time from the
  repo's member set (`display_member()` in `consolidated_history.rs`), and
  those owner fields are deliberately excluded from the generation
  commitment. A membership change can rewrite every physical doc while the
  generation manifest is byte-identical.
- `COMMIT_TOUCHED_FILE` edge targets are snapshot-specific `ProjectFileV2`
  identities (`current_chunk_targets_for_active_selector` in
  `crates/bbox-corpus-index/src/index/project_files.rs` requires the exact
  `(project, snapshot)` identity). A project's edge rows change whenever its
  selected code snapshot changes, even when no commit changed.
- Generation vector tombstoning is entity-id-only and inventory-driven:
  `tombstone_generation_vectors`
  (`crates/bbox-indexing/src/index/history_gc.rs`) deletes every entity id
  in the RETIRED generation's own vector-input inventory across all routes.
  Successive generations of one repo share almost their entire
  `commit:<namespace>:<sha>` inventory, so retiring a predecessor deletes
  the successor's live vectors. This is a LATENT defect today: whole-lane
  republication kept re-enqueueing everything, masking the hole at Voyage
  spend, and it is a candidate contributor to the incident's re-seeding
  residue. Under a delta design that stops re-enqueueing unchanged docs it
  would become a permanent silent coverage hole, so this design fixes it
  (IH-D5c) rather than inheriting it.

## 3. Goals and non-goals

Goals:

1. One commit appends its COMMIT-LANE delta: lane publication and embedding
   enqueue proportional to the change, not the repository.
2. Force-push remains fully correct, including commit REMOVAL, with an
   explicit recovery path instead of paying its worst case on every advance.
3. Overlay/code-selector drift on a committed activation updates selectors
   and only the genuinely affected per-project edge lanes, without touching
   the commit lane or the P3 generation.
4. Every recovery probe stays exact: probes compare live state to complete
   commitments and complete plans, never to deltas, and every crash boundary
   still has exactly one plan to check against.
5. Vector coverage survives generation GC: retiring a generation never
   deletes a vector a rooted generation still requires.

Non-goals:

- No producer-side delta or cursor protocol. The wire contract (GH-FD-4,
  probe/upload/finalize, complete HEAD-reachable manifest with
  content-deduplicated records) is unchanged.
- No change to P3 generation identity, the single-constructor rule (`G11-X`,
  P3-F), the rebuild manifest, or history GC ownership (GC gains one root
  KIND and a corrected tombstone rule, but no new owner).
- No change to the cutover marker's authority model, grants, or the
  producer trust boundary.
- No incremental EdgeIndex data structure and no per-commit EDGE cost claim
  (section 1); this design bounds how often sidecars change. Incremental
  edge segments, rebuild coalescing, or stable file-target indirection are
  named follow-up work.

## 4. Design

### IH-D1: complete logical snapshots, guarded delta physical publication

Keep `G11-B` and GH-FD-4 exactly: the producer uploads complete snapshots,
the P3 builder materializes complete generations with complete manifests and
commitments, and the activation journal binds complete commitments.

Change GH plan section 7.3 step 7. Publication runs in one of two explicit
modes, recorded in the journal core and surfaced in health as
`publication_mode`:

**Delta admission guard.** Delta mode is admitted only when ALL of the
following hold; any failure selects full replacement:

1. `repo_history_id` and primary namespace equal between base and planned
   (entity addressing domain),
2. Git object format equal (SHA-1 vs SHA-256),
3. the physical document encoder version equal (a new versioned field
   naming the commit-doc encoding; schema evolution bumps it),
4. the publication-owner projection equal: the exact emitted
   `CommitDocumentOwnerV1` tuple plus a derivation-algorithm version,
   `(project_id, project_display, owner_projection_version)` (round-2
   finding 8). Sibling membership that does not alter the emitted owner
   tuple does not force re-emission; membership and `bbox_root_relpath`
   remain independently bound for edge planning (IH-D3) and authority.
   Owner drift, including a display-member flip, is a whole-lane
   re-emission by construction because owner fields are outside the
   generation commitment,
5. **live-lane pre-probe** (round-2 finding 2): immediately before
   mutation, inside the writer operation, the live tantivy lane is proven
   equal to the base generation by canonical FULL-ROW digest (every
   physical field, owner-derived fields included), and each surviving
   entity's vector-input row (entity id, content hash) is proven equal
   between base and planned. A surviving entity whose vector-input hash
   differs refuses delta (it means encoder or content drift the commit oid
   cannot explain).

The owner projection and encoder version are bound in the journal core, and
post-publication verification re-derives and checks the owner fields in
addition to the existing complete-row comparison (recovery compares complete
stored rows to the retained generation, per GH plan section 7.4 step 3; this
proposal does not weaken that).

**Delta mode.** The writer actor computes, corpus-side:

```text
base    = commit inventory of the delta base generation, keyed by FULL
          entity id `commit:<namespace>:<sha>` with canonical full-row digest
planned = same projection of the newly prepared P3 generation
adds    = planned - base
removes = base - planned
```

The diff key is the entity id; equality of intersecting rows is by canonical
full-row digest, not message content hash alone (round-2 finding 2). There
is no `updates` lane: an intersecting key with a differing digest is
impossible for genuine Git history (an oid binds header, tree, and parents),
so observing one means encoder drift, owner reprojection outside the guard,
or corruption, and it REFUSES delta mode into full replacement with a
diagnostic (round-1 finding 9).

Publication deletes exactly the `removes` docs by entity-id term, emits
exactly the `adds`, journals the corresponding vector actions (IH-D5c),
and verifies the resulting lane against the COMPLETE planned generation.
The delta is verified or replaced WITHIN the writer operation: `post_commit`
does not run and no new searcher is exposed until verification passes or
the fallback full replacement has been committed in its place (round-2
finding 2). Truncation invariant, stated and tested explicitly: the vector
input row is constructed from the same indexed (possibly truncated) content
the document carries, so a doc unchanged by digest implies its vector input
is unchanged.

**Full-replacement mode** is the current behavior, retained for: first
publication, any guard failure, delta verification failure, and recovery
with an unverifiable base. A cross-namespace replacement (primary-namespace
change with identical oids) additionally deletes the OLD namespace's
`(repo_id, doc_type=commit)` lane after proving unique namespace ownership,
so no old-address residue survives (round-1 finding 4).

Both diff inputs are corpus-verified immutable P3 manifests plus the
journal-bound owner projection, and the live lane is probed against the
base before mutation. This is what dissolves the GH-FD-4 objection: no
untrusted predecessor state is consulted, because the predecessor is our own
retained generation whose integrity and live presence are proven before the
first write.

Force-push needs no ancestry heuristic: removed commits appear in `removes`
by set difference and are deleted exactly (documents AND vectors, IH-D5c).
The explicit force-push recovery path IS the fallback-to-full arm plus the
`removes` lane; health reports `publication_mode` (`delta` |
`full_replacement`) with add/remove counts and the guard clause that forced
full mode, so a force-push or owner flip is visible in doctor output rather
than silent.

**Delta base.** Exactly one delta base exists per repo: the prior Committed
journal's retained P3 generation, which the live lane's pre-probe verifies
directly. An arbitrary retained ancestor is NOT a valid base, because the
delta is a statement about the live lane's current contents (round-1
finding 10). If the base is absent, superseded, quarantined, or fails
either probe, full replacement runs.

### IH-D2: stable identity within a publication domain (amend G11-C)

Within an unchanged addressing/encoding/owner domain, commit documents keep
their `commit:<namespace>:<sha>` address across generations; a doc absent
from `adds`/`removes` is physically untouched, so its tantivy doc, its
vector coverage row, and every edge targeting it remain valid. `G11-C` is
narrowed FOR COMMIT TARGETS: edges cannot target removed commits (dropped
with the `removes` set); edges to surviving commits survive. `G11-C` is
fully preserved for FILE targets: touched-file edges bind snapshot-specific
`ProjectFileV2` identities and are re-resolved whenever the selected code
snapshot changes (IH-D3).

Embedding claim, stated narrowly (round-1 finding 8): this design reduces
ACTIVATION-TIME embed enqueue to the `adds` set (today the writer re-emits
`emit_git_message` for every vector input of the generation). It does NOT by
itself explain or fix the boot-time residue: the restart sweeper
independently scans current docs against durable vector coverage, and the
incident evidence (a 51,593-doc cohort re-seeding on every boot) is
consistent with a coverage-persistence defect that lane churn amplified but
did not cause. The IH-D5c tombstone fix removes one candidate cause
(predecessor GC deleting live coverage). Durable coverage persistence across
two restarts is a HARD gate in the rollout (section 8): if the cohort
re-seeds after this design lands, the remaining persistence defect is found
and fixed BEFORE the sweeper is re-enabled, not merely filed.

### IH-D3: resolved-target-driven per-project edge publication

Today section 7.3 step 7 rewrites every member project's "git" materialized
edge lane on every activation. The skip rule must be driven by resolved
TARGET IDENTITY, not by commit-delta emptiness: a project's
`COMMIT_TOUCHED_FILE` rows change when its selected code snapshot changes
even with zero commit changes (round-1 finding 1).

Per selected project, the plan computes the complete desired edge set:
parent edges from the commit graph, touched-file edges resolved against that
project's ACTIVE snapshot targets. The per-project action is journaled
before any write (round-1 finding 6):

```text
ProjectEdgeActionV1 = Reuse   { existing_receipt_digest, edge_set_sha256 }
                    | Publish { target_snapshot_id, edge_set_sha256,
                                expected_receipt_digest }
                    | Clear   { }
```

- `Reuse`: the desired edge set and target snapshot are byte-identical to
  the durable current lane, proven by reading the DURABLE receipt digest
  and edge commitment at plan, recovery, and acceptance time (round-2
  finding 7; the process-local `activation_was_validated` cache is
  acceptable only with a key extended to core checksum, revision checksum,
  target snapshot, and receipt-manifest generation, and only within the
  operation that performed the durable read). No sidecar write, no receipt
  churn, no watcher wakeup.
- `Publish`: anything else: commit delta touching this project's paths, a
  snapshot/selector change, membership or `bbox_root_relpath` change. The
  COMPLETE edge set for that project is recomputed and its exact snapshot
  receipt published, exactly as today.
- `Clear`: the project leaves selection.

The writer-actor invariant is restated: the projects with staged edge writes
and the projects with newly finalized receipts are exactly the `Publish`
set; `Reuse` projects are proven by their existing durable receipt digests.
Recovery probes ALL planned projects in journal order: a `Reuse` row
verifies the pre-existing receipt digest still matches; a `Publish` row
verifies the new receipt and edge commitment; ambiguity between
"intentionally skipped" and "never completed" cannot arise because the plan
names each project's disposition before the first write (GH plan section
7.4 step 4 extends to this per-project action list).

Honest cost statement (round-2 finding 4): a HEAD advance that activates
new member snapshots makes those members `Publish`, recomputing their full
historical touched-file edge sets and triggering EdgeIndex rebuilds. The
win over today is the elimination of no-op churn (selector drift, unchanged
projects) and receipt/sidecar writes for unchanged lanes, not per-commit
edge cost. The sustained-cadence soak in section 8 is the gate proving this
is enough for current corpus scale; the follow-up work in section 3
non-goals is the path if it is not.

### IH-D4: sibling overlay-revision ledger (amend R3)

The journal file, its byte format, its `immutable_projection()` refusal, and
its terminal-stage rules are UNCHANGED (round-1 findings 3 and 11 rejected
the revision-1 embedded-V2 approach). Selector evolution moves to a sibling
append-only ledger per repo-history id:

```text
OverlayRevisionV1 {
  version, repo_history_id,
  core_checksum_sha256,          # the Committed journal this revises
  revision_ordinal,              # monotonic from 1
  predecessor_checksum_sha256,   # revision N-1's checksum (or the core's,
                                 # for revision 1): atomic CAS on append
  code_selectors, overlays, overlay_clears,
  project_edge_actions,          # IH-D3 action list for this revision
  stage,                         # Planned | Committed | Superseded
  checksum_sha256
}
```

Contract:

1. Append is an atomic compare-and-swap on `predecessor_checksum_sha256`
   under the store mutation lock: two racing planners cannot both attach
   revision N+1, so exactly one plan exists per manifest state.
2. The manifest selector swap executes inside the manifest coordinator with
   an expected-prior-selector comparison: the transaction carries the
   selector state revision N committed, and refuses if live state differs.
3. A `Planned` revision is NONTERMINAL for startup and worker discovery:
   recovery enumerates journals AND ledgers with the same
   one-plan-per-probe discipline as journal checkpoints. The GH plan
   section 7.9 `zero-prepared-journal` cutover proof extends to "zero
   nonterminal cores AND zero `Planned` revisions".
4. **Bounded settle** (round-2 finding 3): "settling" a `Planned` revision
   is a deterministic classification, never an open-ended retry. If live
   selectors equal the planned state: commit action-ahead. If they equal
   the predecessor state and the plan is no longer current (selector inputs
   moved, grant changed): supersede with a diagnostic. If they equal
   neither: clear transport exposure for the repo, supersede with a
   diagnostic, and proceed. Arrival of a newer `current-ready` source
   generation is an EXPLICIT supersession trigger for a `Planned` revision
   whose plan predates it, and a bounded retry count/age applies to
   transient failures, after which the same classification runs. A wedged
   revision therefore cannot block HEAD advancement indefinitely: a new
   source generation may replace a terminal core with `Prepared` only after
   the live revision is `Committed` or `Superseded`, and the classification
   above guarantees one of those outcomes in bounded steps. The fresh
   journal core records the settled ledger tip checksum so recovery can
   order the two artifacts unambiguously.
5. The journal core binds selector-INDEPENDENT inputs: the delta base, owner
   projection, encoder version, commitment set, and the changed-path input
   commitment. The resolved per-project edge-set and receipt commitments
   live in the revision, because they depend on the active selector and
   snapshot-specific target identities (round-1 finding 12).

`activate_source` ordering becomes:

1. Committed core, current revision durable and selector-current: no-op.
2. Committed core durable, selector outcome drifted: plan and CAS-append an
   overlay revision, run its manifest transaction, commit it. No lane work,
   no P3 work. This replaces today's `clear_transport_overlays_for_repo`
   plus full re-activation at `src/server/history_activation.rs:287`.
3. New source generation (HEAD advance): settle the live revision per rule
   4, then fresh journal whose publication step runs IH-D1.
4. Core not durable (probe mismatch): full replacement recovery.

### IH-D5: GC roots for the delta base (stage-dependent)

"One extra retained generation" requires a new DURABLE root kind; the
existing `HistoryReferenceKindV1` vocabulary (catalog record, quarantine
record, active overlay, rebuild manifest, plus the two process-local kinds)
does not name activation journals, and `build_reference_manifest` does not
read them (round-1 finding 5). Add persisted
`HistoryReferenceKindV1::DeltaBase` with STAGE-DEPENDENT derivation
(round-2 finding 5; the journal's `prior_p3_generation_id` is immutable, so
"release on commit" must be a rule about how roots are DERIVED, not a field
rewrite):

| Journal core stage | DeltaBase root contributed |
|---|---|
| Nonterminal (`Prepared` .. `OverlaysPublished`) | Its `prior_p3_generation_id`: materialization may still move past the old lane, and fallback needs the base. |
| `Committed` | None. The planned generation is already rooted by the catalog record and IS the next delta base; the prior generation is released. |
| `Superseded` | None from the journal itself. The exceptional last-good root comes from a TYPED sibling artifact, `HistoryRecoveryRootV1` (`repo_history_id`, journal checksum, generation id, complete lane commitment, reason, creation checkpoint), written ONLY by the writer actor immediately after an exact lane verification. Supersession preserves it; the next Committed activation atomically clears it. The GC rebuild consumes this artifact and never parses journal diagnostic prose (round-3 finding 2: `diagnostic` is a free-form truncated error string with no authority). |

The reference-manifest rebuild reads each repo's activation journal, applies
this table, and roots accordingly. Only a verified, owned (non-quarantined,
non-superseded) generation qualifies as a base. A base GC'd anyway
(corruption, manual purge) fails the IH-D1 pre-probe and selects full
replacement. This table is normative for both `build_reference_manifest`
and the GC tests.

### IH-D5b: recovery matrix deltas

Section 7.4's probe discipline is preserved: every probe compares live state
to a COMPLETE commitment or a COMPLETE per-project plan. New/changed arms:

| Crash point | Recovery |
|---|---|
| Delta computed, partially applied, before in-writer verify | Nothing was exposed (`post_commit` gated on verify); recovery's lane row-comparison probe mismatches; recompute delta against the base; if the base pre-probe fails, full replacement. Delta application is idempotent (entity-id-addressed deletes plus re-adds). |
| After lane verify, before some `Publish` project completes | Lane probe passes; the journaled per-project action list names each project's disposition; re-run only unfinished `Publish` rows, verify `Reuse` rows' durable digests. |
| Overlay revision `Planned`, manifest transaction not run | Revision N+1 is the single plan; run the bounded-settle classification (IH-D4 rule 4). |
| Revision `Planned`, new source generation arrives | Explicit supersession trigger; classification settles the revision, then the new core proceeds. |
| Prior generation absent/GC'd before next delta | Base pre-probe fails; full replacement. `DeltaBase` root prevents this in normal operation. |
| Owner projection drifted between plan and publication | Journal-bound owner hash mismatches at the pre-publication recheck; supersede and re-plan (full replacement, since the guard now fails). |
| P6-R full path-free rebuild ran since the last activation | Delta admission is INVALIDATED (round-2 finding 6): the rebuild re-emits under current encoder/owner derivation without updating any journal core, so the committed core's baseline metadata no longer describes the lane's provenance. The next activation runs full replacement, whose Committed journal re-establishes the base. Rebuilds are rare; a baseline-receipt alternative (P6-R writing a `HistoryLaneBaselineV1`) was considered and rejected as avoidable surface area. |

### IH-D5c: generation vector tombstoning for overlapping inventories

Round-2 blocker: `tombstone_generation_vectors` deletes the retired
generation's ENTIRE deduplicated entity-id inventory across all routes.
With stable entity ids, successive generations share almost their whole
inventory, so retiring G0 after G1 commits deletes G1's live vectors. This
is latent today and becomes permanent under delta publication. Amend P3-F
tombstoning now, as part of this design:

1. Retiring a generation tombstones only vector keys absent from every
   ROOTED generation: the sweep subtracts the union of vector-input entity
   ids of all generations named by the current reference manifest before
   issuing deletions. (Entity-id subtraction suffices; conditional
   `(entity_id, content_hash)` deletion is not required because an entity id
   maps to one live content hash per the IH-D1 truncation invariant.)
2. IH-D1 delta publication journals explicit vector actions as a
   replayable, idempotent tombstone OUTBOX (round-3 finding 1): `adds`
   enqueue embeddings (as today via `emit_git_message`); `removes` become
   pending tombstone actions executed only AFTER the lexical delta has
   committed and verified, and only after subtracting the current complete
   rooted-generation entity-id union at execution time. An id still rooted
   by a pinned old read view, a nonterminal `DeltaBase`, or a prepared
   rebuild manifest defers until root release; generation GC's subtraction
   sweep is the backstop that eventually drains deferred actions. No
   tombstone ever runs before `post_commit` of the lane that removed the
   commit.
3. The two-restart coverage-persistence gate (section 8) runs WITH a GC
   pass in between, proving the subtraction rule holds under real retention.

## 5. What this deliberately does NOT relax

- Producer trust boundary: producers still cannot express "what changed";
  they upload complete snapshots and the corpus derives everything.
- Commitment-exact recovery: no probe ever trusts a delta record; deltas are
  a publication strategy, commitments and complete plans remain the truth.
- Single P3 constructor, single rebuild manifest, GC ownership (one new root
  KIND and a corrected tombstone subtraction, same owner), cutover marker
  authority, grant model: untouched.
- The journal store contract (`save_activation_journal`, immutable
  projection, terminal rules): byte-untouched; evolution lives in the
  sibling ledger.
- Whole-lane replacement: retained, reachable, and tested; it is the
  recovery arm, the first-publication arm, the post-P6-R arm, and the arm
  for every guard failure (owner drift, encoder bump, namespace or
  object-format change, live-lane pre-probe mismatch).
- `G11-C` for file targets: snapshot-specific touched-file edges are always
  re-resolved when the selected snapshot changes.

## 6. Amendment map (exhaustive)

Per the certified amendment mechanic (GH-FD-6 precedent), one commit lands
the code and surgically amends every certified statement below, recording a
Decision Ledger entry whose number is assigned at implementation time.

| Certified statement | Treatment |
|---|---|
| Governing section 11 `G11-B` (complete snapshots) | Unchanged. |
| Governing section 11 `G11-C` (edge targets) | Narrowed for commit targets within an unchanged publication domain; preserved for snapshot-specific file targets (IH-D2). |
| Governing section 11 / P3-F `G11-X` (single constructor) | Unchanged. |
| P3-F vector tombstone rule | Amended: rooted-inventory subtraction before deletion; delta-time removal tombstones (IH-D5c). |
| GH plan GH-FD-4 rejection note | Scoped: producer-side cursor deltas remain rejected; corpus-side two-snapshot diffs under the IH-D1 guard are admitted. |
| GH plan section 7.3 step 7 | Replaced by IH-D1 modes plus IH-D3 per-project actions; searcher exposure gated on in-writer verification. |
| GH plan section 7.3 steps 9-11 | Gain the revision-ledger path for selector-only evolution (IH-D4). |
| GH plan section 7.4 (recovery) | Extended per IH-D5b; step 4's receipt probe becomes the per-project action-list probe over ALL planned projects. |
| GH plan section 7.9 zero-prepared-journal proof | Becomes zero nonterminal cores and zero `Planned` revisions. |
| GH plan section 7.10 observation taxonomy | Currency predicate evaluation while a revision is live: a `Planned` revision within its bounded settle window is not currency loss; classification text amended accordingly. |
| GH plan GH-C mechanics and verification rows | Activation ordering gains the revision path and bounded settle; verification matrix gains delta/owner-flip/namespace-change/revision-crash/wedged-revision rows (section 7 here). |
| GH plan section 9 health vocabulary | Adds `publication_mode`, guard-clause diagnostics, revision ordinal, and settle-classification outcomes to history health detail. |
| GH plan section 10 bridge parity (closed 17-entry list) | Gains entries for delta publication mode, revision ledger, per-project reuse, and delta-time vector tombstones; per that section's own rule this requires plan amendment and re-review, provided by this design's review rounds plus the section 9 ceremony. |
| GH plan section 11.4 fault matrix | Gains the IH-D5b crash points. |
| GH plan section 12 exit gate | Gains delta-vs-full equivalence, revision-crash, wedged-revision settle, and post-P6-R full-replacement steps. |
| Phase 6 `P6-R` (no parallel rebuild manifest) | Unchanged as a manifest rule; a completed P6-R rebuild invalidates delta admission for the next activation (IH-D5b). |
| `bbox-indexing/CLAUDE.md` and writer-actor inline invariants | Amended: "publication proves the whole-lane commitment; it may reach it by guarded, pre-probed, in-writer-verified delta, must reach it by full replacement otherwise"; edge/receipt equality restated over the `Publish` set; delete-by-`repo_id`-alone remains forbidden. |
| `bbox-git-source-store` journal contract | Unchanged bytes; new sibling ledger store (versioned schema, CAS append, bounded-settle classification) and new sibling `HistoryRecoveryRootV1` artifact (writer-actor-authored, GC-consumed), each with its own tests. |
| `HistoryReferenceKindV1` | Gains persisted `DeltaBase` with the stage-dependent derivation table (IH-D5). |

## 7. Verification plan

1. **Delta-equals-full golden:** for linear, merge, root, rename, delete,
   force-push (removal and rewrite), SHA-1/SHA-256, and monorepo fan-out
   fixtures: publish via full replacement and via delta from every
   predecessor state; assert byte-identical lane, edge sidecars, receipts,
   vectors, and commitments, including the owner-derived fields.
2. **Guard matrix:** owner flip via display-member change (and prove an
   unrelated sibling add that does NOT change the emitted owner tuple stays
   in delta mode); encoder-version bump; primary-namespace change at
   identical oids (assert old-namespace lane purge); object-format change;
   live-lane pre-probe mismatch (stale surviving row); surviving entity
   with drifted vector-input hash; each must select full replacement with
   the guard clause named in health.
3. **Force-push matrix:** removal-only, rewrite-only, mixed; assert
   `removes` deletion of docs AND vectors, commit-target edge drop,
   `publication_mode=delta` visibility, and fallback-to-full on a poisoned
   base manifest.
4. **Revision ledger:** CAS race (two planners, one winner); every
   `Planned`/`Committed` crash boundary; bounded-settle classification for
   all three live-selector states; wedged revision (corrupt receipt,
   revoked grant, repeated CAS refusal) superseded within bounds and a new
   core admitted; newer-source explicit supersession; V0 absence (repos
   with no ledger behave exactly as today); cutover preflight refusing a
   `Planned` revision.
5. **Per-project actions:** `Reuse` durable-digest verification at plan,
   recovery, and acceptance, `Publish` receipt parity, `Clear`, and the
   crash between two `Publish` projects; snapshot change with empty commit
   delta MUST produce `Publish` for that project.
6. **GC and tombstones:** `DeltaBase` stage-dependent derivation table
   (nonterminal roots prior, Committed releases, Superseded only via a
   writer-actor-authored `HistoryRecoveryRootV1`, cleared by the next
   Committed activation); rooted-inventory subtraction: retire G0 with G1
   rooted and prove zero live-vector deletions; delta-time removal outbox:
   G0 pinned by an old read view while G1 force-pushes commit C away, and
   C's tombstone defers until the pin releases; outbox replay idempotence
   across a crash; quarantined/superseded generations refused as base.
7. **Churn regression:** replay the incident shape (repeated activations
   with selector-only drift, then a one-commit advance) and assert zero
   lane writes for drift, one bounded delta for the advance, EdgeIndex
   rebuild count equal to real change count, and activation-time embed
   enqueue equal to new-commit count.
8. **Sustained-cadence soak (rollout gate):** realistic commit cadence
   against a corpus-scale fixture (multi-repo, ~1.5M edges), measuring
   EdgeIndex rebuild duration, heap, and backlog; the gate is bounded
   backlog and flat heap when cadence exceeds single-rebuild duration
   (round-2 finding 4).
9. **Coverage persistence (hard gate):** two-restart matrix WITH an
   intervening GC pass proving the previously re-seeding residue cohort
   stays durably covered across boots with the sweeper on in an isolated
   daemon. A red here blocks rollout step 3 until the remaining
   persistence defect is root-caused and fixed.
10. Existing GH-C/GH-G matrices (marker orders, grant loss, code-ahead
    oscillation, detach independence, GC retention) rerun unchanged.

## 8. Rollout ordering (binds thread-78d7563a)

1. This design ratified and implemented; verification items 1-7 green in an
   isolated daemon.
2. Soak and coverage gates (items 8 and 9) green. If coverage is red:
   root-cause and fix the persistence defect first; it is a prerequisite,
   not a parallel gap.
3. voyage-code-4 route migration per the thread's steps (vendor spend cap,
   dims from the routing design, embed config converge).
4. Sweeper re-enable (remove `BLACKBOX_EMBED_SWEEP_INTERVAL_SECS=0` from
   bbox-cage index.ts, converge); first sweep is the one-time family
   migration.
5. Unpause transcript-search and pg-flare `git_history`/`provenance`
   collector lanes; verify two passes with no re-activation churn and
   delta-sized publications.
6. Prune the orphaned voyage-code-3 partition once nothing maps to it.

## 9. Resolved decisions

All open questions from revisions 1 and 2 are resolved through the two
review rounds:

1. No `updates` lane; same-key digest drift refuses delta into versioned
   full replacement.
2. Exactly one delta base: the prior Committed journal's generation,
   verified against the live lane by pre-probe.
3. Sibling append-only revision ledger, not an embedded journal V2; the
   journal store contract stays byte-identical.
4. Per-project edge/receipt commitments live in the overlay revision; the
   core binds only selector-independent inputs.
5. `Reuse` verification reads the durable receipt digest at plan, recovery,
   and acceptance; process-local caching only with the extended key and
   within the operation that performed the durable read.
6. Owner projection is the exact emitted owner tuple plus derivation
   version: `(project_id, project_display, owner_projection_version)`;
   unselected sibling membership does not force re-emission.
7. The amendment re-review is a scoped surface with full GH certification
   rigor: the existing phase-family reviewers and gates over GH-C, sections
   7.3/7.4/7.9/7.10, section 9 health, section 10 parity, P3-F
   GC/tombstones, P6-R interaction, GH-F/G cutover proofs, and the exit
   matrix; unrelated GH-A/B/D/E milestones are not reopened.
