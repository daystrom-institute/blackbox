---
title: "MCP response and contract audit"
kind: design
corpus: blackbox-design
lifecycle: partial
topic:
  - surfaces
  - mcp
brief: "Source-backed triage of tool usefulness, caller contracts, response brevity, and filesystem locality."
---

# MCP response and contract audit

Snapshot: 2026-09-05, source commit `919b8f4a`. This is an adjustment backlog,
not an implemented contract. The preceding commit fixes brofile list shaping,
provider/model discovery, context telemetry, and GPT-6 Astra registration. It
has been pushed; those changes were not deployed during this audit.

The main recommendation is to make each tool return the smallest sufficient
answer to its caller's decision. Persistence structs, execution traces, and
human renderings are inputs to that answer, not automatic response DTOs.
Filesystem colocation is no longer a valid default assumption.

## Scope and evidence

Static inventory covers all **190 `#[tool(name=...)]` declarations** under
`src/tools`, grouped in the [coverage inventory](mcp-audit-coverage.md).
The connected session exposes **106** Blackbox tools. These are different
counts: a session's filtered catalog is not the complete server surface.
Handlers, parameter definitions, response helpers, selected domain DTOs, tool
documentation, and existing gaps were inspected. Five additional bounded,
read-only probes measured representative responses. This is complete inventory
and family-level triage, with deeper source review of the findings below; it is
not a claim that every action of every tool was exercised in production.

Grounding includes decision `knowledge:bb846aad` (four guidance surfaces),
spill-introduction commit `45a54fab18bfea008e81cd105038c92b410366af`, the
[existing locality audit](../../daemon-runtime/mcp-surface-locality-audit.md),
and the [target-surface design](mcp-2026-07-28-target-surface.md). The inspected
decision and spill commit were bundled through `bbox_bundle_evidence`.
Local source is authoritative for proposed implementation changes. Live
responses describe the deployed version, which predates the preceding fixes.
No private project names, paths, response bodies, or credentials are retained
in this report.

### Measurements

UTF-8 byte counts below measure the concatenated text content. The structured
column measures compact JSON serialization of `structuredContent`, separately;
it is not an estimate of what every model client inserts into context. Field
sizes also use compact JSON, so they do not sum to pretty-printed text sizes.
These are individual observations, not percentiles or corpus-wide benchmarks.

| Read-only probe | Text bytes | Structured bytes | Signal |
| --- | ---: | ---: | --- |
| `bbox_describe_schema()` | 16,426 | absent | `vertex_types` 6,611; embedded rendered `text` 1,996; consultant catalog 684 despite compact orientation |
| `bbox_hybrid_search(query="MCP response cap", project=<this project>, limit=3)` | 12,032 | 9,217 | `results` 1,739; `vector_status` 4,922; rendered `text` 1,825; `next_steps` 664 |
| `bbox_gaps(project=<this project>, limit=3, json=true)` | 11,360 | 10,679 | Full `rows` 7,393; `diagnostics` 3,006; shared provenance table 242 |
| `bbox_notes(project=<this project>, limit=3)` | 665 | absent | Bounded previews are already useful |
| `bbox_packet_list(limit=3)` | 1,542 | absent | Compact summaries already work; continuation is missing |

Earlier in this work, the deployed `bro_brofile(action="list")` returned an
oversize envelope reporting 93,041 bytes against an 81,920-byte cap. The new
summary page fixes that producer. It does not fix the general cap contract.

## Expanded review criteria

| Criterion | Question to answer for each tool/action |
| --- | --- |
| Useful decision | What decision or action becomes possible after this call? Does another tool already provide the same capability with comparable cost and authority? |
| Choosability | Can a caller distinguish this tool from its neighbors using its chooser description alone? Are cold doctrine and implementation history kept out of discovery? |
| Valid inputs | Do enums, required combinations, units, defaults, bounds, and examples agree with validation? Do typos fail explicitly rather than widening a query or choosing expensive detail? |
| Default sufficiency | Are identity, answer, applicable state, evidence needed to trust it, and a usable next step present without expansion? |
| Brevity | Would deleting this field change the caller's decision? If useful only for diagnosis, why is it in a healthy default response? |
| Bounded growth | Are row count, nested fan-out, string bodies, and total bytes bounded? A `limit` on rows alone is insufficient. Does producing a small result still perform an unbounded scan? |
| Recoverability | Can omitted information be fetched through a documented, authorized continuation or exact read? Is ordering deterministic, and are stale cursors detected? |
| Truthfulness | Are empty, unavailable, stale, partial, queued, applied, published, and completed distinguishable? Can an omitted field be mistaken for false, zero, or success? |
| Locality and authority | Who owns every path or locator? Where does it resolve? Does it identify data, prove authority, or invite an impossible filesystem operation? |
| Repeated-call cost | How much identical metadata is repeated across search, inspection, polling, and a fan-out of agents? Do full results recur on every status poll? |
| Disclosure | Are returned details within the request's scope? Are credentials and unrelated project diagnostics excluded even from debug output? |
| Migration | Which clients, workflows, templates, and tests consume the existing fields? Can the change preserve semantics without maintaining two conflicting contracts indefinitely? |

Keep the existing four-surface guidance model: schema validates; chooser prose
explains when to call; managed tool documentation explains operation; ambient
scope supplies dispatch-specific facts. A system memory can explain a complex
workflow, but should not be necessary to discover required parameters, output
meaning, or where a returned handle can be read.

## F01. Remove accidental server spill as a response strategy

**Priority: P1. Confirmed in source and live output.**

[`src/server/response.rs`](../../../src/server/response.rs) explicitly assumes
that every client of a localhost daemon has file-read tools. It creates files
under the daemon's state directory, prunes them after seven days on subsequent
spills, and tells callers to read `spilled_to` with file tools. This is neither
a client-accessible continuation nor an intentional export.

The helpers also have two correctness problems: `run_with_structured` and its
blocking twin cap text and then attach the uncapped structured value; and a
JSON oversize error produced inside `ok_text`/`ok_json` remains an MCP success.
Metrics log pre-cap text size rather than the complete result envelope.

First principles:

- The producer owns query cost, cardinality, projection, and a bounded answer
  with honest completeness. It knows enough about the domain to paginate
  without severing a record or evidence chain.
- The server transport owns a final size safeguard and an unambiguous failure
  if a producer violates its contract. It must account for every emitted
  representation. A generic serializer cannot invent a correct domain cursor.
- The client owns model-context budgeting, display, and any local persistence
  it chooses for received results. Server persistence is unnecessary for this.
- An export is a separate, deliberate operation. If Blackbox owns an export
  artifact, its API needs authorization, retention, and bounded retrieval. A
  raw filesystem path is not that API.

**Adjustment:** remove automatic dump creation and the filesystem recovery
hint from ordinary tool responses. Repair oversized producers with summary
pages and exact reads. Keep a small typed oversize failure as the final guard,
with the actual tool's supported narrowing options and a correct error flag.
Do not claim a clipped prefix is complete. An exact read of a large body needs
its own bounded body cursor or deliberate export path.

The earlier proposal to add `blackbox://spill/<id>` would make existing spills
retrievable, but would also preserve accidental result storage as an API. It
is an optional migration bridge, not the recommended destination. Any bridge
needs a removal plan; do not add a permanent spill tool merely because a dump
file already exists. Update [gap-990143f1](../../../.bbox/gaps/gap-990143f1.json)
and the older target-surface proposal when this direction is adopted.

## F02. Restore transcript ingestion and remote drill-down together

**Priority: P1. Ingestion gap recorded; filesystem drill-down confirmed in source.**

[gap-e40feb8f](../../../.bbox/gaps/gap-e40feb8f.json) records the missing native
provider transcript ingest lane after the cage move. It is not evidence that
all historical or connector conversation data is absent today. Reindexing
cannot collect sources the daemon does not receive.

[`bbox-corpus-index/src/index/search.rs`](../../../crates/bbox-corpus-index/src/index/search.rs)
still reads `ContextParams.file_path` directly for native transcripts;
`messages` resolves session files and reads them on the daemon. Its Slack
locator branch correctly uses the conversation landing store. The schema
still calls `file_path` a JSONL file path even though `slack:` is accepted.
Native `session`/`sessions_list` also consult filesystem metadata paths.

**Adjustment:** collector-owned native ingestion plus backfill, then a
source-neutral stored transcript/message locator for search, cite, context,
session, and messages. All follow-ups must resolve through the receiving
store or an explicit source-owner protocol. Preserve provider, role,
chronology, source generation, and truncation; return ingestion freshness or
unavailability when it limits a search. Validate an entire remote search to
context/messages chain, not merely a successful index count.

## F03. Every path-bearing contract needs an owner and disposition

**Priority: P1 for broken operations; P2 for presentation.**

A path-like string can be a useful project-selector alias or a relative source
location. It should not silently become daemon filesystem authority. Nor is
daemon-internal file I/O itself a defect. The boundary defect is exposing or
consuming a path as if caller, checkout owner, execution worker, and daemon
shared one filesystem.

| Surface | Current concern | Proposed disposition |
| --- | --- | --- |
| `bbox_context`, `bbox_messages`, native session reads | Raw native transcript paths | Stored source/message handles; F02 |
| `bbox_artifact_install` | Chooser offers a local JSON path or URL; local means server-side | Typed inline artifact or explicit remote source; host CLI can upload local files. Document source ownership and installation authority |
| `bbox_artifact_list` | `ArtifactListEntry.path` exposes storage location | Stable artifact ref in summaries; storage location only in scoped operator diagnostics |
| Project register/init/attach/detach/promote/rename/eject/scope/default-attachment operations | Several routes still probe checkout paths from the daemon; existing locality audit records broken and partially applied cases | Route proofs and file mutations through the checkout owner; refuse before side effects when no working lane exists; give an actually usable owner-side command |
| `bbox_project_catalog_get` | Explicit `host_local_attachments` is honest but verbose in routine identity lookup | Default identity/scope/state; opt-in attachment diagnostics labeled with their owner. Keep epoch when needed for a subsequent guarded mutation |
| `bbox_render`, `bbox_bootstrap` | Local rendered files and instruction inputs | Render plans/completions through checkout owner; global content through the existing client pull flow; bootstrap must obtain inputs from their owner |
| `bbox_roadmap` | `write_path` and `template_path`, including configuration-derived defaults | Resolve retirement first (F10); any retained render returns content or an explicit owner-applied plan |
| `bbox_blame`, provenance export/import, `bbox_ref_size(file:...)` | Old filesystem-shaped expectations coexist with newer checkout-local/proof lanes | Publish one clear supported lane per action. Preserve the generation-bound provenance export plan; use indexed refs for indexed data |
| `work_smart_read`, `work_bash`, `work_git_*` | Execution and filesystem operations in daemon MCP adapters | Harness/worker capability, or explicit owner-bound execution. Keep `work_*` restricted to workflow agents; do not expose a remote shell that guesses whose checkout is meant |
| `bro_mcp`, project brofile/team operations | Scope resolution and stores can refer to daemon-local configuration | State which owner is configured; use an owner-directed mutation where the intent is to configure a harness or checkout |
| `bro_providers` | `found`, `bin`, and `path` describe daemon binary lookup | Catalog discovery should not imply worker availability. Omit these from ordinary discovery, or use worker-derived availability with its observation scope |
| Task `transcriptLocation`/`transcriptCursor`, dispatch `cwd` | Execution-owner coordinates may be read as caller paths | Keep opaque task/transcript handles; expose raw coordinates only with owner semantics. `cwd` selects a worker's workspace, not an arbitrary daemon directory |
| Storage/doctor/GC reports | Paths can be useful evidence about server-owned storage | Operator detail only, explicitly server-owned; deletion must use the service's guarded operation, not a caller file tool |

Follow the [existing locality audit](../../daemon-runtime/mcp-surface-locality-audit.md)
for individual administration routes rather than reopening already established
owner-side lanes. Separately fix onboarding's missing transport enrollment
([gap-78c4fa64](../../../.bbox/gaps/gap-78c4fa64.json)); eliding its repeated
diagnostics must not conceal the unresolved underlying state.

## F04. Search responses should return evidence, not vector fleet telemetry

**Priority: P1. Confirmed and measured.**

[`HybridSearchResponse` and `HybridResult`](../../../crates/bbox-mcp-tools/src/mcp_tools/hybrid_search.rs)
carry rendered text, next-step prose, results, per-route queues and partition
metrics, fusion scores, and degradation. The live three-result observation
devoted more bytes to `vector_status` than to the results themselves.

**Default:** ordered refs, labels, bounded excerpts, source identity needed to
disambiguate, source generation/visibility where needed to assess freshness,
and concise result-affecting degradation. Ordering usually replaces explicit
rank; raw `score`, `base_score`, and fusion `sources` belong in ranking debug.
Keep a useful ref-bearing follow-up cue once, not duplicated in every rendering.

**Expansion:** ranking explanations and relevant vector routes through
`debug` or the existing embedding health surface. Do not return global queue
and HNSW metrics on every query. Preserve whether vector search was requested,
actually used, or fell back, without requiring the caller to interpret queue
internals. Give `include_vectors` an explicit meaning in schema documentation:
it selects retrieval behavior, not a request to return raw vector arrays.

Remove the embedded `text` mirror when introducing the compact structured DTO.
If transport compatibility requires a text representation as well, render the
same bounded answer there; do not embed another complete prose copy inside it.

## F05. Gap and knowledge recall need purpose-built list DTOs

**Priority: P1. Confirmed and measured.**

[`bbox_gaps`](../../../src/tools/gaps.rs) serializes `GapNote` directly. Its
record includes wanted capability, evidence, workaround, notes, lifecycle,
and task/session provenance. A three-row page can already exceed 11 KB.
`GapStore::query` defaults to 50 and has no maximum clamp or continuation.
Both [`bbox_knowledge`](../../../src/tools/knowledge.rs) and gaps append every
degraded carrier diagnostic before finalizing a scoped response.

**Gap list:** id, title, kind/domain, impact, blocking level, resolution,
updated time, dedupe key, supersession when relevant, and a short capability
preview. Full evidence, fallback, narrative notes, and session attribution
belong to exact-id detail. Preserve enough text to dedupe correctly; a title
alone is inadequate. Existing exact-id lookup provides the expansion route.

**Knowledge recall:** ref, title/category, useful excerpt, scope/visibility,
and minimal provenance that distinguishes published from provisional state.
Retain bounded packet/memory signposts with exact expansion handles. Do not
change ranking or cross-store recall semantics merely to shorten output.
Separate the primary result limit from sidecar limits and document each.

**Diagnostics:** default only to issues that affect the selected scope or
returned rows. A query over all scopes can summarize affected scope counts
and offer detail. Do not print unrelated project names/errors on a scoped
lookup. Keep shared `built_from` references where they carry actual authority
or freshness, while eliding empty maps and repeated textual stamp tables.
Link to [gap-f29ee57e](../../../.bbox/gaps/gap-f29ee57e.json) rather than creating
another gap about gap-list overflow.

## F06. Standardize bounded list/detail contracts

**Priority: P1 for full records; P2 for already-small catalogs.**

Confirmed examples:

- `bbox_thread_list`, `bbox_artifact_list`, and `bbox_project_catalog_list`
  lack pagination parameters. Their stores can grow independently of a query.
- `bro_agent_list` and `atom_list` return all summaries if `limit` is omitted;
  an explicit limit is applied without a maximum or continuation.
- `consultant_proposals_list` and its Badgey equivalent return complete
  proposals, including drafts and events, without a page control.
- `bro_team(list_templates)` serializes templates; `bro_arc_status()` returns
  all snapshots and pending waits. Event/reaction/webhook/poller/cron lists
  need summary projections rather than automatic serialization of specs.
- `bbox_packet_list` has a useful summary and maximum limit, but truncates
  before reporting `count`, without a next page or total-match indication.
- Notes have useful previews and exact detail, but no continuation. A list
  cap is not equivalent to a browseable collection.

**Adjustment:** bounded summaries by default, a documented exact read, and
continuation for collections larger than one page. Use a consistent page
vocabulary such as `items` and optional `next_cursor`; provide a total only
when useful and cheap. Prefer a generation-aware cursor for changing catalogs
or logs; offsets are acceptable when ordering and mutation behavior are
explicit. Bound nested histories and individual string fields too. A field
that is essential to select an atom (purpose, inputs, cost class) earns its
space; installation timestamps and complete supersession chains usually do
not. Do not impose identical row schemas on unrelated domains.

## F07. Schema should prevent guesswork and silent fallback

**Priority: P1. Specific failures confirmed in source.**

`bro_dashboard` parses unknown provider/status strings into no filter; an
unknown team similarly yields no team filter. A typo can therefore return a
broader roster. Inspect's `property_mode` silently becomes `smart` on an
unknown value, while bundle's becomes `full`, potentially increasing output.
`bbox_context` documents only the filesystem branch of a dual locator API.

**Adjustment:** typed enums or explicit validation; reject unknown selectors;
document required alternatives such as `session_id` versus transcript locator
and `to` versus `to_type`. Separate exact identity from substring search where
both are useful. Do not let “unknown” mean “all.”

Multiplexers (`bro_brofile`, `bro_team`, `bbox_thread`, `bbox_roadmap`,
`bbox_mcp_surface`, `bro_mcp`) need action-specific required fields, examples,
side effects, and response shapes. Prefer validated tagged action inputs where
supported; split tools only when chooser clarity or authority differs enough
to justify another catalog entry. Opaque `spec: Value` inputs in workflow,
reaction, and integration installation need a discoverable concrete schema,
not knowledge of Rust enum JSON conventions hidden in a memory.

Update chooser descriptions and `TOOL_DOCS` together. Existing parity tests
detect text drift, not whether an example calls the real contract successfully.

## F08. Separate routine status from deliverables and execution traces

**Priority: P2, with P1 for misleading availability/state.**

[`task_result_json_from_inner`](../../../src/orchestration/mod.rs) combines
state, latest assistant text, accounting, reports, transcript coordinates,
context, supervision, and failures. Status and wait families reuse it.
Existing status budgets and omission of healthy terminal supervision are
useful precedents. `bro_dashboard` still returns full reports and an agent
metrics rollup derived before its task page is truncated; `limit` therefore
does not bound every response dimension.

**Routine status:** task identity, execution state, last meaningful activity,
blocker or recoverable failure, and whether a deliverable is available. A
completed wait should return the deliverable; repeated polls should not replay
its full body. Use an explicit result/detail option or retrieval operation,
with a body cursor for large deliverables. Preserve `structuredExit` for
workflow callers that consume it, rather than making them parse escaped text.

**Trace/detail:** cache accounting, full reports, raw event tails, allocator
score components, workflow node visits, atom effect history, and transcript
transport coordinates. Keep actual effects and unresolved obligations in the
default atom/workflow summary where they affect the next action.

The preceding context fix is semantically necessary. The next brevity pass
can remove repeated interpretation prose from every row once the chooser/doc
contract carries it clearly. Never recreate a “remaining work budget” from
context occupancy, and never represent an unavailable measurement as zero.

## F09. Configuration DTOs must not echo credential values

**Priority: P1. Serialization paths confirmed; no credential values probed.**

`bro_brofile(action="list_accounts")` serializes the accounts map and `set_account`
echoes `env`. [`bro_mcp(action="get")`](../../../src/orchestration/mcp.rs)
serializes `McpServerConfig`; `SecretString::Plain` serializes as its literal
string. These are persistence representations rather than redacted views.
Installed integration specs and URLs should receive the same review.

**Adjustment:** account/server names, capability or configured-state flags,
and redacted key names. A successful set returns what changed, not the secret
value supplied. Debug must not turn credential disclosure back on. Keep
secret references distinct from resolved values; redact sensitive URL/query
components and inline headers at the DTO boundary. Test with synthetic values.

## F10. Retire deadweight based on capability and consumers

**Priority: P2. Retirement candidates, not blanket deletion instructions.**

| Candidate | Evidence and disposition |
| --- | --- |
| `bbox_absorb` | Chooser explicitly says compatibility no-op. Remove from normal discovery now; retain a narrow migration response only while a known caller needs it |
| `bbox_roadmap` | Existing [gap-56c74f23](../../../.bbox/gaps/gap-56c74f23.json) proposes graph-native replacement. Inventory remaining callers/data and migrate useful operations before retiring; do not polish obsolete render options indefinitely |
| `badgey_ask` | Source calls the same resume implementation with `question`. Prefer one chooser; keep an alias only where consumer contracts justify it |
| Badgey proposal shims vs `consultant_*` | Source labels the former pinned shims. Workflow compatibility may justify retention on a restricted surface, not duplicate general discovery |
| Agent/atom `get` versus `describe` | There is a real distinction: raw installed manifest versus resolved executable contract. Make it clear, then consider one detail selector if no consumers require separate tools |
| `bbox_project_list` versus catalog list/get | Legacy attached-project view and durable remote-capable catalog differ. Pick the catalog as ordinary identity discovery; retain legacy view only for an explicit operational use |
| `bbox_corpus_search` | Required harness compatibility projection with stable `id`/`text`; not deadweight merely because hybrid search exists. Restrict visibility to callers needing the compatibility shape |
| `work_*`, migration and repair tools | Useful capabilities can belong outside the general MCP chooser. Relocate or restrict by owner/role rather than deleting their underlying work |
| `bbox_topics`, specialized lifecycle conveniences | Utility is not established by a short implementation or low usage alone. Review call frequency, unique outcomes, and replacement cost before deciding |

`bro_wait`, `bro_when_all`, and `bro_when_any` have distinct coordination
semantics; their shared payload implementation does not make them redundant.
Likewise, health/repair tools can legitimately be detailed because diagnosis
is their purpose. Avoid collapsing unrelated tools into a giant action bag
just to reduce the catalog count.

## F11. Graph orientation and evidence should be compact but epistemically honest

**Priority: P2. Confirmed in source and representative inspection.**

`bbox_describe_schema` omits installed agents by default but always adds a
Badgey consultant section. Make orientation about supported entity/edge
vocabulary and traversal, scoped to what the caller can use; move consultant
discovery elsewhere. Keep useful population/participation distinctions and
validate the edge vocabulary against the actual provider registry.

Inspect and bundle duplicate structured data into rendered `text`. Inspection
can return generic zero-count next hops and render optional zero-coverage
families even in a property-only request. Omit generic empty scaffolding;
preserve schema-authored absent relationships when they answer a real question.
Per-family edge caps do not bound aggregate fan-out across all families.

Bundle already has entity/path/edge caps and explicit stale/unresolved flags.
Keep those flags. Consider summary properties by default and opt-in full
bodies, with aggregate byte budgets. Retain edge direction, assertion
authority, source generation, endpoint freshness, and unresolved refs needed
to evaluate an evidence chain. Do not hide stale evidence behind debug or
replace it with a confident prose summary. Body truncation needs a usable
exact continuation; an ellipsis alone is not sufficient for verification.

## F12. Normalize outcomes without erasing workflow semantics

**Priority: P1. Confirmed mixed contracts.**

Graph inspection can return `status="error.bad_input"` inside a successful
`Result<String>` and hence an MCP success. Oversize errors have the same issue.
Consultant apply deliberately returns domain `failed`/`bad_input` inside a
successful tool result so workflow templates can branch on its status.
These cases should not be conflated.

**Adjustment:** invalid invocation and transport/production failure use a
consistent typed error with the correct MCP error signal. A successfully
observed failed task or a domain rejection can remain a normal typed outcome,
provided its meaning is explicit and tested. Migrate dependent workflow
templates before changing their established error behavior.

Queued checkout mutations need a receipt distinguishing admission, owner-side
application, and publication. Return the mutation id, current state, and the
working follow-up. A write that is not yet published must not imply that an
immediate published-view lookup will see it. Partial multi-store changes must
report completed and outstanding effects rather than a generic failure.
These small pieces of metadata earn their space.

## F13. Keep diagnosis targeted and off the ordinary reasoning path

**Priority: P2. Some good patterns already exist.**

`bbox_embed_status` has explicit expensive coverage/diagnostic opt-ins;
`bbox_storage_health` defaults to totals and `include_files=false`;
`bbox_doctor` offers a ranked compact summary. Use these patterns for other
families, and ensure opt-in detail is still bounded and remotely meaningful.
Doctor currently couples `format=json` to full detail: encoding and detail
should be independent choices.

Scope health to the actual question. A search needs to know its vector lane
fell back or its source is stale; it does not need every queue. A GC preview
needs exact candidates and rules to approve deletion, with bounded pages and
a plan identity; a routine health call needs aggregate pressure and an
actionable reason. A plan's full candidate set must be reviewable before
application even if the first page is compact. Preserve confirmation and
operator-authority inputs; brevity is not permission to default them.

## Implementation order and acceptance evidence

1. **Response correctness and disclosure:** F01, F09, F12. Audit text and
   structured bytes together; redact configuration views; type errors and
   mutation receipts. Remove filesystem spill as producers gain continuations.
2. **High-frequency retrieval:** F04, F05, F11. Add summary DTOs and aggregate
   budgets; retain scoped degradation and provenance; stop repeated mirrors.
3. **Locality and transcript functionality:** F02/F03, starting with the broken
   ingestion and drill-down chain and administration routes with partial-write
   risk. Do not call a doc-only change a locality fix.
4. **Collection and status contracts:** F06/F07/F08. Page every growing list;
   distinguish status from body/trace; reject typo-driven widening.
5. **Discovery pruning:** F10/F13. Inventory consumers, move specialist and
   compatibility tools to appropriate surfaces, and rewrite chooser/docs around
   surviving behavior. Do not enlarge ambient memory to compensate.

Implement vertical slices across parameter schema, chooser, managed docs,
adapter, domain projection, and consumers. Avoid a global `debug=true` retrofit
that leaves persistence DTOs as the normal response. Use `detail=summary|full`
for answer depth, a cursor for continuation, and `debug` for execution internals
where each concept is needed. Format is independent of all three.

Starting review targets, to calibrate with representative fixtures: ordinary
discovery pages around 4 KiB, search/inspection/status around 8 KiB, with explicit
exceptions for evidence and requested deliverables. These are proposed product
budgets, not measurements or a new universal hard cap. More important than an
arbitrary number: default calls fit their documented budget regardless of
store size, and every omitted essential detail remains retrievable.

Acceptance evidence should include:

- Default and expanded response budgets over large synthetic catalogs, long
  Unicode bodies, nested fan-out, and both text/structured representations.
- Stable pagination under inserts, stale cursor handling, exact retrieval of
  omitted bodies, and honest empty/partial/unavailable outcomes.
- Remote-client probes with no shared filesystem, including native transcript
  search to drill-down and each retained path-bearing operation.
- Synthetic credential redaction, project-scoped diagnostics, and no leakage
  of unreadable graph lanes through counts or suggested follow-ups.
- Invalid enum/action/selector examples fail before work or mutation; rendered
  doc examples succeed against their intended schema and operation.
- Caller reasoning fixtures: distinguish queued from published, failed from
  unavailable, stale from current evidence, and context occupancy from work
  capacity after removing diagnostic fields.
- Consumer checks for workflow templates and compatibility aliases, plus
  per-tool output-byte telemetry covering the whole envelope. Track spill or
  oversize frequency, not just average size; a healthy default call should
  never depend on the final guard.

The audit adds no runtime behavior and performs no production mutation,
dispatch, migration, or service restart. Existing unresolved gap records are
evidence to reconcile against current code and deployment, not proof that
every historical symptom still reproduces.

## F14. Provider selection must not imply unrelated account membership

The operator reported Brodex dispatches labeled with the `openrouter` account
without an intentional account selection. Source inspection found that
`allocator::account_candidates` enumerated every global account for every
provider. The selected account environment was subsequently applied during
execution. An unrelated account could therefore be merely a misleading label
or an actual environment override, depending on its contents.

The implementation now uses the explicit account when present, otherwise the
selected provider's declared default or native credentials. It does not add a
second native candidate when a provider default exists, because execution
resolves that candidate back to the default and would create two allocation
keys for the same credentials. Synthetic coverage checks unrelated accounts,
provider-scoped defaults, native fallback, and explicit override. Track the
operator report as `tooling/runtime-allocation/cross-provider-account-candidates`
(gap-3f34ae40, queued through the checkout-owner lane).

## Implementation checkpoints

The first implementation milestone addresses whole-result sizing and accidental
spill removal (F01), compact hybrid search (F04), gap summary pages and scoped
carrier diagnostics (F05/F06), configuration redaction (F09), canonical
invocation error flags (part of F12), and account candidate selection (F14).
Tests and deployed smoke results are recorded at milestone completion. Other
findings remain open; the original observations above describe the audit
snapshot and must not be mistaken for the current implementation status.


The second source checkpoint adds strict locked account mutations that retain
existing policy, validates graph detail selectors, removes embedded graph text
mirrors, and removes native transcript filesystem reads. Native drill-down is
explicitly an indexed projection, with exact selectors and byte-bounded pages;
it does not establish source completeness or restore ingestion. The native
collector/source replacement remains in progress. The bridge fixture amendment
records the intended presentation changes rather than weakening view-selection
or authority assertions.


The next source checkpoint separates doctor encoding from detail, adds bounded
ranked summary pages, and preserves explicit full diagnostic JSON for existing
operator consumers. The retired absorb no-op is hidden from the remaining
ordinary interactive surface and ambient recommendations; its migration alias
remains on ops. Shipped render/review runbooks no longer claim it imports edits.
Roadmap retains useful operator-directed stored work tracking: checked-in docs
and promotion/link consumers justify keeping it pending a real graph migration.
Its MCP renderer now returns content and accepts inline templates; explicit
local paths refuse and implicit server-configured destinations are removed.
Badgey's question alias remains because two shipped eval workflows call it;
Badgey tools already belong outside ordinary default/interactive discovery.


### Deployed retrieval milestone, 2026-09-06 UTC

Image `4ffe9430f0e3`, immutable digest
`1397786cc6253fa33d6d174765c26c6c4434459f591e382c39ce5a06021af2fe`,
was deployed through cage converge to blackboxd and the Slack collector.
Build workflow `build-bbox-image-rfglp` succeeded. Verification workflow
`bbox-verify-lcrbz` passed full nextest (7400 passed, 21 explicitly skipped),
Clippy, and concurrency lint on `a4fc0fdc`; its only difference from the built
revision is three test assertions recognizing canonical MCP error flags.
The earlier verification failure was those outdated assertions, not a waived
gate. The bridge fixture amendment was reviewed row by row.

Remote MCP probes measured the complete compact-JSON result envelope, counting
both text and structured representations and UTF-8 bytes. Before/after calls
used the same selectors; source contents and vector availability remain live.

| Probe | Before bytes | After bytes |
| --- | ---: | ---: |
| Provider discovery | 14787 | 854 |
| Brodex model catalog (explicit provider) | not compared | 3885 |
| Brofile summaries, limit 3 | old unbounded list spilled | 638 |
| Hybrid search, limit 3 | 22444 | 3897 |
| Hybrid search with ranking debug, limit 3 | not compared | 7367 |
| Gap summaries, limit 3 | 19390 | 6795 |
| Exact gap detail | not compared | 4274 |
| Deliberately oversized invalid entity ref | not compared | 897, MCP error |

The old brofile spill incorrectly had `isError=false`. The new guard emits
`isError=true`, a typed response_too_large error, no spill path, and a retry
warning appropriate for potentially completed mutations. Invalid dashboard
provider/status/team filters all refused. Brodex discovery included GPT-6
Astra; a real narrowly scoped Astra dispatch with no account pin selected
native credentials (`account=null`) and completed with the expected synthetic
reply. Configuration redaction was covered with synthetic tests, without
probing live credential values. Native ingestion is a subsequent milestone;
this deployment's indexed transcript reads do not claim missing history was
collected. Subsequent source commits are not covered by these deployment claims.


### Subsequent source checkpoint: collection and exact continuation

The native source transport now uses dedicated existing producer grants,
content-addressed chunks, compare-and-swap snapshot admission, and streamed
host capture. Reader leases and purge protection preserve the indexed generation
across concurrent publication and source outages. Revocation still removes
index enrollment. Source observations distinguish contact, scan completion,
published generation, and indexed generation. Host enrollment and backfill were verified in the deployed milestone below.
Continuous host collection still requires macOS Local Network approval.

Graph inspection now pages edges and exact property text using revision-bound
cursors. Bundles default to 600-character summaries with exact expansion.
Task MCP and control status expose bounded previews and exact result/report
continuation; duplicate snapshots and routine accounting are omitted. The
control event detail states that it covers only retained ring events.

Atom, agent, workflow, cron, poller, webhook, event, and reaction discovery now
have bounded pages. Event continuation is anchored to journal identity; catalog
offsets explicitly describe a live view. Trigger and reaction diagnostics omit
credentials, opaque URL components, request values, and server-local paths.
Storage health pages daemon-relative file coordinates and retains warnings.
Artifact installation accepts inline JSON, propagates runtime persistence
failures, and reports completed, failed, and unattempted stages. Its stores
remain nontransactional; exact stage receipts describe partial effects.

The full-suite gate caught a historical-agent supersession regression introduced
while moving artifact deactivation after replacement persistence. The correction retires old snapshots after durable replacement and repairs
interrupted retirement on retry. Full verification and deployment succeeded
in the milestone below. Proposal summary pagination and the shipped Badgey
page/expansion loop are implemented together in the next source milestone;
their coordinated runtime upgrade remains separately gated. Whiteboard
visibility and project-admin locality fixes are included in the deployed
collection milestone.


### Deployed collection and continuation milestone, 2026-09-06 UTC

Image `ef7f7f04d0f1`, digest
`057dfc885fab28deb76c87c9e4c848e6eee564c5ac7ddf619d2416bb5a482e5c`,
passed image workflow `build-bbox-image-fctq9`. Full verification
`bbox-verify-kb8t8` passed 7464 tests (21 explicitly skipped), Clippy, and
concurrency lint on `72092204`. That verification revision differs from the
image only in two audit classifications: brokered catalog leases and a
response-only canonical-path read. Both the daemon and Slack collector are
healthy; a subsequent converge reported 22 unchanged resources.

A dedicated native transcript producer enrolled through the source catalog.
An interactive backfill discovered and published all 720 eligible Claude and
Codex streams, uploading 3022167556 bytes with zero failures or deferrals.
Live search, context, messages, and session reads returned indexed native
locators with matching published/indexed generations. This establishes a
successful backfill, not continuous collection: the installed launch agent
cannot connect under macOS Local Network privacy, while the same binary can
connect interactively. Its bounded error chain reports TCP errno 65. Normal
signing and operator Local Network approval are required before a completed
background cycle can be verified; see `docs/native-transcript-collector.md`.

Live read-only MCP checks measured complete compact result envelopes:

| Probe | Prior bytes | Deployed bytes |
| --- | ---: | ---: |
| Atom catalog, default page | 63660 unbounded | 6900 |
| Agent catalog | not compared | 1782 |
| Workflow catalog | not compared | 1110 |
| Event list, limit 3 | 1348 | 831 |
| Storage health summary | 3872 | 2325 |
| Storage health files, limit 3 | not compared | 3189 |
| Doctor JSON summary, limit 3 | not compared | 2090 |

All 135 installed atoms were reachable exactly once across seven pages. Task
result continuation reconstructed the synthetic Astra result in three
8-byte pages. Graph property continuation reconstructed the stored 300-byte
preview in five pages, and evidence bundling succeeded. This checks exact
stored-property continuation, not recovery of text already shortened during
indexing; that distinction is under further scrutiny. No live Slack message
was sent for these probes. Configuration and whiteboard confidentiality
checks used synthetic fixtures rather than exposing live private values.


### Deployed proposal and catalog milestone, 2026-09-06 UTC

Image `a85fc06768aa`, digest
`34a5913a5196d0195410f5c7110c2ffa0d19eb6efe6d51a826ad112215c2052f`,
passed build `build-bbox-image-g7j4c`. Verification `bbox-verify-bvt7h`
passed 7481 tests (21 skipped), Clippy, and concurrency lint on `1b56fe5b`.
That verification revision only updates the ownership audit's function name
and classification for response-only attachment coordinates after helper
extraction. The initial full run caught the stale audit entry; no gate was
waived. Converge updated both runtime Deployments, which became ready with
zero restarts.

The deployed proposal API and shipped workflow consumers were upgraded together:
`hook-route/proposal-page` v1, `badgey-slack-emit-proposal-arc` v2, then
`badgey-triage-channel-arc` v3. Each install was preceded by discovery; installed
runtime and artifact versions were checked afterward. The arc runtime was quiet
and no Slack post was triggered. There were no installed Badgey instances to
exercise a real draft: large draft reconstruction and workflow pagination were
verified with synthetic adapter/workflow fixtures.

Live catalog summary and alias reads succeeded (789 and 708 bytes for the
source-only native history project). Full arc JSON reconstructed across five
8-byte pages. The three trigger install schemas now expose required nested
spec contracts. A synthetic invalid timezone was rejected by the live cron
install endpoint, and exact cron discovery remained empty before and after.
These probes validate error admission without installing or running a timer.

### Further source scrutiny

Embedding status repeated provider configuration, null values, zero counters,
and coverage advice across nine routes (7554 bytes live). Its summary now keeps
health, queue depth, actionable failures, nonzero loss/cap counters, and one
coverage note; `debug=true` restores configuration diagnostics without enabling
expensive scans. Queue success counts now say `session_indexed_count` rather
than appearing to measure corpus totals after restart. Exact coverage retains
separate source/indexed counts. Poller summaries distinguish requested cadence
from the effective interval captured when the loop starts; cron diagnostics
report the effective timezone and identify legacy unsupported settings.

Commit inspection now returns stored indexed content, so ordinary smart
projection marks shortening and exact property pages can recover beyond the
old intrinsic 300-character preview. Upstream ingestion loss remains explicit;
other provider preview fields cannot reconstruct content they never retained.
Team discovery moves full charters and templates behind exact JSON body pages,
uses bounded summary pages, and reports malformed store records visibly.
These follow-up source changes require their own gates and deployed probes.


### Deployed final scrutiny milestone, 2026-09-06 UTC

Image `b049caa8572c`, digest
`07ee88c45e6926492fb67f6b94836ab1499aae571c2fa0b2fc8f8828e36d987b`,
passed image build `build-bbox-image-qzjzz` and full verification
`bbox-verify-6wd9q` on that same source revision: 7497 tests passed, 21 explicitly
skipped, Clippy and concurrency lint passed. Converge updated both runtime
Deployments; both were ready with zero restarts. Subsequent source edits only
record release and verification evidence.

Live MCP checks passed after deployment:

- Embedding health fell from 7554 to 1080 bytes for the default reply. Explicit
  debug detail was 6211 bytes and classified each indexed counter's meaning.
- All 11 teams and 7 global templates were reachable exactly once through
  summary pages. Exact template JSON reconstructed across three 128-byte pages.
  Project-template discovery refused an unsupported remote checkout instead of
  probing its path. Team creation caps expanded membership at 256; synthetic
  tests verify refusal before writes and consistent global configuration across
  later broadcast, exec, resume, allocator, and advisor consumers.
- Commit inspection marked its shortened content and recovered all 706 stored
  bytes through three property pages, beyond the old 300-byte preview.
- Corpus stats returned 198 bytes and no longer inferred source absence or zero
  edges from daemon-local files. Counts are indexed metadata, cached up to 60s;
  source coverage and freshness are explicitly unassessed by that tool.
- Cron upcoming results declared UTC, and all three upgraded proposal runtime
  artifacts retained their expected active versions across the restart.

The macOS collector built with its own embedded identity and Local Network usage
metadata and was installed on the source host. A subsequent interactive publish
reported 720 discovered streams, 4 changed and published, 716 unchanged, zero
failed or deferred, and 9873357 uploaded bytes. Live session freshness recorded
the completed scan with `scan_in_progress=false` and matching published/indexed
generations. The launch agent still refuses TCP with errno 65. Its linker ad-hoc
signature does not bind the embedded Info.plist; normal signing and the user's
Local Network approval remain required. Metadata alone does not grant access.

The audit therefore closes the deployed MCP shape/size/locality changes while
leaving continuous host collection activation explicit in gap-e40feb8f. Exact
reads recover retained provider data, not source content discarded at ingestion.
No claim is made that every action of all 190 registered tools was exercised
against production state; the coverage inventory distinguishes source review,
synthetic contract tests, live reads, and the limited authorized deployments and
artifact upgrades.


Final checkout-owner publication reached durable success for code, Git history,
and repo-owned knowledge/gaps at publisher commit `21e62d85`; the updated native
collection gap was confirmed through the published MCP view. The same collector
cycle received `503 edge_index_warming` for provenance export while the complete
graph rebuilt after deployment. That export was not claimed successful; indexed
property reads and the separate publication lanes succeeded independently.
