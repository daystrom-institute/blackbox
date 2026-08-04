---
title: "Durable project catalog Phase 6 handoff inventory"
kind: design
lifecycle: partial
corpus: blackbox-design
topic:
  - daemon-runtime
  - corpus
tags: [decomposition, project-identity, phase6, deletion-inventory, bridge-cut]
brief: "What Phase 6 deletes, what it must keep until last, and the machine-checked inventory that tracks both."
---
# Durable project catalog Phase 6 handoff inventory
Date: 2026-08-04
Governing design: [`durable-project-catalog-impl.md`](durable-project-catalog-impl.md).
Produced by: Phase 5 milestone P5-H, per [`durable-project-catalog-phase5-impl.md`](durable-project-catalog-phase5-impl.md) section 16.

## 1. What this document is
Phase 5 converted every checkout-touching adapter to capability leases and
made verified accepted publication the path-free catalog authority. It did
NOT delete the version-1 bridge; [D-002](../../DECISION_LEDGER.md#d-002)
keeps the production authority switch prohibited, so both authorities still
ship.

This is the inventory Phase 6 consumes: what remains, why each thing is
still here, and the order the cut has to happen in. It is deliberately
paired with a machine-checked counterpart so it cannot rot into prose that
disagrees with the tree.

## 2. The machine-checked half
`scripts/catalog-ownership-baseline.txt` is a per-SITE evidence inventory:
one row per pattern, file, and enclosing item, with its occurrence count
and its Phase 6 deletion or retention reason.
`scripts/acceptance-catalog-ownership.sh` fails when a site grows, when a
site is absent without the baseline being refreshed, and when a site
carries no stated reason. It runs blocking from
`catalog_ownership_ratchet_holds`.

Per-site is what a per-pattern TOTAL could not do: a total stays flat when
a prohibited occurrence is substituted for an approved one, so counting
alone accepted the exact move the proof exists to reject.

Baseline at the Phase 5 exit gate:

| Pattern | Sites | Occurrences | Phase 6 disposition |
|---|---|---|---|
| `project_record_import` | 19 | 19 | Delete with the v1 record type. The sanctioned compatibility projection (`catalog_records.rs`) goes last, because it is what lets the catalog serve v1-shaped consumers during the cut. |
| `canonical_path_read` | 54 | 89 | Delete with `ProjectRecord`. Every one is a bridge arm; the catalog arm beside it already resolves through attachment identity. |
| `checkout_root_path` | 51 | 80 | Delete with the v1 path lane. A catalog-mode path reaches a checkout only through a capability lease. |
| `direct_git_process` | 15 | 17 | Delete or route through lease-held authority. A direct Git process against a checkout root is an unleased open by definition. |
| `legacy_publisher` | 24 | 31 | Delete outright. `PublisherRefStore`, `elect_publisher`, and `PublisherAuthorizationCache` have no catalog-mode caller. |
| `watcher_selected_carrier` | 4 | 4 | Delete. Catalog registrations are `ArtifactWatchAttachment::AttachmentId`; `Selected` is bridge-only. |
| `repo_io_selected_target` | 4 | 7 | Delete the `Selected` and `Checkout` variants of `RepoCarrierTarget`, leaving `Attachment` as the only target. |

A shrinking inventory is the Phase 6 progress metric. When every row is
gone except the compatibility projection, the bridge is cut.

**Baseline diffs are review artifacts.** Regeneration is a deliberate act,
and any baseline diff in a change is read row by row, exactly like an
allowlist change. `--write-baseline` preserves reasons by key, so a
regeneration that nobody read can carry a NEW prohibited site into the
baseline wearing an old row's reason, and the check cannot detect that by
construction: it compares the tree against the baseline, and the
regeneration just moved the baseline. The reviewer is the only thing
standing between a laundered site and the inventory. Treat a diff touching
rows the change did not intend to touch as a finding, not as noise.

### 2.1 The other two machine-checked inputs
The ratchet counts occurrences. Two further inputs check things a count
cannot see, and both are blocking from `src/server/state.rs`:

| Input | Checked by | What it catches that a count cannot |
|---|---|---|
| `scripts/checkout-callsite-audit.tsv` | `acceptance-checkout-callsites.sh`, blocking from `checkout_callsite_audit_is_complete` | An unclassified checkout-open call site. A count stays flat when a site is replaced by a different unaudited one. |
| `tests/fixtures/bridge-parity/bridge-parity.json` | `bridge_parity_holds_against_canonical_fixtures`, an ordinary blocking test | A bridge RESPONSE change. Every count can stay flat while a deletion silently alters what the bridge returns, including through a type the bridge and the catalog share (Risk 18). |

The parity fixture is the input Phase 6 leans on most heavily, because
Phase 6 is a deletion campaign and the failure mode of a deletion campaign
is removing something that was load-bearing for output nobody was watching.
Each deletion step in section 5 should be run with the parity verifier
green BEFORE and after; a red parity row is the signal that a "dead" bridge
arm was not dead.

Regenerating the fixture is a decision, not a chore. `settle()` says so in
its own failure text: a diff there means a bridge output changed and needs
a new explicit decision. During Phase 6 the ONLY legitimate regeneration is
the cut itself (see 3.7).

## 3. Deletion inventory by subsystem

### 3.1 Version-1 project authority
- `ProjectRegistry` (`src/projects.rs`, `crates/bbox-indexing/src/projects.rs`) and every `bridge_registry()` caller.
- `ProjectRecord` and `ProjectRecordsSnapshot::records` (`crates/bbox-corpus-core/src/project_record.rs`). `corpus_project_ids`, `authority_epoch`, and `code_identities` SURVIVE: Phase 5 clause 1 proves the corpus surfaces depend on those and not on `records`.
- `CatalogProjectRecordsProvider`'s record-projection half (`crates/bbox-indexing/src/catalog_records.rs`). Retain until every v1-shaped consumer is converted; it is the last thing to go.

### 3.2 Legacy publisher lane
- `PublisherRefStore` and `elect_publisher` (`crates/bbox-indexing/src/publisher.rs`).
- `resolve_authorized_publisher`, `AuthorizedPublisher`, `PublisherAuthorizationCache`, and the 250 ms TTL (`src/server/knowledge_lifecycle.rs`).
- The publisher-alternate parameter on the bridge overlay recompute path.

### 3.3 Bridge caches and views
- Scope-keyed published knowledge and gap caches, superseded by the content-stamp-keyed catalog caches.
- `hydrate_repo_recall_stats` on the published read path. Catalog accepted reads deliberately omit it ([plan 4.14](durable-project-catalog-phase5-impl.md)), so it dies with the bridge.

### 3.4 Legacy carriers
- Watcher: `ArtifactWatchAttachment::{Selected, CheckoutId}` and the bridge two-step scope discovery in `DaemonArtifactWatchAccess::with_discovery`.
- Repository I/O: `RepoCarrierTarget::{Selected, Checkout}` and the `PublisherConfigTreeRead` scope-discovery lease the legacy arm takes. The catalog arm already skips it, which matters beyond cost: that lease rides `repo_knowledge` ([D-032](../../DECISION_LEDGER.md#d-032)).

### 3.5 Compatibility observation counters
`CheckoutAccessObservations` keeps a closed, low-cardinality key space as
Phase 6 cut evidence ([plan 4.17](durable-project-catalog-phase5-impl.md)).
Do NOT delete it before the cut: `active_compatibility_lanes` is how the cut
is judged safe. Delete `CheckoutAccessSourceLane::{LegacyProjectRecord,
LegacyCheckoutRegistry, LegacyPathResolver}` with the lanes themselves, and
keep `NativeAttachment`.

The separate `ProjectRuntimeStatus` projection (P5-G) is NOT cut evidence
and is not affected.

### 3.6 Accepted generation GC roots
`AcceptedPublicationRuntime::protected_generation_roots` reports current,
prior, and pinned-read generations. Phase 6 inherits it unchanged; storage
maintenance may collect only after a fresh protected-root read. No cleanup
debt is outstanding at the Phase 5 exit gate.

### 3.7 Bridge parity fixture inventory
`src/server/bridge_parity.rs` and `tests/fixtures/bridge-parity/bridge-parity.json`
pin the bridge's exact responses. Both are bridge-only and both are deleted
AT the cut, not before: the harness is the instrument that proves every
earlier step in section 5 changed nothing observable, so deleting it early
removes the evidence that the cut was safe.

Each row names the bridge surface it pins and what happens to that row:

| Parity row | Pinned surface | At the cut |
|---|---|---|
| `publisher_authorization` | `AuthorizedPublisher` four fields, scope-keyed cache, `elect_publisher` | Dies with 3.2. |
| `published_knowledge`, `own_knowledge`, `all_knowledge` | Bridge published read plus overlay merge | Dies with 3.2 and 3.3. The catalog lane keeps its own view tests. |
| `published_gaps`, `own_gaps`, `all_gaps` | Same, gap lane | Dies with 3.2 and 3.3. |
| `project_administration` | `bbox_project_list` over `ProjectRecord` | Dies with 3.1. This row is the one that will move FIRST, because it renders record fields directly. |
| `watcher_carriers` | `ArtifactWatchAttachment::{Selected, CheckoutId}` | Dies with 3.4. |
| `checkout_observations` | Compatibility lane key-space and the granted/denied split | Dies with 3.5. Its `active_compatibility_lanes` going empty IS the cut signal. |
| `file_provider`, `blame`, `render`, `provenance_export_plan`, `provenance_note_export`, `provenance_note_import` | Bridge-lane ROUTING into surfaces that survive | Row dies; the surface does not. A red row here during Phase 6 means a converted adapter changed output, which is a defect, not progress. |
| `doctor_sections` | Section inventory, per-section finding counts, levels, next commands | Row dies. Doctor survives; its bridge-shaped findings do not. |
| `catalog_only_tools_refuse` | `bbox_project_publisher_advance` and `_status` refusing `error.project_catalog_inactive` | INVERTS. This is the only row that must be DELETED rather than carried: after the cut the refusal is wrong, so a row asserting it would be actively false. |

Two properties Phase 6 must not quietly relax:

- **Normalizations are exact-value, never pattern.** Each is declared per
  row and self-policing in both directions: a declared substitution that
  never fires fails as vacuous, an undeclared one that fires fails as
  unaudited, and a substitution that lands inside an unrelated token fails
  as a mid-token landing. A Phase 6 change that reaches for a regex
  normalization to make a row pass has converted the proof into a
  formality.
- **Every row compares the COMPLETE serialized response.** Nothing is
  dropped or summarized. Three surfaces were briefly projected instead
  (the system-memory trailer, exact observation counters, doctor finding
  messages) and the closing bookend review was right to reject that:
  changes confined to an omitted field left the comparison green.
  Determinism for each was bought at the source instead. The system-memory
  catalog is pinned to a fixture-owned pair, so the trailer is captured
  whole and moves only when the harness moves. Observation counters are
  exact because the harness drops the publisher-authorization cache before
  every row, which removes the 250 millisecond TTL from the measurement
  rather than tolerating its variance. Doctor is captured whole, with the
  daemon version and the host state directory substituted by exact value
  from the same sources doctor renders them from.

Re-projecting any of those three to make a Phase 6 diff go away would
re-open the finding, not resolve it.

The section 13.8 fixture set
(`crates/bbox-indexing/tests/project_catalog_migration_facade.rs`) is NOT
deletion inventory. It is catalog-side and SURVIVES the cut; only its
migration-shaped half retires with the migration facade. Do not delete it
by association with the parity fixture beside it in the same P5-H
milestone.

## 4. Residuals Phase 6 inherits, not fixes
- **[D-033](../../DECISION_LEDGER.md#d-033) item 1**, the bind/advance detach-at-swap window. Catalog detach does not take the publication lock, so a pointer can name a freshly detached attachment. Phase 5 makes it OBSERVABLE (`binding.status == "detached"` in the runtime projection and a doctor action finding) and repairable by explicit bind. Phase 5 does not claim to close it.
- **[D-033](../../DECISION_LEDGER.md#d-033) item 2**, the synthetic `v1-root` checkout id for markerless checkouts. Retires with the v1 lane.
- **Bridge capability asymmetry** ([D-032](../../DECISION_LEDGER.md#d-032)): version-1 records carry no capability bits and cannot truthfully derive them. Resolves when the v1 lane goes.

## 5. Ordering constraint
The cut is not a single commit. The safe order:
1. Convert or delete the remaining v1-shaped consumers, watching the ratchet counts fall.
2. Delete the legacy publisher lane (no catalog caller).
3. Delete the bridge carriers, watcher and repository I/O.
4. Delete `ProjectRegistry` and `ProjectRecord`.
5. Delete the compatibility projection LAST, then the compatibility observation lanes.

Reversing 4 and 5 breaks the cut: the projection is what keeps v1-shaped
consumers alive while they are being converted.

## 6. Status of this document
`lifecycle: partial`. Both P5-H deliverables this document was waiting on
have landed and both run blocking:

- the checkout-open call-site audit, recorded in section 2.1 as a
  machine-checked input;
- the bridge parity fixture, recorded in section 2.1 as a machine-checked
  input and in section 3.7 as its own deletion inventory.

### 6.1 Why this is still partial
This document was briefly marked `complete` on the strength of those two
inputs existing and running blocking. The closing bookend review then found
that existing and running is not the same as VALID, and its findings
contradict assertions this document makes. A handoff inventory whose prose
outruns its machine checks is the exact failure mode section 1 says the
pairing exists to prevent, so the marker goes back until the checks are
worth what the prose claims for them.

Open findings, all in the closing bookend round:

| Finding | Contradicted assertion |
|---|---|
| 4 (high) | Section 2.1 says the parity fixture catches a bridge RESPONSE change. It did not catch one confined to a field the harness projected rather than compared: the system-memory body on the published views, the exact observation counters and sequences, and doctor finding messages. Section 3.7's claim that widening a projection converts the proof into a formality applied to the harness's own three projections first. FIX LANDED, awaiting re-review: all three are captured complete, with determinism bought at the source and only exact-value substitutions remaining. |
| 2, 3 | Findings against the other P5-H proofs, held by their own workers. They bear on section 2.1's claim that the three machine-checked inputs together leave no unwatched gap. |

Re-completion is a final-round act, not a same-round one: it happens after
the re-review passes, not after the fixes are pushed. Until then, treat
section 2.1's coverage claims as the INTENT of the inputs rather than a
statement of what they currently prove.
