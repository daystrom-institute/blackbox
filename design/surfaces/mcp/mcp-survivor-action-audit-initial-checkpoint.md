---
title: "Surviving MCP action audit: initial and integration checkpoints"
kind: design
corpus: blackbox-design
lifecycle: partial
topic:
  - surfaces
  - mcp
brief: "Current served-tool dispositions, live caller-contract findings, and explicit evidence limits."
---

# Surviving MCP action audit: historical checkpoints

Preserved from source 004822640975 on 2026-09-06. The findings and matrix below
record the initial audit, followed by integration checkpoints. Their present-tense
claims and final next steps are historical. Use the [current reconciliation](mcp-survivor-action-audit.md)
for thread-c749d06c dispositions; GLM follow-up is
owned by thread-c130128f. Original measured evidence is retained unchanged.

This is the current action checklist for thread-d7cd3385. It replaces the
historical 190-tool table as the audit's coverage denominator. Source
`f4fbdfda6366e16114feb555f72e532c07ccd780` and live `surface=ops` replay
agree on **109 named tools**. The action matrix below also separates meaningful
read, mutation, detail, and authority branches. Those rows are descriptive
branches, not additional tool names or invented `action` parameter values.

The task is to audit what a caller can choose, provide, receive, trust, and
recover. Findings about delivery or publication remain dependencies of an
honest response; they do not authorize a transaction engine or indexing
redesign as part of this audit. The existing delivery and readiness gaps remain
separate. No runtime code, services, dispatches, durable knowledge, or production
data deletion were changed in this pass. Filing the audit gap used the existing
queued owner lane.

## Evidence and limits

- [Evidence snapshot](mcp-survivor-audit-evidence.json): exact served names,
  returned-result sizes, safe probe arguments, and continuation checks.
  Bodies, credentials, unrelated project identities, and native session
  identities are excluded.
- Source review covered each adapter and its parameter/response branches,
  with targeted domain reads for discovered findings. Existing test assertions
  were inspected selectively; **no Rust test suite was run in this docs-only
  audit**. Prior deployed test/smoke evidence is explicitly labeled below and
  remains in the [chronological audit](mcp-response-and-contract-audit.md).
- The client can call 101 of the 109 names. Its missing eight are the five
  `bro_agent_*` and three `bro_allocator_*` tools. Their rows have source
  evidence, not current live execution evidence. Ops replay proves discovery,
  not callable access from this client.
- Live checks used reads, invalid read selectors, and GC dry-run plus immutable
  receipt reads. No arbitrary active task was resumed or controlled. Unknown
  task IDs exercised wait errors without dispatching an agent.
- Sizes are UTF-8 bytes of compact JSON serialization of the returned tool
  result, including text escaping and structured content when present. They
  exclude HTTP/JSON-RPC framing and are not token measurements. Text and
  structured sizes are recorded separately; their duplication matters.
- A small live sample proves that response, not a growth bound. Source paths
  without producer paging are still marked for adjustment even if empty today.
  A bounded response can still perform broad work before projection.
- `retain` means the action has a defensible distinct role and no retirement
  recommendation here. It is **not** a blanket pass for every failure,
  confidentiality, growth, or mutation case. Linked findings remain obligations.
  `adjust` is a concrete caller-contract correction; `restrict` is an explicit
  specialist/owner-local disposition; `retire-candidate` requires the stated
  compatibility or consumer/data decision. No removal is implied to have shipped.

Audit findings are tracked by [gap-7a2513c9](../../../.bbox/gaps/gap-7a2513c9.json),
with pre-existing gaps referenced where they already own the work. The matrix
is complete as a catalog/branch inventory; audit closure remains partial until
its outstanding findings and verification obligations are discharged.

## Findings and concrete acceptance

### A01. Missing tasks can become successful aggregate completion

**High, live reproduced.** Calling `bro_when_all` with one invented task ID
returned `{"all_completed":true,"results":[]}`. The matching `bro_when_any`
returned `{"any_completed":false,"results":[]}`; `bro_wait` correctly returned
an unknown-task error. In [dispatch.rs](../../../src/tools/dispatch.rs),
`resolve_when_targets` accepts supplied IDs, both aggregates `filter_map`
missing tasks away, and `all` on the empty result becomes true.

Required surface correction: reject unknown selections before waiting, or
explicitly return requested/missing IDs and an incomplete aggregate outcome.
Define mixed known/unknown, duplicate IDs, competing team/task selectors,
pruned team-history IDs, and input/result fanout limits. Preserve per-task
timeout/failure versus successful completion. This is an adapter contract
finding, not a task-store redesign.

### A02. A read-only pin filter requires checkout write authority

**High, live reproduced.** `bbox_pin(action=list, project=89bd722f)` fails with
`attachment_inactive` because the root cannot be canonicalized.
The corresponding thread-scoped list without project returns normally.
[attention.rs](../../../src/tools/attention.rs) resolves every nonempty project
with `resolve_project_write_scope_with_id` before branching; the list cannot
reach the host-owned pin store. `bbox_note` uses the same write-scope resolver
for project association; that mutation path is source-only evidence.

Required correction: use catalog/filter identity for reads, with explicit
unknown-selector behavior. Decide the actual authority of host-owned note/pin
writes separately from checkout-owned knowledge. Validate scopes/actions before
performing locality work. Acceptance includes existing remote project,
attachment-less logical project, alias, historical path, and unknown selector.
No checkout transport needs to be invented to list stored pins.

### A03. Invalid filters and scopes masquerade as ordinary results

**Medium, live reproduced.** Invalid scope in `bro_brofile list` returns the
same first brofile as the normal list; invalid scope in `bro_mcp list` returns
the effective server list. Invalid pin scope returns `0 pins`; invalid
packet-event operation returns a successful empty array. In contrast, knowledge
mode validation correctly rejects a typo.

Source also shows different dry-run/apply project resolution in
[storage_migration.rs](../../../src/tools/storage_migration.rs): dry-run compares
the raw filter only to registered IDs, while the schema advertises paths and
apply uses project resolution. `bro_brofile get` does not use its scope to
choose the requested store.

Required correction: enums or shared explicit validation for closed choices;
action-specific fields either apply, are documented as irrelevant, or reject.
Do not widen a typo into global/effective lookup or make it look like no data.
Free-text search/filter semantics are different from closed vocabulary.

### A04. Exact detail is still an unbounded response on several surfaces

**Medium to high usability, live and source.**

| Probe | Returned result bytes | Caller problem |
| --- | ---: | --- |
| `bbox_thread get`, this thread | 23,108 | Complete growing history, no continuation |
| `bbox_project_graph_describe`, design graph | 26,237 | Full schema embedded in every description |
| Exact packet body, 1,024-byte page | 3,398 | Working bounded alternative |
| GC candidates, 128-byte page | 548 | Working bounded alternative |

[project_graph_read.rs](../../../src/project_graph_read.rs) always clones the
schema into the description. Its compact schema value alone is 9,229 bytes
before pretty-printing and result escaping. Native transcript reads explicitly
label indexed projection and clamp body limits; they should retain that honesty.

Other source cases: full notes, exact knowledge/system memories, review queue
bodies, full brofiles, agent manifests/resolved brofiles, allocator traces, full
doctor reports, and full evidence bundles. Row-count limits and a global
oversize error do not make an oversized individual record recoverable.

Required correction: summary plus exact text/JSON pages, with selectors and
content/version-bound cursors. Preserve thread handoff, graph generation,
knowledge provenance and actionable failure fields in the first response.
Do not silently trim the only copy of an answer. Prefer existing body-page
helpers and exact readers where they fit.

### A05. Repeated diagnostics crowd out the actual answer

**Medium, live reproduced.** A knowledge query with no matching entries and
`limit=1` returned **6,789 bytes**: 3,353 text bytes and 3,344 structured bytes,
mostly visibility diagnostics. The same query narrowed to this project is a
different scope, not a complete recovery of omitted global diagnostics.
[bbox_knowledge](../../../src/tools/knowledge.rs) appends textual diagnostics
and emits the structured view as well.

Publisher status returned 4,135 bytes with accepted scope, ref, commit,
generation and source binding repeated under `health`. Its CAS generation
and pointer tokens are useful and must survive a summary projection.
Surface replay repeats selected policy lists alongside the resulting tool list.

Required correction: scoped warning/count summary, one identity representation,
and bounded opt-in diagnostic detail with exact recovery. Keep
unavailable/stale/queued/partial state visible. Treat connector inventories and
debug replies as bounded producers too; debug is not permission to disclose
credentials or unrelated data.

### A06. Nested inventories and fanout remain outside row limits

**Source finding, some live small cases.** Unpaged arrays/maps remain in surface
list/replay/describe, graph list/schema/validation errors, project compatibility
list, pin/review queues, account lists, allocator status/probes, partition
inventory, migration plans, and packet event history. Packet apply-all/audit can
return caller- or packet-sized findings. Wait/broadcast aggregate bodies scale
with task/member count even where each task result is individually bounded.

Required correction: bound serialized envelope, row bodies and nested fanout;
offer continuation/detail, totals and honest truncation. Latest-N events need
an older-page path, not only a lower time bound. For caller-supplied batches,
reject or explicitly partition oversize requests before effects. A response
receipt is detail recovery, not an approval token for later mutation.

### A07. Search suggests unusable recovery calls for non-transcript hits

**High usefulness, live reproduced.** Searching the exact phrase
`"MCP surface audit"` returns this thread as the top hit, then recommends a
thread-store locator with `bbox_context` and `bbox_messages(session_id="")`.
Following both hints fails: transcript-not-indexed and blank-selector errors.
[search.rs](../../../crates/bbox-corpus-index/src/index/search.rs) distinguishes
Slack from all other hits, not transcript versus thread/knowledge/code entities.

Required correction: preserve hit entity type/ref and emit the corresponding
exact reader (`bbox_thread get` or entity inspection for a thread). Emit
transcript context/session hints only for valid transcript coordinates. A
source-native search hit successfully followed context/messages in this pass,
so repair the type dispatch rather than weakening reader validation.

### A08. Served compatibility and overlapping admin actions need disposition

**Source and live, retirement recommendations only.**

- `bbox_absorb` does no import; its global call still returns diagnostics.
  `bbox_bootstrap` always returns a retired error. Neither earns callable
  discovery as a working capability. Preserve a compatibility tombstone only
  if a concrete client needs it.
- `bro_mcp sync` has no destination: `FANOUT_PROVIDERS` is empty in
  [mcp.rs](../../../src/orchestration/mcp.rs). Current providers consume
  per-dispatch injection. Retire sync and stale provider-CLI/Gemini wording.
- `bbox_project_list` is an unpaged compatibility root projection; catalog
  list/get is authoritative logical-project discovery. `unregister` means
  detach in catalog mode, not logical project retirement. Reconcile aliases
  and bridge callers before consolidating.
- `bbox_roadmap` still has 13 executable branches, including render templates,
  ranking and thread promotion. Existing
  [gap-56c74f23](../../../.bbox/gaps/gap-56c74f23.json) owns retirement and data/
  consumer disposition. Do not label the whole tool a no-op. Its promote
  replay hint recommends `bro_resume` for thread IDs, another chooser mismatch.
- Keep `bbox_corpus_search`: the harness actually projects its id/text
  contract as `corpus_search` in
  [mcp.rs](../../../crates/bro-harness/src/mcp.rs). Keep seed discovery's graph
  augmentation, and distinguish stored agent get from computed describe.
  Similar naming alone does not justify removal.
- Legacy compaction, migration and provenance adapters retain specialist roles
  only while legacy data or owner-side consumers need them; tests/history are
  not themselves proof of a current external consumer.

### A09. State the real owner and restriction before the failed call

**Source plus pin/live discovery evidence.** Remote attach/promote/scope
migration, relocation/ejection, publisher binding and host render do not become
working remote operations merely by returning truthful errors. The
[locality audit](../../daemon-runtime/mcp-surface-locality-audit.md) documents
retained local/offline lanes; producer onboarding and missing-root retirement
have their own open gaps. Catalog detach/default selection remain path-free.

Brofile/team/MCP project configuration must identify which host/store owns it.
`bro_mcp` still resolves project configuration paths in the daemon process;
its stdio-add error points at provider CLIs that the current dispatch plane
does not use. Ordinary admin discovery should reveal the prerequisite and an
actual supported lane. Recorded attachment capabilities are observations,
not proof of present filesystem authority. Do not invent an owner CLI.

### A10. Schema and chooser prose still disagree or require cold knowledge

**Source and live schema evidence.** Free-string action bags in threads,
pins, brofiles, teams and roadmap hide required combinations; roadmap's action
field omits `default_template`; partition action prose omits `scrub`.
MCP add advertises stdio but rejects it. Packet inputs/datasets and nested
artifact schemas require knowledge not supplied by their shape. Surface list
explicitly ignores replay selectors; an action-specific schema would clarify it.

Publisher status and path finding chooser descriptions enumerate extensive
internal contracts, while render, provenance import and several lifecycle tools
say too little about ownership. Move deep instructions to managed docs, keep
chooser purpose/prerequisites short, and make validation/schema/examples agree.
Packet audit fidelity is agreement with the supplied dataset, not proof of a
universally correct classifier; remove the arbitrary-scale cost claim.

### A11. Raw configuration/debug projection still needs adversarial proof

**Source concern, not a claimed live secret leak.** MCP account/server response
views already redact values and have source tests. Agent get/describe still
serialize manifests, inline brofiles and resolved configuration; allocator
status/trace/probe return raw summaries and nested records. No real credential
was injected or extracted to test these paths.

Required verification: isolated synthetic sentinels in every accepted secret/
opaque-config field, nested input and debug path, then assert absence from text,
structured replies and errors. Preserve safe identity/presence fields. Do not
infer that all free text contains secrets, or that one redacted account reader
proves every configuration reader safe.

### A12. Admission, persistence, effects and publication are different outcomes

**Mixed source/prior proof.** Existing knowledge/gap queues correctly announce
asynchronous delivery. This pass filed gap-7a2513c9 through that lane; the local
file subsequently appeared. That is delivery evidence, not accepted publication.
The existing checkout precondition/replay defect remains gap-92bd3d34 and is
not expanded into backend implementation here.

A separate source-only response issue exists in allocator probe updates:
`probe_store_save` returns no result and its adapter returns success. Verify
failed persistence with an isolated fixture before claiming durable update.
For imports, register/eject, artifact installation, broadcast, cleanup and
control, retain per-stage/child state and uncertainty. Dry-run GC success is
not live apply validation. Packet apply/audit append observation events even
though their classification is deterministic.

### A13. Continuation must say whether the underlying view can change

Catalog exact-detail stale epoch rejection worked. Packet body reconstruction
recovered 6,969 exact bytes over seven pages; GC candidate detail recovered
298 exact bytes over three pages. Both reconstructed valid JSON. GC receipt
reads do not rerun deletion.

Threads/notes/brofile/team lists use live offsets: document possible movement
and restart behavior, rather than calling them snapshot-stable. Tool-call
continuation is a candidate offset and can return an empty nonterminal page.
Keep these distinctions. Historical prior proofs are not a substitute for a
current oversized/stale-cursor probe on a changed producer.

### A14. Small replies can still require broad work

Source shows doctor builds its report before section projection, storage health
scans before file paging, and several catalog/list paths load all records before
slicing. Search compatibility paths can trigger indexing on an empty index;
embedding coverage/diagnostics/probes are explicitly expensive opt-ins.

Keep cheap healthy summaries cheap, document exceptional cost, and use bounded
work or targeted collection where needed. This is a query-cost finding. It does
not turn the MCP audit into an index/storage redesign.

## Action matrix

Source links identify adapter owners. Evidence IDs refer to the companion JSON.
`source` is inspection only; `prior milestone` points to the chronological
audit and does not claim a test or mutation was executed in this pass. Each
row's rationale states the useful decision and any material residual; findings
above specify the acceptance correction. Where a row has no finding, retain
its present contract subject to the stated evidence limits.


### [src/tools/agents.rs](../../../src/tools/agents.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bro_agent_describe` | describe | adjust | Resolved brofile/filters support dispatch inspection, but current output dumps full brofile plus manifest. Clarify it is partial resolution, not every runtime permission plane. | source | A04 A10 A11 |
| `bro_agent_dispatch` | dispatch | retain | Manifest validation and prompt construction add value over raw bro_exec; custom adapters explicitly refuse. Separate agent attribution from task admission. | source; prior milestone | A12 |
| `bro_agent_get` | get | adjust | Stored manifest/lifecycle read is distinct from computed dispatch description; full manifest, embedding and inline configuration need bounded intentional projection. | source | A04 A11 |
| `bro_agent_list` | detail | retain | Installed callable-agent discovery excludes retired adapters and pages summaries; useful independently of artifact receipts. Eight specialist tools are not callable in this client. | source | A04 |
| `bro_agent_list` | summary | retain | Installed callable-agent discovery excludes retired adapters and pages summaries; useful independently of artifact receipts. Eight specialist tools are not callable in this client. | source | A04 |
| `bro_agent_search` | query | adjust | Semantic installed-agent discovery is useful; descriptions/anti-patterns and repeated vector telemetry need summary/detail split. total_matched currently equals returned count. | source | A05 A06 |

### [src/tools/artifacts.rs](../../../src/tools/artifacts.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_artifact_install` | HTTP source | retain | Versioned install remains useful for packets/brofiles/agents/teams; exactly one input, inline byte cap, typed kind, no caller paths. Nested artifact schema still requires discovery. | source; prior milestone | A10 A12 |
| `bbox_artifact_install` | inline | retain | Versioned install remains useful for packets/brofiles/agents/teams; exactly one input, inline byte cap, typed kind, no caller paths. Nested artifact schema still requires discovery. | source; prior milestone | A10 A12 |
| `bbox_artifact_list` | detail | retain | Paged installation/lifecycle inventory is distinct from executable discovery; retired kinds remain explicitly inactive. Detail can still contain a single oversized row. | artifact_list | A04 A13 |
| `bbox_artifact_list` | retired kind | retain | Paged installation/lifecycle inventory is distinct from executable discovery; retired kinds remain explicitly inactive. Detail can still contain a single oversized row. | artifact_list | A04 A13 |
| `bbox_artifact_list` | summary | retain | Paged installation/lifecycle inventory is distinct from executable discovery; retired kinds remain explicitly inactive. Detail can still contain a single oversized row. | artifact_list | A04 A13 |
| `bbox_artifact_remove` | call | restrict | Hard removal is admin-only and confirm-gated; preserve retained-kind policy. No destructive probe performed. | source | A12 |
| `bbox_artifact_supersede` | call | retain | Version lifecycle without deleting history; retain same-kind validation and explain activation effect versus receipt retention. | source | A12 |

### [src/tools/attention.rs](../../../src/tools/attention.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_inbox` | get | retain | Bounded attention aggregation is useful; retain per-group incompleteness and exact follow-ups. It is observational, not automated workflow execution. | source; prior milestone | A13 |
| `bbox_pin` | delete | adjust | Scope/target pins are distinct from knowledge; validate enum and action fields before resolving ownership. List and mutation must not share an indiscriminate write lease. | source | A02 A10 |
| `bbox_pin` | list | adjust | Ambient context discovery must be a path-free project filter; current adapter acquires write scope and fails. Full pin bodies also have no list/page cap. | pin_project, pin_unscoped, pin_invalid | A02 A03 A04 |
| `bbox_pin` | set | adjust | Scope/target pins are distinct from knowledge; validate enum and action fields before resolving ownership. List and mutation must not share an indiscriminate write lease. | source | A02 A10 |

### [src/tools/config.rs](../../../src/tools/config.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bro_mcp` | add | adjust | Persistent dispatch MCP settings remain useful; project paths still resolve daemon-local files, mutation replies expose paths, and add schema advertises unsupported stdio. | source | A09 A10 A12 |
| `bro_mcp` | allow | adjust | Persistent dispatch MCP settings remain useful; project paths still resolve daemon-local files, mutation replies expose paths, and add schema advertises unsupported stdio. | source | A09 A10 A12 |
| `bro_mcp` | clear_filters | adjust | Persistent dispatch MCP settings remain useful; project paths still resolve daemon-local files, mutation replies expose paths, and add schema advertises unsupported stdio. | source | A09 A10 A12 |
| `bro_mcp` | disallow | adjust | Persistent dispatch MCP settings remain useful; project paths still resolve daemon-local files, mutation replies expose paths, and add schema advertises unsupported stdio. | source | A09 A10 A12 |
| `bro_mcp` | get | retain | Selected configuration detail uses response_view redaction; explain server-not-registered domain outcome and daemon ownership. | source; existing redaction tests | A09 |
| `bro_mcp` | list | adjust | Effective servers/filters are useful, endpoint values redacted; scope is ignored and inventory unpaged. Distinguish effective union from selected-scope get. | mcp_list, mcp_bad_scope | A03 A06 A09 |
| `bro_mcp` | remove | adjust | Persistent dispatch MCP settings remain useful; project paths still resolve daemon-local files, mutation replies expose paths, and add schema advertises unsupported stdio. | source | A09 A10 A12 |
| `bro_mcp` | sync | retire-candidate | FANOUT_PROVIDERS is empty: sync resolves config/secrets but cannot synchronize any current provider. Per-dispatch injection is the real consumer; retire this action and stale CLI/Gemini guidance. | source | A08 |

### [src/tools/dispatch.rs](../../../src/tools/dispatch.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bro_allocator_probe` | clear | adjust | Specialist lane observation override must be explicit; write helper returns no persistence result, yet adapter replies success. raw_summary needs bounded/redacted handling. | source | A11 A12 |
| `bro_allocator_probe` | read | adjust | Specialist lane observation override must be explicit; write helper returns no persistence result, yet adapter replies success. raw_summary needs bounded/redacted handling. | source | A11 A12 |
| `bro_allocator_probe` | update | adjust | Specialist lane observation override must be explicit; write helper returns no persistence result, yet adapter replies success. raw_summary needs bounded/redacted handling. | source | A11 A12 |
| `bro_allocator_status` | candidate preview | adjust | Specialist allocation diagnosis has unique eligibility data; currently serializes all pools/probes/leases and candidates without paging. Identify configuration/worker ownership. | source | A06 A09 A11 |
| `bro_allocator_status` | status | adjust | Specialist allocation diagnosis has unique eligibility data; currently serializes all pools/probes/leases and candidates without paging. Identify configuration/worker ownership. | source | A06 A09 A11 |
| `bro_allocator_trace` | get | adjust | Exact selection explanation is distinct from current status; whole retained trace needs bounded body pages and safe raw diagnostics. | source | A04 A11 |
| `bro_broadcast` | dispatch | adjust | Team fan-out is useful external composition; each child admission can fail independently. Bound aggregate receipts and preserve per-member outcome/identity. | source | A06 A12 |
| `bro_cancel` | call | retain | Terminal cancellation is distinct from steer/interrupt; control result must report requested versus observed exit. Never infer ownership from dashboard discovery. | source; prior milestone | A12 |
| `bro_exec` | dispatch | retain | Core fresh-task admission; request_key protects duplicate admission, not exactly-once execution. Selector alternatives and worker cwd need explicit schema guidance. | source; prior deployed request-key probes | A10 A12 |
| `bro_exec` | request_key replay | retain | Core fresh-task admission; request_key protects duplicate admission, not exactly-once execution. Selector alternatives and worker cwd need explicit schema guidance. | source; prior deployed request-key probes | A10 A12 |
| `bro_interrupt` | call | retain | Interrupt active turn with optional redirect while retaining session; distinct from terminal cancel. Do not label delivery as completed redirection. | source | A12 |
| `bro_prune` | apply | restrict | Admin terminal-task cleanup has useful exact-ID scope; retro option additionally resumes models and needs explicit consequence. Aggregate cleanup reports need bounds. | source | A06 A10 A12 |
| `bro_prune` | preview | restrict | Admin terminal-task cleanup has useful exact-ID scope; retro option additionally resumes models and needs explicit consequence. Aggregate cleanup reports need bounds. | source | A06 A10 A12 |
| `bro_prune` | retro option | restrict | Admin terminal-task cleanup has useful exact-ID scope; retro option additionally resumes models and needs explicit consequence. Aggregate cleanup reports need bounds. | source | A06 A10 A12 |
| `bro_resume` | request_key replay | retain | Core existing-session continuation, single-flight and replay distinct from fresh exec. Preserve owner restrictions and actual returned task/session IDs. | source; prior deployed request-key probes | A12 |
| `bro_resume` | resume | retain | Core existing-session continuation, single-flight and replay distinct from fresh exec. Preserve owner restrictions and actual returned task/session IDs. | source; prior deployed request-key probes | A12 |
| `bro_retro` | call | restrict | Optional model turn for terminal-task feedback, not a passive log read. Retain specialized operator use with explicit resume semantics. | source | A10 A12 |
| `bro_status` | debug | retain | Progress and exact bounded deliverables have distinct modes and stale-cursor checks; latest preview is separate from complete result. Debug audit remains field-specific. | source; prior deployed preview/body proofs | A11 |
| `bro_status` | report | retain | Progress and exact bounded deliverables have distinct modes and stale-cursor checks; latest preview is separate from complete result. Debug audit remains field-specific. | source; prior deployed preview/body proofs | A11 |
| `bro_status` | result | retain | Progress and exact bounded deliverables have distinct modes and stale-cursor checks; latest preview is separate from complete result. Debug audit remains field-specific. | source; prior deployed preview/body proofs | A11 |
| `bro_status` | structured_exit | retain | Progress and exact bounded deliverables have distinct modes and stale-cursor checks; latest preview is separate from complete result. Debug audit remains field-specific. | source; prior deployed preview/body proofs | A11 |
| `bro_status` | summary | retain | Progress and exact bounded deliverables have distinct modes and stale-cursor checks; latest preview is separate from complete result. Debug audit remains field-specific. | source; prior deployed preview/body proofs | A11 |
| `bro_steer` | call | retain | Queue text into running harness without stopping current turn; preserve queued acknowledgement and unavailable-process error. | source | A12 |
| `bro_wait` | wait | retain | Single-task observation correctly rejects unknown ID; timeout snapshot is not completion or death. Result continuation reuses bro_status. | wait_missing; prior milestone | - |
| `bro_when_all` | task_ids | adjust | Wait aggregation drops unknown IDs and can claim all_completed with no tasks. Also lacks requested-cardinality/aggregate-result budget and rejects neither competing selectors nor duplicates. | when_all_missing; source | A01 A06 |
| `bro_when_all` | team | adjust | Wait aggregation drops unknown IDs and can claim all_completed with no tasks. Also lacks requested-cardinality/aggregate-result budget and rejects neither competing selectors nor duplicates. | when_all_missing; source | A01 A06 |
| `bro_when_any` | task_ids | adjust | Race observation silently drops unknown IDs; all-missing returns empty false rather than actionable error. Bound input and aggregate results; preserve selected identity. | when_any_missing; source | A01 A06 |
| `bro_when_any` | team | adjust | Race observation silently drops unknown IDs; all-missing returns empty false rather than actionable error. Bound input and aggregate results; preserve selected identity. | when_any_missing; source | A01 A06 |

### [src/tools/doctor.rs](../../../src/tools/doctor.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_doctor` | full | retain | Ranked health with bounded findings and section selection is useful; full diagnostic trees remain an explicit but unpaged edge case, and section narrows rendering after collection. | doctor; source | A04 A14 |
| `bbox_doctor` | summary | retain | Ranked health with bounded findings and section selection is useful; full diagnostic trees remain an explicit but unpaged edge case, and section narrows rendering after collection. | doctor; source | A04 A14 |

### [src/tools/gaps.rs](../../../src/tools/gaps.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_gap` | global | retain | First-class deduplicated substrate finding; queued project receipt is truthful. Live audit filing exercised admission only, not proof of publication. | audit gap filing | A12 |
| `bbox_gap` | project | retain | First-class deduplicated substrate finding; queued project receipt is truthful. Live audit filing exercised admission only, not proof of publication. | audit gap filing | A12 |
| `bbox_gap_resolve` | resolve | retain | Resolution and structured replacement link are distinct outcomes; paired admission is not atomic owner delivery, carried in existing transport gap. | source; prior milestone | A12 |
| `bbox_gap_resolve` | supersede | retain | Resolution and structured replacement link are distinct outcomes; paired admission is not atomic owner delivery, carried in existing transport gap. | source; prior milestone | A12 |
| `bbox_gap_update` | update | retain | Distinct edit/append semantics for an existing gap; preserve project ownership and queued status. Do not collapse into new occurrence creation. | source; prior milestone | A12 |
| `bbox_gaps` | debug | retain | Filtered summary pages and exact records preserve visibility warnings; bounded debug is useful. An oversized individual full record still needs recoverable expansion. | gaps; prior milestone | A04 A13 |
| `bbox_gaps` | exact id | retain | Filtered summary pages and exact records preserve visibility warnings; bounded debug is useful. An oversized individual full record still needs recoverable expansion. | gaps; prior milestone | A04 A13 |
| `bbox_gaps` | full | retain | Filtered summary pages and exact records preserve visibility warnings; bounded debug is useful. An oversized individual full record still needs recoverable expansion. | gaps; prior milestone | A04 A13 |
| `bbox_gaps` | summary | retain | Filtered summary pages and exact records preserve visibility warnings; bounded debug is useful. An oversized individual full record still needs recoverable expansion. | gaps; prior milestone | A04 A13 |

### [src/tools/graph.rs](../../../src/tools/graph.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_blame` | anchor | restrict | Line-level provenance is useful; requires checkout/attended authority. Describe partial Git-only outcome and working owner-side command in chooser. | source; prior milestone | A09 A10 |
| `bbox_blame` | git fallback | restrict | Line-level provenance is useful; requires checkout/attended authority. Describe partial Git-only outcome and working owner-side command in chooser. | source; prior milestone | A09 A10 |
| `bbox_bundle_evidence` | full | adjust | Packages evidence and cached paths, preserving stale-state warnings; nested edge caps exist but caller ref fanout and full properties need total-byte recovery. | opening sequence; source | A04 A06 |
| `bbox_bundle_evidence` | none | adjust | Packages evidence and cached paths, preserving stale-state warnings; nested edge caps exist but caller ref fanout and full properties need total-byte recovery. | opening sequence; source | A04 A06 |
| `bbox_bundle_evidence` | summary | adjust | Packages evidence and cached paths, preserving stale-state warnings; nested edge caps exist but caller ref fanout and full properties need total-byte recovery. | opening sequence; source | A04 A06 |
| `bbox_describe_schema` | agents | adjust | Graph vocabulary is useful; default 15 KB repeats field/edge catalogs. Full/agents adds an unpaged installed-agent inventory; use focused sections and correct live edge vocabulary. | schema | A06 A10 |
| `bbox_describe_schema` | full | adjust | Graph vocabulary is useful; default 15 KB repeats field/edge catalogs. Full/agents adds an unpaged installed-agent inventory; use focused sections and correct live edge vocabulary. | schema | A06 A10 |
| `bbox_describe_schema` | orientation | adjust | Graph vocabulary is useful; default 15 KB repeats field/edge catalogs. Full/agents adds an unpaged installed-agent inventory; use focused sections and correct live edge vocabulary. | schema | A06 A10 |
| `bbox_edge_compact` | apply | restrict | Legacy sidecar compaction is distinct from migration/GC; explicit local rebuild option is potentially costly. Retain as operator repair, not normal retrieval. | source | A14 |
| `bbox_edge_compact` | apply with rebuild | restrict | Legacy sidecar compaction is distinct from migration/GC; explicit local rebuild option is potentially costly. Retain as operator repair, not normal retrieval. | source | A14 |
| `bbox_edge_compact` | preview | restrict | Legacy sidecar compaction is distinct from migration/GC; explicit local rebuild option is potentially costly. Retain as operator repair, not normal retrieval. | source | A14 |
| `bbox_find_paths` | to | retain | Direction-preserving bounded BFS is distinct from inspection; keep fanout truncation, endpoint authority and path IDs. Chooser currently contains runbook-scale prose. | source; prior milestone | A10 |
| `bbox_find_paths` | to_type | retain | Direction-preserving bounded BFS is distinct from inspection; keep fanout truncation, endpoint authority and path IDs. Chooser currently contains runbook-scale prose. | source; prior milestone | A10 |
| `bbox_inspect_entity` | edge_cursor | retain | Identity, freshness and projection omissions survive summarization; exact property and edge cursors reject changed evidence. Full remains explicit. | opening sequence; prior milestone | A05 |
| `bbox_inspect_entity` | full | retain | Identity, freshness and projection omissions survive summarization; exact property and edge cursors reject changed evidence. Full remains explicit. | opening sequence; prior milestone | A05 |
| `bbox_inspect_entity` | property | retain | Identity, freshness and projection omissions survive summarization; exact property and edge cursors reject changed evidence. Full remains explicit. | packet_property, packet_recovery | A05 |
| `bbox_inspect_entity` | smart | retain | Identity, freshness and projection omissions survive summarization; exact property and edge cursors reject changed evidence. Full remains explicit. | opening sequence; prior milestone | A05 |
| `bbox_inspect_entity` | summary | retain | Identity, freshness and projection omissions survive summarization; exact property and edge cursors reject changed evidence. Full remains explicit. | opening sequence; prior milestone | A05 |
| `bbox_project_graph_describe` | all | adjust | Useful schema and retrieval status, but full schema is always embedded. Offer summary and exact schema expansion without losing generation/authority. | graph_describe, graph_missing | A04 |
| `bbox_project_graph_describe` | own | adjust | Useful schema and retrieval status, but full schema is always embedded. Offer summary and exact schema expansion without losing generation/authority. | graph_describe, graph_missing | A04 |
| `bbox_project_graph_describe` | published | adjust | Useful schema and retrieval status, but full schema is always embedded. Offer summary and exact schema expansion without losing generation/authority. | graph_describe, graph_missing | A04 |
| `bbox_project_graph_list` | all | adjust | Graph inventory retains authored/reflected counts and source plane; no list continuation or inventory cap. | graphs | A06 |
| `bbox_project_graph_list` | own | adjust | Graph inventory retains authored/reflected counts and source plane; no list continuation or inventory cap. | graphs | A06 |
| `bbox_project_graph_list` | published | adjust | Graph inventory retains authored/reflected counts and source plane; no list continuation or inventory cap. | graphs | A06 |
| `bbox_project_graph_validate` | all | adjust | Validation is distinct from description; valid sample is compact, invalid error arrays need pagination/exact detail. Outer status describes invocation, inner valid describes graph. | graph_validate; source | A06 |
| `bbox_project_graph_validate` | own | adjust | Validation is distinct from description; valid sample is compact, invalid error arrays need pagination/exact detail. Outer status describes invocation, inner valid describes graph. | graph_validate; source | A06 |
| `bbox_project_graph_validate` | published | adjust | Validation is distinct from description; valid sample is compact, invalid error arrays need pagination/exact detail. Outer status describes invocation, inner valid describes graph. | graph_validate; source | A06 |
| `bbox_provenance_export` | export | restrict | Legacy local notes mutation adapter; remote producers use owner-side export/transport. Chooser must identify locality and legacy scope before invocation. | source | A09 A10 |
| `bbox_provenance_export_plan` | continuation | restrict | Retain corpus-side generation-bound export computation for authenticated checkout clients; ordinary MCP project selector is insufficient authority. Readiness defect remains separate. | source | A09 |
| `bbox_provenance_export_plan` | first page | restrict | Retain corpus-side generation-bound export computation for authenticated checkout clients; ordinary MCP project selector is insufficient authority. Readiness defect remains separate. | source | A09 |
| `bbox_provenance_import` | import | restrict | Local notes import is distinct from corpus export planning; preserve explicit transport-authoritative refusal and owner lane. Do not imply arbitrary remote path access. | source | A09 A10 |
| `bbox_ref_size` | file refs | restrict | Useful preflight for known refs; indexed content works remotely, file refs require checkout authority. 500-ref cap is not an aggregate byte bound. | source | A06 A09 |
| `bbox_ref_size` | indexed refs | restrict | Useful preflight for known refs; indexed content works remotely, file refs require checkout authority. 500-ref cap is not an aggregate byte bound. | source | A06 A09 |

### [src/tools/knowledge.rs](../../../src/tools/knowledge.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_decide` | checkout-owner | retain | Durable commitment with rationale and supersession; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |
| `bbox_decide` | global/local | retain | Durable commitment with rationale and supersession; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |
| `bbox_forget` | checkout-owner | retain | Retire or supersede existing knowledge; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |
| `bbox_forget` | global/local | retain | Retire or supersede existing knowledge; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |
| `bbox_knowledge` | exact system memory | adjust | Recall keeps useful provenance and independent sidecars; primary/exact bodies and diagnostics are not fully paged. Empty query result carries repeated unrelated diagnostics. | knowledge_empty, knowledge_narrow, knowledge_invalid_mode | A04 A05 |
| `bbox_knowledge` | packet category | adjust | Recall keeps useful provenance and independent sidecars; primary/exact bodies and diagnostics are not fully paged. Empty query result carries repeated unrelated diagnostics. | knowledge_empty, knowledge_narrow, knowledge_invalid_mode | A04 A05 |
| `bbox_knowledge` | query | adjust | Recall keeps useful provenance and independent sidecars; primary/exact bodies and diagnostics are not fully paged. Empty query result carries repeated unrelated diagnostics. | knowledge_empty, knowledge_narrow, knowledge_invalid_mode | A04 A05 |
| `bbox_knowledge` | system_memory category | adjust | Recall keeps useful provenance and independent sidecars; primary/exact bodies and diagnostics are not fully paged. Empty query result carries repeated unrelated diagnostics. | knowledge_empty, knowledge_narrow, knowledge_invalid_mode | A04 A05 |
| `bbox_knowledge_link` | checkout-owner | retain | Append a typed relationship to existing knowledge; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |
| `bbox_knowledge_link` | global/local | retain | Append a typed relationship to existing knowledge; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |
| `bbox_learn` | checkout-owner | retain | Rendered standing rule with explicit operator approval; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |
| `bbox_learn` | global/local | retain | Rendered standing rule with explicit operator approval; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |
| `bbox_remember` | checkout-owner | retain | Cold durable fact without ambient rendering; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |
| `bbox_remember` | global/local | retain | Cold durable fact without ambient rendering; preserve explicit owner and admitted/applied/published distinction. Transport replay correctness is a separately tracked dependency, not this audit implementation. | source; prior milestone | A12 |

### [src/tools/mcp_surface.rs](../../../src/tools/mcp_surface.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_mcp_surface` | describe | adjust | Explains selected policy revision; rules lack continuation and matching_surface is only a display approximation for complex predicates. Exact packet body already has a reader. | surface_describe; source | A06 A10 |
| `bbox_mcp_surface` | list | adjust | Surface-packet discovery is useful but unpaged; list explicitly ignores project/surface selectors. Prefer action-specific schema to accepting inapplicable fields. | surface_list, surface_list_ignored | A06 A10 |
| `bbox_mcp_surface` | replay | adjust | Specialist policy replay is useful; return verdict/counts plus paged tools and optional rule detail. Unknown surface correctly yields deny, not an invocation error. | surface_default, surface_readonly, surface_unknown | A05 A06 |

### [src/tools/notes.rs](../../../src/tools/notes.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_note` | create | adjust | Task/thread side-channel signal is useful; supplied project goes through checkout write-scope even though notes are host-owned. Inspect owner-neutral admission before claiming remote availability. | source | A02 |
| `bbox_note_resolve` | batch | retain | Acknowledge versus address is useful; persistence error is surfaced. Batch cardinality/result bounds require a documented cap. | source | A06 |
| `bbox_note_resolve` | single | retain | Acknowledge versus address is useful; persistence error is surfaced. Batch cardinality/result bounds require a documented cap. | source | A06 |
| `bbox_notes` | exact/full | adjust | Summary pages are useful and bounded; full bodies have no byte continuation, so exact recovery can still exceed envelope cap. | notes; source | A04 A13 |
| `bbox_notes` | summary | adjust | Summary pages are useful and bounded; full bodies have no byte continuation, so exact recovery can still exceed envelope cap. | notes; source | A04 A13 |

### [src/tools/packets.rs](../../../src/tools/packets.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_apply` | all | adjust | Deterministic classification has a real policy role; all findings/consequents need byte bounds and expansion. Remove unsupported cheap-at-arbitrary-scale claim. | source; prior policy proof | A06 A10 A14 |
| `bbox_apply` | first | adjust | Deterministic classification has a real policy role; all findings/consequents need byte bounds and expansion. Remove unsupported cheap-at-arbitrary-scale claim. | source; prior policy proof | A06 A10 A14 |
| `bbox_audit` | all | adjust | Dataset agreement is useful but not proof of general correctness; dataset type and mismatch continuation are opaque/unbounded at the MCP boundary. | source; prior policy proof | A06 A10 |
| `bbox_audit` | first | adjust | Dataset agreement is useful but not proof of general correctness; dataset type and mismatch continuation are opaque/unbounded at the MCP boundary. | source; prior policy proof | A06 A10 |
| `bbox_compile` | compile | adjust | Permission routing is a real surviving packet consumer; authoring needs discoverable predicate/consequent shape and typed errors, not an opaque rubric blob and cold-memory prerequisite. | source | A10 |
| `bbox_packet_events` | query | adjust | Useful operational history, but latest-N cannot recover older rows and invalid op silently returns empty. Expose continuation and enum validation. | packet_events, packet_bad_filter | A03 A06 |
| `bbox_packet_gap` | record | retain | Packet AST expressiveness reports remain useful while packets enforce permissions; keep distinct from general substrate gaps and off readonly surface. | source; prior milestone | A10 |
| `bbox_packet_list` | detail | retain | Paged discovery with optional histograms and exact packet body reader; keep revision identity explicit. | packets; prior milestone | A13 |
| `bbox_packet_list` | exact revision | retain | Paged discovery with optional histograms and exact packet body reader; keep revision identity explicit. | packets; prior milestone | A13 |
| `bbox_packet_list` | summary | retain | Paged discovery with optional histograms and exact packet body reader; keep revision identity explicit. | packets; prior milestone | A13 |

### [src/tools/project_catalog.rs](../../../src/tools/project_catalog.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_project_attach` | attach | restrict | Requires broker-proven existing attachment authority; remote source onboarding is separate. Early truthful refusal is retained restriction, not remote attach support. | source; prior locality tests | A09 |
| `bbox_project_catalog_get` | aliases | retain | Distinct exact identity/detail planes; recorded attachment capability is not proof of live access. Detail pages are byte and epoch bounded. | catalog_epoch_rejection; source | - |
| `bbox_project_catalog_get` | attachments | retain | Distinct exact identity/detail planes; recorded attachment capability is not proof of live access. Detail pages are byte and epoch bounded. | catalog_detail; source; prior milestone | - |
| `bbox_project_catalog_get` | observations | retain | Distinct exact identity/detail planes; recorded attachment capability is not proof of live access. Detail pages are byte and epoch bounded. | catalog_detail; source; prior milestone | - |
| `bbox_project_catalog_get` | summary | retain | Distinct exact identity/detail planes; recorded attachment capability is not proof of live access. Detail pages are byte and epoch bounded. | catalog_detail; source; prior milestone | - |
| `bbox_project_catalog_list` | list | retain | Authoritative logical-project discovery with query, bounded summaries and epoch-aware continuation; preferred to compatibility roots. | catalog | - |
| `bbox_project_default_attachment` | clear | retain | Path-free catalog selection has a distinct role; default selection does not establish checkout access. | source; prior locality tests | A12 |
| `bbox_project_default_attachment` | set | retain | Path-free catalog selection has a distinct role; default selection does not establish checkout access. | source; prior locality tests | A12 |
| `bbox_project_detach` | detach | retain | Path-free catalog/attachment transaction is the precise operation; prefer over overloaded unregister when selecting an attachment. | source; prior locality tests | A12 |
| `bbox_project_promote` | apply | restrict | Whole-catalog migration requires administrator checkout authority; remote MCP refuses. Document actual offline command, not an invented owner transport. | source; locality audit | A09 |
| `bbox_project_publisher_advance` | advance attachment apply | retain | Explicit CAS publication boundary is useful; remote Ready candidate is supported. Keep dry-run, pointer uncertainty and served convergence distinct. | source; prior milestone | A12 |
| `bbox_project_publisher_advance` | advance attachment preview | retain | Explicit CAS publication boundary is useful; remote Ready candidate is supported. Keep dry-run, pointer uncertainty and served convergence distinct. | source; prior milestone | A12 |
| `bbox_project_publisher_advance` | advance candidate apply | retain | Explicit CAS publication boundary is useful; remote Ready candidate is supported. Keep dry-run, pointer uncertainty and served convergence distinct. | source; prior milestone | A12 |
| `bbox_project_publisher_advance` | advance candidate preview | retain | Explicit CAS publication boundary is useful; remote Ready candidate is supported. Keep dry-run, pointer uncertainty and served convergence distinct. | source; prior milestone | A12 |
| `bbox_project_publisher_advance` | establish attachment apply | retain | Explicit CAS publication boundary is useful; remote Ready candidate is supported. Keep dry-run, pointer uncertainty and served convergence distinct. | source; prior milestone | A12 |
| `bbox_project_publisher_advance` | establish attachment preview | retain | Explicit CAS publication boundary is useful; remote Ready candidate is supported. Keep dry-run, pointer uncertainty and served convergence distinct. | source; prior milestone | A12 |
| `bbox_project_publisher_advance` | establish candidate apply | retain | Explicit CAS publication boundary is useful; remote Ready candidate is supported. Keep dry-run, pointer uncertainty and served convergence distinct. | source; prior milestone | A12 |
| `bbox_project_publisher_advance` | establish candidate preview | retain | Explicit CAS publication boundary is useful; remote Ready candidate is supported. Keep dry-run, pointer uncertainty and served convergence distinct. | source; prior milestone | A12 |
| `bbox_project_publisher_bind` | bind | restrict | Attachment-bound administration has deliberate covered-project refusal; typed source binding is not a caller filesystem assertion. | source | A09 |
| `bbox_project_publisher_status` | get | adjust | Accepted/served state and CAS tokens are decision-critical; repeated health identities, attachment/watcher inventories and connector diagnostics belong behind bounded detail. | publisher | A05 A06 A10 |
| `bbox_project_scope_migrate` | apply | restrict | Attached scope migration is administrator-owned; remote MCP refuses and offline attestation differs. Do not present restriction as implemented remote functionality. | source; locality audit | A09 |
| `bbox_project_scope_migrate` | preview | restrict | Attached scope migration is administrator-owned; remote MCP refuses and offline attestation differs. Do not present restriction as implemented remote functionality. | source; locality audit | A09 |

### [src/tools/projects.rs](../../../src/tools/projects.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_project_eject` | apply | restrict | Local multi-stage administration with explicit partial outcome; remote protocol absent. Keep out of routine chooser, do not imply a working remote alternative. | source; locality audit | A09 A12 |
| `bbox_project_eject` | preview | restrict | Local multi-stage administration with explicit partial outcome; remote protocol absent. Keep out of routine chooser, do not imply a working remote alternative. | source; locality audit | A09 A12 |
| `bbox_project_init` | init | restrict | Creates checkout-owned .bbox identity/config; expose host ownership before invocation. Retain local/collector lane rather than probing remote paths. | source; locality audit | A09 |
| `bbox_project_list` | list | retire-candidate | Compatibility attached-root projection is unpaged and not a truthful complete logical catalog. Prefer catalog_list/get; retain bridge-mode use only with explicit compatibility disposition. | project_list; source | A06 A08 A09 |
| `bbox_project_register` | register | restrict | Local initialization differs from path-free catalog discovery; remote collector init/onboarding is supported lane, with existing enrollment gap carried forward. | source; locality audit | A09 |
| `bbox_project_rename` | apply | restrict | Legacy relocation spans owner stores; catalog pair-store relocation differs. Remote path mutation refuses. Preserve partial-stage reporting and actual locality prerequisite. | source; locality audit | A09 A12 |
| `bbox_project_rename` | preview | restrict | Legacy relocation spans owner stores; catalog pair-store relocation differs. Remote path mutation refuses. Preserve partial-stage reporting and actual locality prerequisite. | source; locality audit | A09 A12 |
| `bbox_project_unregister` | apply | adjust | Name hides mode-dependent unregister versus detach. Catalog retirement is a separate offline operation; missing-root retirement and stale consumer references remain explicit gaps. | source; locality audit | A08 A09 A10 |
| `bbox_project_unregister` | preview | adjust | Name hides mode-dependent unregister versus detach. Catalog retirement is a separate offline operation; missing-root retirement and stale consumer references remain explicit gaps. | source; locality audit | A08 A09 A10 |

### [src/tools/render.rs](../../../src/tools/render.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_absorb` | global | retire-candidate | Explicit no-op has no import capability; global still appends diagnostics and default demands project. Remove from served discovery; decide compatibility tombstone outside normal tools. | absorb, absorb_global | A08 |
| `bbox_absorb` | project | retire-candidate | Explicit no-op has no import capability; global still appends diagnostics and default demands project. Remove from served discovery; decide compatibility tombstone outside normal tools. | absorb, absorb_global | A08 |
| `bbox_bootstrap` | call | retire-candidate | Always returns typed retired error and replacement search arguments; keep compatibility refusal if needed, remove callable discovery entry. | source; existing refusal test | A08 |
| `bbox_lint` | get | adjust | Contradiction/staleness findings are useful; unpaged findings and wide diagnostic work need budgets or section selection. | source | A06 A14 |
| `bbox_render` | both | restrict | Host-owned render: remote global uses bro render global, managed project uses bound owner transport. Public chooser currently conceals these prerequisites. | source; prior milestone | A09 A10 |
| `bbox_render` | global | restrict | Host-owned render: remote global uses bro render global, managed project uses bound owner transport. Public chooser currently conceals these prerequisites. | source; prior milestone | A09 A10 |
| `bbox_render` | project | restrict | Host-owned render: remote global uses bro render global, managed project uses bound owner transport. Public chooser currently conceals these prerequisites. | source; prior milestone | A09 A10 |
| `bbox_review` | approve | retain | Operator-directed review mutation; exact ID plus explicit project owner for queued delivery. Return durable admission separately from publication. | source; prior milestone | A12 |
| `bbox_review` | list | adjust | Pending-review queue is useful but emits every complete content body; project is explicitly a mutation-only selector. Add bounded filterable discovery. | source | A04 A06 |
| `bbox_review` | reject | retain | Operator-directed review mutation; exact ID plus explicit project owner for queued delivery. Return durable admission separately from publication. | source; prior milestone | A12 |

### [src/tools/roadmap.rs](../../../src/tools/roadmap.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_roadmap` | create | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | default_template | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | delete | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | get | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | link | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | list | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | next | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | promote | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | render | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | repair_links | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | search | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | unlink | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |
| `bbox_roadmap` | update | retire-candidate | Existing graph-native retirement gap owns consumer/data disposition. These actions still implement real state, ranking and thread promotion; preserve records and design migration before removing. Render/templates have no unique execution value; list/full bodies are unpaged beyond top-N. | source; gap-56c74f23 | A08 |

### [src/tools/roster.rs](../../../src/tools/roster.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bro_brofile` | clear_provider_default | retain | Explicit default-account routing is useful and persistence errors are surfaced; account authority is not inferred from provider discovery. | source | A12 |
| `bro_brofile` | create | adjust | Persona lifecycle belongs here; project paths require owner-aware access and action-specific scope validation. Creation echoes complete lens/config. | source | A04 A09 A10 |
| `bro_brofile` | delete | adjust | Persona lifecycle belongs here; project paths require owner-aware access and action-specific scope validation. Creation echoes complete lens/config. | source | A04 A09 A10 |
| `bro_brofile` | get | adjust | Exact persona/config read is useful; full lens/defaults lack body pages, and get does not use scope to select the requested store. | source | A03 A04 A09 |
| `bro_brofile` | get_provider_default | retain | Small provider-keyed mapping has a bounded provider universe; keep absent default distinguishable from resolved account. | source | - |
| `bro_brofile` | list | adjust | Paged persona discovery is useful; invalid scope currently falls through to normal results. Project source ownership must be explicit. | brofiles, brofile_bad_scope | A03 A09 |
| `bro_brofile` | list_accounts | adjust | Account names/presence are useful and values are redacted; full unpaged map and silent config fallback require explicit availability. | source | A06 A11 |
| `bro_brofile` | list_provider_defaults | retain | Small provider-keyed mapping has a bounded provider universe; keep absent default distinguishable from resolved account. | source | - |
| `bro_brofile` | set_account | retain | Account configuration mutation returns redacted presence/name view, not environment values; explicit ownership remains daemon configuration. | source; existing response_view tests | A12 |
| `bro_brofile` | set_provider_default | retain | Explicit default-account routing is useful and persistence errors are surfaced; account authority is not inferred from provider discovery. | source | A12 |
| `bro_dashboard` | summary | retain | Fleet-wide bounded progress is distinct from single-task status; preserve worker-local availability and latest-preview limits. Observation grants no task ownership. | source; prior milestone | A11 |
| `bro_providers` | detail | retain | Provider/model chooser remains useful; default model count plus explicit inventory detail avoid account/binary conflation. | providers; prior milestone | - |
| `bro_providers` | summary | retain | Provider/model chooser remains useful; default model count plus explicit inventory detail avoid account/binary conflation. | providers; prior milestone | - |
| `bro_report` | call | retain | Milestone telemetry is a useful minimal execution primitive; bound text and preserve that it is not authoritative completion. | source | A12 |
| `bro_team` | create | retain | Team lifecycle stays external-orchestration data, with member caps and no automatic advisor. Project templates require deliberate restriction; dissolve may cancel tasks. | source; prior milestone | A09 A12 |
| `bro_team` | delete_template | retain | Team lifecycle stays external-orchestration data, with member caps and no automatic advisor. Project templates require deliberate restriction; dissolve may cancel tasks. | source; prior milestone | A09 A12 |
| `bro_team` | dissolve | retain | Team lifecycle stays external-orchestration data, with member caps and no automatic advisor. Project templates require deliberate restriction; dissolve may cancel tasks. | source; prior milestone | A09 A12 |
| `bro_team` | get | retain | Exact JSON pages with change-bound cursors provide a usable full-detail contract; preserve scope/project selectors across pages. | source; prior milestone | - |
| `bro_team` | get_template | retain | Exact JSON pages with change-bound cursors provide a usable full-detail contract; preserve scope/project selectors across pages. | source; prior milestone | - |
| `bro_team` | list | retain | Bounded team/member discovery has a distinct fan-out role; project template reads refuse missing owner transport. Explicitly live offsets. | teams, templates; source | A13 |
| `bro_team` | list_templates | retain | Bounded team/member discovery has a distinct fan-out role; project template reads refuse missing owner transport. Explicitly live offsets. | teams, templates; source | A13 |
| `bro_team` | roster | retain | Bounded team/member discovery has a distinct fan-out role; project template reads refuse missing owner transport. Explicitly live offsets. | teams, templates; source | A13 |
| `bro_team` | save_template | retain | Team lifecycle stays external-orchestration data, with member caps and no automatic advisor. Project templates require deliberate restriction; dissolve may cancel tasks. | source; prior milestone | A09 A12 |

### [src/tools/sessions.rs](../../../src/tools/sessions.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_embed_partitions` | list | adjust | Partition lifecycle remains useful; inventories lack continuation and scrub is omitted from action field prose. Preserve explicit age/route and apply choice. | source | A06 A10 A14 |
| `bbox_embed_partitions` | prune apply | adjust | Partition lifecycle remains useful; inventories lack continuation and scrub is omitted from action field prose. Preserve explicit age/route and apply choice. | source | A06 A10 A14 |
| `bbox_embed_partitions` | prune preview | adjust | Partition lifecycle remains useful; inventories lack continuation and scrub is omitted from action field prose. Preserve explicit age/route and apply choice. | source | A06 A10 A14 |
| `bbox_embed_partitions` | scrub apply | adjust | Partition lifecycle remains useful; inventories lack continuation and scrub is omitted from action field prose. Preserve explicit age/route and apply choice. | source | A06 A10 A14 |
| `bbox_embed_partitions` | scrub preview | adjust | Partition lifecycle remains useful; inventories lack continuation and scrub is omitted from action field prose. Preserve explicit age/route and apply choice. | source | A06 A10 A14 |
| `bbox_embed_status` | coverage | retain | Cheap health separates expensive opt-in scans/probes; keep unavailable distinct from zero, and route/deadline bounds visible. Explicit debug still needs disclosure review. | embed_status; source | A11 A14 |
| `bbox_embed_status` | debug | retain | Cheap health separates expensive opt-in scans/probes; keep unavailable distinct from zero, and route/deadline bounds visible. Explicit debug still needs disclosure review. | embed_status; source | A11 A14 |
| `bbox_embed_status` | diagnostics | retain | Cheap health separates expensive opt-in scans/probes; keep unavailable distinct from zero, and route/deadline bounds visible. Explicit debug still needs disclosure review. | embed_status; source | A11 A14 |
| `bbox_embed_status` | recall probe | retain | Cheap health separates expensive opt-in scans/probes; keep unavailable distinct from zero, and route/deadline bounds visible. Explicit debug still needs disclosure review. | embed_status; source | A11 A14 |
| `bbox_embed_status` | summary | retain | Cheap health separates expensive opt-in scans/probes; keep unavailable distinct from zero, and route/deadline bounds visible. Explicit debug still needs disclosure review. | embed_status; source | A11 A14 |
| `bbox_messages` | native | retain | Exactly one session or locator; count/body bounds and continuation preserve stored projection limits. Caller must not infer original-source completeness. | native_messages, thread_hint_messages; source | - |
| `bbox_messages` | retained-conversation | retain | Exactly one session or locator; count/body bounds and continuation preserve stored projection limits. Caller must not infer original-source completeness. | source; prior milestone | - |
| `bbox_reembed` | start | retain | Route embedding rebuild is distinct from orphan cleanup; chooser should name required route, cost and admission/completion observation. | source | A10 A12 A14 |
| `bbox_reindex` | queue | retain | Explicit index maintenance distinguishes admitted from completed; collected source publication is not a daemon checkout walk. Keep destructive empty-root authority explicit. | source; prior milestone | A10 A12 A14 |
| `bbox_reindex` | wait | retain | Explicit index maintenance distinguishes admitted from completed; collected source publication is not a daemon checkout walk. Keep destructive empty-root authority explicit. | source; prior milestone | A10 A12 A14 |
| `bbox_session` | get | retain | Session metadata has a distinct purpose from reading messages; concise chooser needs locator/provider ambiguity guidance. | source | A10 |
| `bbox_sessions_list` | list | adjust | Recent-session discovery is useful; explain defaults, maximum and empty/project semantics in schema. Empty observed page does not prove source collection is empty. | sessions | A10 |
| `bbox_stats` | get | retain | Cached index counts are distinct from source freshness; retain the stated 60-second cache limitation. | source | - |
| `bbox_topics` | get | retain | Cheap session term profile supports session selection; keep as specialized discovery, not required before message reads. | source | - |

### [src/tools/storage_gc.rs](../../../src/tools/storage_gc.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_storage_gc` | apply | retain | Useful managed cleanup and immutable bounded report reads; receipt is result detail, not an authorization token for later apply. Deletion remains independently requested; partial stages retained. | gc_preview; source; prior receipt proof | A12 |
| `bbox_storage_gc` | preview | retain | Useful managed cleanup and immutable bounded report reads; receipt is result detail, not an authorization token for later apply. Deletion remains independently requested; partial stages retained. | gc_preview; source; prior receipt proof | A12 |
| `bbox_storage_gc` | receipt candidates | retain | Useful managed cleanup and immutable bounded report reads; receipt is result detail, not an authorization token for later apply. Deletion remains independently requested; partial stages retained. | gc_candidates, gc_recovery | A12 |
| `bbox_storage_gc` | receipt deleted | retain | Useful managed cleanup and immutable bounded report reads; receipt is result detail, not an authorization token for later apply. Deletion remains independently requested; partial stages retained. | gc_preview; source; prior receipt proof | A12 |
| `bbox_storage_gc` | receipt errors | retain | Useful managed cleanup and immutable bounded report reads; receipt is result detail, not an authorization token for later apply. Deletion remains independently requested; partial stages retained. | gc_preview; source; prior receipt proof | A12 |
| `bbox_storage_gc` | receipt exclusions | retain | Useful managed cleanup and immutable bounded report reads; receipt is result detail, not an authorization token for later apply. Deletion remains independently requested; partial stages retained. | gc_preview; source; prior receipt proof | A12 |
| `bbox_storage_gc` | receipt full | retain | Useful managed cleanup and immutable bounded report reads; receipt is result detail, not an authorization token for later apply. Deletion remains independently requested; partial stages retained. | gc_preview; source; prior receipt proof | A12 |
| `bbox_storage_gc` | receipt packets | retain | Useful managed cleanup and immutable bounded report reads; receipt is result detail, not an authorization token for later apply. Deletion remains independently requested; partial stages retained. | gc_preview; source; prior receipt proof | A12 |
| `bbox_storage_gc` | receipt summary | retain | Useful managed cleanup and immutable bounded report reads; receipt is result detail, not an authorization token for later apply. Deletion remains independently requested; partial stages retained. | gc_preview; source; prior receipt proof | A12 |

### [src/tools/storage_health.rs](../../../src/tools/storage_health.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_storage_health` | files | retain | Storage diagnostics expose totals/top contributors or bounded relative-path inventory. Paths identify daemon storage, not recovery files. Full scan cost persists behind small output. | storage_health; source | A14 |
| `bbox_storage_health` | summary | retain | Storage diagnostics expose totals/top contributors or bounded relative-path inventory. Paths identify daemon storage, not recovery files. Full scan cost persists behind small output. | storage_health; source | A14 |

### [src/tools/storage_migration.rs](../../../src/tools/storage_migration.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_storage_migrate_legacy_edges` | apply | adjust | Specialist legacy extraction has migration value; dry-run does not resolve documented alias/path like apply and returns all project plans. Keep specialist pending consumer retirement review. | source | A03 A06 A09 |
| `bbox_storage_migrate_legacy_edges` | preview | adjust | Specialist legacy extraction has migration value; dry-run does not resolve documented alias/path like apply and returns all project plans. Keep specialist pending consumer retirement review. | source | A03 A06 A09 |

### [src/tools/threads.rs](../../../src/tools/threads.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_thread` | continue | adjust | Useful manual work continuity; string action schema hides required combinations and state transitions. Historical snapshots and graph refresh must distinguish pending/complete outcomes. | source | A10 A12 |
| `bbox_thread` | get | adjust | Continuity read needs latest checkpoint plus paged exact notes, sessions and edges; current complete growing history has no continuation. | thread | A04 |
| `bbox_thread` | link | adjust | Useful manual work continuity; string action schema hides required combinations and state transitions. Historical snapshots and graph refresh must distinguish pending/complete outcomes. | source | A10 A12 |
| `bbox_thread` | open | adjust | Useful manual work continuity; string action schema hides required combinations and state transitions. Historical snapshots and graph refresh must distinguish pending/complete outcomes. | source | A10 A12 |
| `bbox_thread` | promote | adjust | Useful manual work continuity; string action schema hides required combinations and state transitions. Historical snapshots and graph refresh must distinguish pending/complete outcomes. | source | A10 A12 |
| `bbox_thread` | rename | adjust | Useful manual work continuity; string action schema hides required combinations and state transitions. Historical snapshots and graph refresh must distinguish pending/complete outcomes. | source | A10 A12 |
| `bbox_thread` | resolve | adjust | Useful manual work continuity; string action schema hides required combinations and state transitions. Historical snapshots and graph refresh must distinguish pending/complete outcomes. | source | A10 A12 |
| `bbox_thread_list` | list | retain | Bounded summary pages support list-before-open; mutable offset order is explicitly live rather than immutable snapshot pagination. | threads | A13 |

### [src/tools/tool_calls.rs](../../../src/tools/tool_calls.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_tool_calls` | query | retain | Unique indexed tool-call filters; candidate continuation can be empty and nonterminal, explicitly documented. Typed kind/time validation. | history | A13 |

### [src/tools/transcripts.rs](../../../src/tools/transcripts.rs)

| Tool | Action / branch | Disposition | Contract / required adjustment | Evidence | Findings |
| --- | --- | --- | --- | --- | --- |
| `bbox_cite` | claim | retain | Claim-oriented transcript retrieval; retain only as origin evidence with confidence, not proof that a retrieved statement is true. | source | - |
| `bbox_context` | native | retain | Opaque locator and event offset recover indexed context; native completeness is explicitly a projection. No caller filesystem access. | native_context, thread_hint_context; source | - |
| `bbox_context` | retained-conversation | retain | Opaque locator and event offset recover indexed context; native completeness is explicitly a projection. No caller filesystem access. | source; prior milestone | - |
| `bbox_corpus_search` | query | retain | Compatibility id/text contract has a real consumer in bro-harness/src/mcp.rs; keep separate from richer search. Cold empty-index call can trigger indexing. | source | A14 |
| `bbox_discover_seed_entities` | query | retain | Adds notable edge counts to hybrid seeds; useful graph entry point, not an identical alias. Requires complete graph readiness. | source | A10 |
| `bbox_hybrid_search` | debug | retain | Mixed typed seeds, identity and degradation belong in default; ranking telemetry belongs in debug. Top-k is a selection, not an exhaustive list. | prior milestone; opening sequence | A05 |
| `bbox_hybrid_search` | default | retain | Mixed typed seeds, identity and degradation belong in default; ranking telemetry belongs in debug. Top-k is a selection, not an exhaustive list. | prior milestone; opening sequence | A05 |
| `bbox_search` | fulltext | adjust | Ranked corpus recall; return a type-correct expansion handle. Current non-transcript hits receive transcript-only hints. | search, search_native | A07 |
| `bbox_search` | smart | adjust | Ranked corpus recall; return a type-correct expansion handle. Current non-transcript hits receive transcript-only hints. | search, search_native | A07 |

## Completion boundary

The current served-name and action/branch inventory is recorded, with an explicit
role/disposition for every row. The outstanding findings above remain open.
Source-only mutation, large-input and confidentiality cases are not upgraded to
verified by the existence of past global green gates. Runtime fixes should be
small changes justified by these caller-facing findings, followed by focused
fixture/served-contract validation. The audit thread remains active.


## Integration review evidence

The operator-authorized caller-contract corrections supersede the old byte-for-byte
knowledge presentation freeze for three bridge fixture rows: all_knowledge,
own_knowledge, and published_knowledge. Entry selection and provenance stay the
same. Memory signposts now describe bounded exact reads; visibility diagnostics
move from an unbounded repeated text list into the structured diagnostic projection
with explicit exact recovery. The reviewed fixture update covers only those three
rows. It does not authorize changing registry authority or hiding tombstones.

The first complete integration gate at 9c82824b compiled and ran 6,667 tests:
6,629 passed, 38 failed, 24 were skipped under the mid-cycle profile. This is
failure evidence, not acceptance. Corrections include missing packet pagination
metadata, knowledge map insertion, complete review envelope bounds, fixture
continuations, and source/documentation chooser parity. Protocol replay and native
observation regressions passed earlier at 8c01614d. No deployed proof is claimed
for these integration changes.


The requested Flash option uses `glm-5.3-flash` in the shared provider catalog
and preserves that slug on native requests. Its explicit 1M context metadata
follows the [Z.AI model guide](https://docs.z.ai/guides/vlm/glm-5.3-flash).
The provider's existing flagship default remains selected unless the caller pins
Flash. Native request support still needs a successful provider probe; code-path
and catalog tests alone do not prove endpoint admission under a quota cap.


### Verified integration checkpoint

Code revision `9dbfb74f` includes the ten dispatched implementation drafts and
orchestrator corrections. The full-profile workspace suite passed at `83810c02`
(6,679 passed, 19 skipped). Workspace clippy and the concurrency lint pass after
moving brofile store operations onto the blocking pool at `9dbfb74f`. An extra
`clippy --workspace --all-targets` run found existing test-fixture filesystem lint
errors in `crates/fleetd/src/workspace.rs`; that broader gate is not green.

A throwaway catalog-mode daemon built at `9dbfb74f` served 109 tools. Its 58
HTTP/MCP calls exercised these concrete cases with synthetic isolated state:

| Contract | Observed result |
| --- | --- |
| `bro_providers(provider="glm")` | Advertises `glm-5.3-flash`; flagship default remains `glm-5.3`. |
| `bro_when_any` and `bro_when_all`, unknown-only selection | Both refuse the selection; neither reports empty success. |
| Brofile/MCP scope typos, action-mismatched MCP field, invalid partition action and thread detail | Explicit error responses. |
| Account write, summary and exact inventory | Values are redacted; exact inventory reconstruction retains the account identity. |
| Thread summary, history and exact note continuation | Summary omits the large body; continuation reconstructs the complete 16,522-byte Unicode/escaped note. |
| Schema, unmatched knowledge and dashboard reads | Successful bounded responses against the isolated daemon. |

The largest complete MCP result in those calls was 15,245 serialized bytes.
This measurement covers those fixtures, not every possible input. The isolated
daemon was stopped after the probe. These are built-runtime observations, not
production deployment evidence.

The native-search request, replay and durable-log regressions cover canonical
Anthropic blocks, GLM's generic assistant results, and MiniMax's request schema
variation. They establish those repaired protocol behaviors. They do not establish
why the provider initiated placeholder queries. The actual GLM 5.3 probe at
20:49:11 UTC and Flash probe at 20:57:24 UTC on 2026-09-06 both returned HTTP 429,
code 1308, with a five-hour quota-cap message. The returned reset timestamp had no
timezone. A successful live Flash turn remains unverified; this is not a claim of
permanent provider unavailability.

### Final integration verification

At `e7985322`, the full-profile workspace gate passed all 6,684 tests with
19 skipped; workspace clippy, the concurrency lint and pinned formatting also
passed. The expanded isolated HTTP/MCP probe made 159 successful checks across
109 served tools, including expected error cases. Its largest complete result
was 15,245 bytes. The probe exercises a subset of action combinations; neither
the catalog count nor test count represents exhaustive live execution.

The follow-up findings at the earlier checkpoint are now implemented and checked:

| Follow-up | Implemented contract | Evidence |
| --- | --- | --- |
| Partition outcomes and mappings | Exact preview inventory; candidate/skipped rows follow the visible page with global totals; oversized apply batches refuse before effects and accept a route selector. Failure stops the batch and reports completed/unattempted counts. | Workspace regressions reconstruct candidate pages and huge fields, prove pre-effect refusal, route selection and partial failure. No production prune/scrub was run. |
| Agent summary inventories | Metadata and complete computed-summary body planes; default oversized filter/history/warning fields retain counts and exact hints. URL sources omit credential/query details. | Large-filter and metadata regressions; isolated installation and full merged-filter/metadata recovery. |
| MCP server identities | Exact redacted list inventory recovers full names; ordinary list budgets encoded output and retains continuation. | Escaped-name/filter fixture plus isolated oversized server-name reconstruction. |
| Packet result bodies | Exact deterministic apply/audit result pages bound to input and result; exact reads add no observation event. Large summary values expose the result reader. | First-consequent, audit-value and changed-input regression; isolated apply/audit full recovery. |

Reproduce the isolated probe with `scripts/probe-mcp-survivor.py --repo <checkout>`
after building `blackboxd` and `blackbox` for that host. In a build lane, invoke
Python inside its pod. The script creates synthetic state, starts one throwaway
daemon, records RPC envelopes and stops that daemon. It does not select production
state or dispatch a provider turn. Measured results are checked in as
[mcp-survivor-integration-verification.json](mcp-survivor-integration-verification.json).

Native placeholder-search invocation cause remains open in `gap-900d052c`.
The replay, observation and MiniMax request-shape defects are repaired and tested;
a successful provider-level probe is still blocked by the observed quota cap.
Preserve the enabled search capability while gathering evidence. A query deny list
would not establish or repair the protocol cause.

### Native search scrutiny follow-up

The saved GLM exchanges contain typed `server_tool_use` calls named
`web_search_prime`, paired with provider-owned generic assistant `tool_result`
blocks. In one exchange, both searches and the eventual client `tool_search`
call belong to the same assistant response. The harness did not dispatch a
client tool between those searches. These are provider-native executions,
not merely rendered text, but the snapshots do not preserve the exact request
tool catalog that preceded them.

Both incidents followed process resumes. Successful `tool_search` receipts
before those resumes had activated `mcp__blackbox__bro_report`. Registry
activations were memory-only: session persistence retained the conversation's
loaded-tool promises but omitted the activation set. The native binary's
synthetic endpoint probe reproduced the defect directly: `file_read` was
absent initially, present after activation, and absent again in the first
request after resume. This proves a discoverability defect; it does not prove
why the model selected a placeholder web query.

`a9825318` persists activation names in existing session side-state. Legacy
sessions recover names from successful, paired `tool_search` results in the
durable event log, including receipts no longer present after compaction.
Restoration intersects with the current filtered catalog and uses current
schemas. A native process probe passed normal resume, legacy recovery,
permission removal, and explicit empty activation state. Native search was
present in all six captured requests. Reproduce with
`python3 scripts/probe-tool-search-resume.py --binary <bro-harness>`; it uses
only a local scripted endpoint and isolated synthetic state.

A separate live allocator preview explained Flash's `no_candidates` response:
the GLM lane had `quota_confidence=runtime_rate_limit`, `quota_status=exhausted`,
and an already expired cooldown. The hard exclusion ignored expiry. The
allocator correction permits a fresh attempt after that runtime cooldown,
scores quota as unknown, and retains the historical receipt. Current summaries
mark the observation expired instead of presenting old utilization as current.
Authoritative quota-probe exhaustion and credential failures still refuse.

The direct Flash probe at `2026-09-06T21:46:50Z` returned HTTP 429/code 1308
with a five-hour cap. The provider response and the allocator's stale exclusion
are separate evidence. Neither the local scripted probe nor expired cooldown
establishes successful live GLM admission. `gap-900d052c` remains open for
provider query-selection attribution; `gap-ff52b07c` tracks activation recovery
and `gap-6451641a` tracks the allocator expiry defect.

At `b015e051`, all 6,687 full-profile workspace tests passed (19 skipped),
along with workspace clippy, pinned formatting and the concurrency lint.
Two existing migration lock-contention tests hit their three-second completion
deadline under parallel suite load. The final full run used a temporary copy of
the repository's nextest configuration with those two tests assigned
`threads-required = "num-test-threads"`; both passed with their assertions and
timeouts unchanged. Nextest documents this [exclusive scheduling setting](https://nexte.st/docs/configuration/threads-required/).
All 165 isolated HTTP/MCP checks passed, including active cooldown refusal,
expired runtime quota admission, unknown current quota summaries, and continued
authoritative-quota refusal. The largest MCP result remained 15,245 bytes.

The signed native harness is installed. Cage image `b015e051edf2` deployed as
digest `061b334e04815801cbe146b7285cee7ca29a01730fa07ac83974e5a4c5f084a1`;
only the blackboxd deployment changed. Live allocation then admitted Flash, and
task `d52fa41b-fe00-48ab-9991-0ddf37a4b922` reached the provider and failed in
five seconds with a fresh HTTP 429/code 1308. That observation installed a new
active cooldown. This proves the expired-exclusion recovery path, not successful
provider execution or resolution of placeholder-search initiation.

Callable retirement and locality restrictions retain their explicit dispositions
in the action matrix. That matrix records the original audit; the integration
checkpoints above record subsequent fixes and their actual verification scope.
The audit thread remains active for the native-search investigation and the
recorded retirement/restriction dispositions. No production mutation or retirement
result is inferred from isolated fixture checks.
