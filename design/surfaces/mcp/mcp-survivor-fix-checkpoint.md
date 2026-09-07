---
title: "MCP survivor caller-contract fixes"
kind: design
corpus: blackbox-design
lifecycle: implemented
topic:
  - surfaces
  - mcp
brief: "Historical caller-contract implementation checkpoint with evidence and a completed audit closeout."
---

# MCP survivor caller-contract fixes

2026-09-07 supersession: [roadmap elision](roadmap-retirement.md) removes the
roadmap subsystem completely. Historical-reader, preservation and migration
obligations below describe this earlier checkpoint and no longer apply.
Measured evidence and original counts remain unchanged.

This is the implementation checkpoint for thread-c749d06c following the
[reconciled audit](mcp-survivor-action-audit.md) at fdd0180c. The complete
109-tool matrix remains a coverage inventory, not a claim that every action,
source mode, failure injection and provider path has been executed.

Subsequent corrections, retirement and production rollout are recorded in the
[closure milestones](mcp-survivor-closure.md). This earlier checkpoint remains
a record of its tested revision.

## Implemented contracts

| Queue | Result | Regression evidence |
| --- | --- | --- |
| R01 | Prune rejects invalid providers and explicit empty task IDs before selection or effects. Wait durations must be finite, nonnegative and representable before observers register. | Mixed-provider preservation and duration-edge tests; isolated HTTP invalid preview/apply and wait cases. |
| R02 | Review validates before owner enqueue; note/report/prune/dissolve bound admitted batches. Allocator observations use strict locked read-modify-write and preserve corrupt bytes and concurrent lanes. Config discovery distinguishes unavailable from empty. | Corrupt-store, concurrent-writer, rejected mutation and bounded receipt tests. |
| R02 | Broadcast preserves earlier admitted task IDs after later member/history errors. Team deltas preserve peer changes; dissolve reports cancellation and removal separately. Catalog detach/unregister report auxiliary cleanup separately; detach supports an epoch-validated cleanup retry. Compaction and multi-note export preserve prior completed effects. | Isolated partial-outcome regressions, including failed rebuild, second export write, poisoned watcher, stale epoch and successful cleanup retry. |
| R02/R06 | Roadmap transitions validate before assigning any fields; successful create/update/promote retain compact receipts and exact IDs. Promotion consumes typed thread identity rather than parsing display prose. | Invalid update leaves the complete item unchanged through later persistence/reopen; large Unicode title create/update/promote and repeated promotion tests. |
| R03 | Artifact success/metadata projections withhold source URLs and daemon paths. Fetch errors strip URLs; metadata/catalog errors use path-free messages. | Synthetic URL user/password/query sentinels inspected in complete MCP fetch-failure and supersede/metadata replies. Account/MCP credential projection and agent metadata tests remain active. |
| R04 | Thread metadata, gaps and scoped visibility diagnostics, artifact inventories/receipts, doctor findings and system-memory catalogs have complete recovery. Doctor pages the complete producer findings, including findings beyond the old prefix. | Escaped Unicode reconstruction, later severe findings, stale/cross-selector refusal and no-mutation read tests. |
| R05 | Ordinary knowledge, agent search, schema, roadmap and publisher status use bounded projections. Bundles and reference-size results automatically page oversized complete replies; graph validation separately pages variants and error rows. | Complete serialized envelope tests, ranked row preservation, stable identity/CAS checks, variant stamps and exact body reconstruction. |
| R06 | Served schemas and tool docs state exact-reader selectors, wrong-action refusals, owner locality and live offset behavior. Explicit thread summary works; sessions list exposes its 30/100 limits. Complex surface predicates no longer appear to be unconditional wildcard matches. | Selector/schema tests and isolated MCP served-catalog calls. |
| R07 | Existing useful compatibility lanes remain explicit. No tool is deleted solely because an older audit called it a candidate. Roadmap remains an operator-directed tracker; its consumer/data ownership decision remains gap-56c74f23. | Source review of distinct bridge/history/owner outcomes; the handler catalog retains 109 names. |
| R08 | Migration resolves the selected page before planning its sidecars. Doctor section selection uses the selected producer. Storage-health discovery explicitly discloses its rescan. | Target-planning visit-count regression and section-selection tests. These are bounded-selection improvements, not an index/backend redesign. |

## Evidence boundaries and retained obligations

Exact body cursors bind the serialized content and relevant selectors. Most
readers recollect the current view on every page; changed evidence refuses
continuation. Offset collections explicitly remain live views. Neither form
claims a durable snapshot unless its existing domain contract provides one.

Mutation receipts distinguish admission, in-memory changes, requested
persistence, confirmed persistence, and partial effects where those states are
observable. A directory-sync failure after atomic replacement can leave
durability unconfirmed. Failed current provenance writes may have applied
fragments beyond the count confirmed for earlier completed targets.

Confidentiality evidence is field-specific. Source URLs, credential values and
daemon-owned metadata paths are withheld by the relevant projections. Opaque
operator diagnostic prose is not promised to be secret-free or automatically
credential-sanitized.

The later [closure milestones](mcp-survivor-closure.md) complete diagnostic
snapshot paging and native locator recovery, and remove roadmap entirely.
Thread-c749d06c and gap-7a2513c9 are closed on their MCP scope. Fleet/provider
execution acceptance and backend delivery residuals continue separately in
thread-87b1eb39; they do not keep this audit open. Storage-health scans and
changing doctor/publisher detail recollection remain disclosed costs.

## Bridge presentation amendment

The operator-authorized caller-contract fixes supersede the historical byte
freeze only for these eight bridge capture rows: all_gaps, own_gaps,
published_gaps, all_knowledge, own_knowledge, published_knowledge,
file_provider and provenance_note_export. Gap rows gain exact-reader hints;
knowledge rows change system-memory signpost separators. File measurement
changes JSON formatting; provenance export adds completed/total target counts. Selection,
visibility, tombstones, registry authority, file measurement and written
provenance content are unchanged. The capture remains a complete byte-for-byte
comparison after this reviewed update; no fields or normalizations are removed.

## Verification

Tested code revision: `7e98006e851260a5734ffa13b341b4aed8ea5eba`. The full
workspace nextest profile passed all 6,733 tests in 144.975 seconds, with
19 skipped and no scheduling overrides. Workspace clippy, binary build,
pinned formatting and concurrency lint also passed. The isolated HTTP probe
passed 268 checks against the 109-tool catalog; the largest measured complete
MCP result was 8,321 bytes. The catalog count does not imply every tool or
action was exercised by HTTP. Commands, per-call measurements and evidence
boundaries are recorded in [fix verification evidence](mcp-survivor-fix-verification.json).
Earlier failed runs are correction-loop evidence, not passing gates. The
production service is outside this implementation checkpoint; no deployment
is inferred from a source push or a successful isolated daemon probe.
