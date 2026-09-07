---
title: "Surviving MCP action audit"
kind: design
corpus: blackbox-design
lifecycle: partial
topic:
  - surfaces
  - mcp
brief: "Current per-action dispositions, evidence limits, and prioritized residual caller-contract work."
---

# Surviving MCP action audit

2026-09-07 supersession: [roadmap elision](roadmap-retirement.md) removes the
roadmap subsystem completely. Historical-reader, preservation and migration
obligations below describe this earlier checkpoint and no longer apply.
Measured evidence and original counts remain unchanged.

Current owner: thread-c749d06c. The audit below is the reconciliation baseline
at source 004822640975f19f90dc4d50dd16d7fc9efc42e8, committed as fdd0180c.
The subsequent [fix checkpoint](mcp-survivor-fix-checkpoint.md) is authoritative
for implemented corrections, current verification and retained obligations.
Remaining/future wording in the baseline matrix describes the queue that drove
those fixes, not a claim that each original defect still exists.

The source declarations and complete ops replay still agree on 109 named tools.
Default replay exposes 96; this connected client exposes 101, with the five
agent and three allocator tools absent. Policy discovery is not callable client
access. The table covers all 256 original branch rows and explicitly lists the
45 detail refinements. These refinements subdivide rows, not disjoint actions
to add to the tool count. Machine-readable mappings and sanitized measurements
are in [reconciliation evidence](mcp-survivor-reconciliation-evidence.json).

Audit what a caller can choose, supply, receive, trust and recover. Backend
delivery/readiness failures can limit truthful claims; their existing owners
retain implementation responsibility. GLM experiments belong to thread-c130128f.
The shared allocator extraction is a separate design exploration. Neither is
a prerequisite for this surface audit.

## Evidence and closure rules

- **Source**: adapters, parameter DTOs, response producers and code-owned tool
  docs inspected at the revision above. Named regressions were inspected, not
  rerun. Source-confirmed defects are distinct from unexecuted adverse cases.
- **Production read-only**: unknown-only aggregate refusal, project-filtered
  pins, invalid scope/selector refusal, surface paging, ordinary knowledge and
  publisher/schema reads, native search hints and exact thread-note recovery.
  Explicit thread detail=summary failed live while omitted detail works.
- **Prior isolated HTTP**: the handoff reports 173 checks with synthetic state
  and inert-worker dispatch. The checked-in integration JSON is an older
  checkpoint. Neither represents exhaustive execution of every action here.
- **Prior gates**: the handoff records 6690 full-profile passes, 19 skips,
  clippy, pinned formatting and handler lint, with a temporary lane-only
  scheduling override for two publication-lock tests. This pass changes audit
  artifacts only and does not claim new Rust gates or a new deployment.
- **Freshness**: publisher status accepted source 00482264. That does not prove
  the running binary revision; deployed 27046e0c2dfc is reported by the handoff,
  not independently verified in this pass.
- **Measurement**: compact JSON serialization of the complete CallToolResult,
  including escaped text and structuredContent duplication where present;
  HTTP/JSON-RPC framing excluded. Measurements are samples, not growth bounds.
  No raw private bodies, other-project names, session locators or credentials
  are committed with this evidence.

Disposition is independent of evidence depth: **adjusted** means a correction
exists, **retained** means a distinct useful role, **restricted** means an
intentional authority/compatibility lane, **retired** means a callable refusal,
and **remaining** names a current concrete contract correction. None means
every adversarial case passed. Work references below identify exact next work;
V01 is a verification obligation, not an assertion that a current defect exists.

The [initial matrix and integration checkpoints](mcp-survivor-action-audit-initial-checkpoint.md)
preserve the old findings and
measurements. They are historical evidence, not current defect descriptions.
The [chronological audit](mcp-response-and-contract-audit.md) and
[older 190-tool inventory](mcp-audit-coverage.md) remain historical maps.

## Original finding reconciliation

| Finding | Current disposition | Evidence and residual |
| --- | --- | --- |
| A01 aggregate false success | Adjusted, narrow live verification | Unknown-only all/any refuse; whole-selection, duplicate/team/fanout and mixed terminal outcomes have inspected regressions. Duration validation remains R01. |
| A02 checkout authority on host reads | Adjusted, narrow live verification | Registered-project pin read works. Notes/pins use host-owned identity. Their different unknown-project semantics and action bags remain R06. |
| A03 invalid selector fallback | Partially adjusted | MCP/brofile/pin/partition/surface validation and migration resolution corrected. Prune provider/empty-ID broadening is a separate current instance, R01. |
| A04 unbounded exact detail | Partially adjusted | Thread/note/pin/knowledge/review/graph/MCP/account/agent/allocator/packet exact readers exist. Gaps/artifact receipts, doctor producer omissions and thread metadata recovery remain R04. |
| A05 repetitive diagnostics | Partially adjusted | Knowledge diagnostics, publisher health and surface policy summaries improved. Normal knowledge text, agent search, schema and remaining metadata still need R05. |
| A06 nested fanout | Partially adjusted | Many inventories and mutation batches now bound rows/receipts. Graph validation variants, bundle metadata and post-effect report/prune output remain R02/R05. |
| A07 incorrect search reader hints | Adjusted, source and inspected tests | Typed native/conversation/thread/entity recovery implemented. Fresh native-hit probe confirms only that branch; V01 must exercise other hit types. |
| A08 overlapping/retired capability | Explicit dispositions, decision open | MCP sync already refuses; absorb/bootstrap labeled compatibility, default-hidden. Project bridge consumer and real roadmap branches preclude automatic deletion, R07. |
| A09 locality hidden from chooser | Partially adjusted | Rename/eject/promote/migrate now clearer. Render/import/bind/register/init and daemon-owned brofile/MCP configuration need R06. |
| A10 schema/chooser parity | Partially adjusted | New exact selectors documented; MCP/partition closed choices improved. Explicit summary fails live, owner review validation differs, and roadmap hints/default_template remain R02/R06. |
| A11 confidentiality | Partial field-specific evidence | Account/MCP credential projection and agent URL handling tested previously. Artifact supersede raw metadata and opaque debug text require isolated sentinels, R03. No live leak asserted. |
| A12 admission versus completed effects | Partially adjusted | Allocator save failure now surfaces; existing queue and staged receipts preserved. Pre-effect validation and lost partial outcomes remain R02; backend guarantees stay separately owned. |
| A13 honest continuation | Partially adjusted | Content/stamp cursors and explicit live offsets implemented on many surfaces. Exact thread note recovered across 14 pages. Metadata recovery and missing list movement notes remain R04/R06. |
| A14 bounded work | Still distinct workstream | Doctor section selection improved. Migration, storage, packet history and some diagnostic/detail readers still collect broadly before paging, R08. Small bytes are not cheap execution proof. |

## Prioritized implementation queue

The rows are ordered work areas. Split implementations along the listed source
owners, retaining already-working contracts and validating each change.
R01-R03 take precedence over cosmetic brevity. Scope is caller behavior, not
general backend repair. No retirement is authorized by this queue alone.

| Work | Priority and owner | Concrete change and acceptance |
| --- | --- | --- |
| R01 | P0 selector safety; dispatch and bro_params | Reject invalid prune provider and explicit empty task_ids before selection, persistence or retro. Validate nonnegative representable wait/all/any duration before registering waits. Seed mixed providers/statuses in isolated state; invalid preview/apply inputs leave tasks, persistence requests and retro admissions unchanged. Include zero and huge finite durations. |
| R02 | P1 effects and validation; dispatch, roster, allocator, render/review, notes, project/admin adapters | Validate allocator clear/update/cooldown/detail contradictions and review fields before enqueue/write. Strict allocator read-modify-write must preserve corrupt bytes and concurrent lane updates. Account/brofile discovery must distinguish unreadable/corrupt state from empty/not-found without changing stored bytes. Bound note/report/prune batches before effects or return exact bounded receipts. Preserve all admitted broadcast child IDs after later filter failure, team save/cancel/removal errors, compaction-before-rebuild and multi-note-export partial effects. Inject later-stage failures in isolated fixtures; distinguish applied in memory, persistence requested/failed and published. |
| R03 | P1 confidentiality evidence; artifacts and config/agent/allocator projections | Probe synthetic credential/query sentinels through artifact install then supersede, account/MCP exact reads, agent metadata and every debug/error branch. Inspect complete text plus structured replies. Replace raw metadata projection if disclosure is confirmed. Treat opaque operator prose as opaque, not automatically secret-free or credential-sanitized. No real credentials in fixtures. |
| R04 | P2 complete recovery; gaps, artifacts, doctor, threads, knowledge | Add exact recovery for oversized gap/artifact records and producer-omitted doctor findings; supply type-correct recovery for thread topic/session/edge metadata and system-memory catalog pages. Start with more than 20 project findings, long titles and escaped Unicode. Reconstruct exact stored content and prove stale/cross-selector refusal; do not page an already-trimmed report and call it full recovery. |
| R05 | P2 complete-envelope brevity; knowledge, graph, agents, roster and publisher | Bound ordinary knowledge text/title/provider/histogram output, agent search hit metadata, schema agent expansion, bundle/path metadata, graph validation variants, publisher auto-advance refusal and lifecycle receipts. Use summary plus exact expansion and honest top-k/omission counts. Measure normal and escaped complete envelopes at worst accepted field size and fanout; preserve identity, CAS and stale/unavailable/partial state. |
| R06 | P2 chooser parity; parameter DTOs, adapters and bbox-tool-docs | Fix explicit thread summary dispatch; reject or document wrong-action fields. Correct review owner-path ordering, publisher detail_limit semantics, roadmap promotion hint/default_template, lint reader name, prune retro ordering and session-list limits. Lead restricted tools with actual authority and supported lane; move cold descriptions to runbooks. Probe omitted/explicit defaults, contradictory selectors and missing requirements through served schemas. |
| R07 | P3 simplification decision; compatibility owners | Inventory actual consumers and unique outcomes for sync/absorb/bootstrap, project list/unregister, legacy migration/compaction/provenance and roadmap. Preserve bridge discovery and historical data. Document retained, restricted or retired choice with a concrete replacement/migration; roadmap consumer/data ownership remains gap-56c74f23. No broad deletion based on candidate labels. |
| R08 | P3 query cost; response producers | Measure requested section/filter cost separately from output size. Target doctor catalog status, storage/migration planning, packet event scans, allocator preview, repeated publisher detail and empty-index corpus lookup. Prefer bounded selection/existing targeted producers; document unavoidable scans. Do not expand into index/database redesign. |
| V01 | Closure verification; implementing author and orchestrator | Extend isolated HTTP cases only for uncovered meaningful branches: growth, exact bytes, stale cursors, no writes on detail, unknown/mixed/pruned selections, unavailable/corrupt state, retries/partial effects and all relevant source/authority modes. Every remaining branch needs source plus concrete regression/runtime evidence, or an explicit restriction/dependency. For code changes run required lane-side pinned fmt, nextest --workspace full, clippy and concurrency lint; record the exact ref and any scheduling override. |

## Source anchors and inspected regression evidence

These are implementation evidence, not newly run tests. The matrix's adapter
links are source owners; domain anchors below locate the notable current issues
and existing tests to extend. All anchors refer to source 00482264.

| Family | Current implementation and regression anchors |
| --- | --- |
| Dispatch/roster | src/tools/dispatch.rs:1515 whole-selection waits; :2071 empty prune IDs, :2093 provider fallback, :2232 post-effect receipt; :1920/:1973 broadcast early returns; src/tools/roster.rs:265 report, :833 dissolve. Inspected when_selection_validates_entire_input_before_waiting and when_team_selection_defines_stale_history_and_empty_teams; maximum-fanout/mixed-outcome tests at dispatch.rs:3385-3854. |
| MCP/accounts/agents | src/orchestration/mcp.rs:738 selected-store validation, :1154 retired sync; src/tools/agents.rs:369 exact describe, :759 search; src/tools/roster.rs:590 exact accounts. Inspected exact inventory/escaped-identity tests in mcp.rs:1957-2102, roster.rs:1938-1988 and agent metadata/computed-summary tests at agents.rs:3287-3463. |
| Allocator | src/tools/dispatch.rs:1044 status, :1256 trace, :1320 probe; src/orchestration/allocator.rs:1425 lossy probe load. Save-failure/exact-page tests at dispatch.rs:4681-4985 exist; mutation-plus-stale-cursor and corrupt/concurrent update cases remain. |
| Host stores/threads | src/tools/attention.rs, notes.rs and threads.rs; crates/bbox-threads/src/threads.rs:733 explicit summary mismatch, :724-803 history previews. Inspected pin_reads_and_writes_resolve_host_owned_identity_without_checkout, pin_exact_body_pages_reconstruct_unicode_and_reject_stale_and_cross_cursors, note_exact_read_pages_unicode_and_rejects_stale_and_cross_cursors and bbox_thread_get_defaults_bounded_and_exact_reads_use_body_cursor. |
| Knowledge/review/gaps | src/tools/knowledge.rs:2240 text projection; crates/bbox-knowledge/src/knowledge.rs:4302 lint, :4410 review; src/tools/render.rs:362 queued review validation; crates/bbox-gaps/src/gaps.rs:2165 full record. Inspected exact_diagnostics_pages_preserve_scope_and_reconstruct_unicode, bbox_knowledge_exact_metadata_pages_stay_inside_serialized_envelope, queue bounds/reconstruction in src/tools/knowledge_queue_tests.rs:145/:201; domain wrong-field tests do not prove queued validation. |
| Graph/search | src/tools/graph.rs:1279 validation variants; crates/bbox-mcp-tools/src/mcp_tools/bundle_evidence.rs:58; describe_schema.rs:42; crates/bbox-corpus-index/src/index/search.rs:75 typed recovery. Inspected project_graph_describe_default_is_compact_and_schema_recovers_exactly, project_graph_variants_select_and_page_across_sources_and_checkouts, project_graph_validate_pages_errors_and_recovers_the_exact_array, knowledge_and_code_hits_use_canonical_entity_inspection and slack_and_generic_entity_hits_get_matching_recovery_selectors. |
| Packets/artifacts | src/tools/packets.rs:168/:196 result pages; artifacts.rs:110 list and :240 supersede; crates/bbox-artifacts/src/artifacts.rs:86 metadata. Inspected large_first_consequent_and_audit_values_have_exact_result_pages, packet_audit_pages_mismatches_and_rejects_oversized_batches_before_events, event_cursors_reject_filter_changes_and_same_size_rewrites and artifact_install_catalog_failure_reports_persisted_runtime. |
| Catalog/admin | src/tools/project_catalog.rs:1989 status, :2148 raw auto-advance attempt; :2006 detail validation; projects.rs:1297 bridge list. Inspected publisher large-field/reconstruction tests at project_catalog.rs:5396-6006; bridge consumer src/server/bridge_parity.rs:983; real export-plan consumer crates/bro-cli/src/provenance.rs:11. |
| Health/maintenance | src/doctor.rs:350-498 producer caps; src/tools/graph.rs:1569 compaction/rebuild; crates/bbox-mcp-tools/src/mcp_tools/provenance.rs:115 batch writes. Inspected migration selector tests at src/tools/storage_migration.rs:195/:272/:345, GC immutable-reader tests at src/tools/storage_gc/report.rs:309/:366, and partition_exact_inventory_recovers_large_fields_and_partial_apply_is_explicit in src/embed_runtime.rs. |
| Surface/history | src/tools/mcp_surface.rs:177 byte-aware pages and :377 approximate predicate display; src/tools/tool_calls.rs:95 candidate scan. Inspected test_replay_policy_detail_pages_exact_patterns, test_list_rejects_replay_and_describe_selectors, test_surface_page_byte_limit_recomputes_next_offset and tool_history_continues_past_empty_filtered_pages_and_preserves_identity. |

## Current action dispositions

Every original branch label appears below. Additional detail refinements follow
the original labels in the same cell. Labels such as published/own/all are
authority branches, not necessarily action parameter values. Each row names its
residual work or verification obligation; source-only retention stays explicitly
short of audit closure. MCP sync has a retired action disposition inside its
otherwise retained configuration tool; restricted prune/retro operations still
require operator authority even where the row says remaining.

### [src/tools/agents.rs](../../../src/tools/agents.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bro_agent_describe` | describe. Detail refinements: manifest body; brofile body; metadata body; summary body | adjusted | Stored manifest, resolved brofile, metadata and complete computed-summary pages now recover oversized fields. Actual runtime permission planes are explicitly partial; distinct from stored get. Work: R03, V01. |
| `bro_agent_dispatch` | dispatch | retained | Manifest validation, custom-adapter refusal, authority checks and attribution add value over raw exec. No new provider execution in this pass. Work: V01. |
| `bro_agent_get` | get. Detail refinements: manifest body; metadata recovery via bro_agent_describe | adjusted | Lifecycle/manifest summary and exact redacted manifest pages implemented; complete metadata recovers through bro_agent_describe(detail_plane=metadata). Field-specific redaction is not a claim that arbitrary prose is sanitized. Work: R03, V01. |
| `bro_agent_list` | detail; summary | retained | Filter-before-paging and sorted identity summaries; expanded list is not exact manifest replacement. Specialist discovery does not mean this client can call it. Work: V01. |
| `bro_agent_search` | query | remaining | Full descriptions/when-to-use/anti-pattern arrays remain per hit; total_matched is only returned length and vector telemetry is always present. Use existing exact readers for expanded text. Work: R05. |

### [src/tools/artifacts.rs](../../../src/tools/artifacts.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_artifact_install` | HTTP source; inline | retained | Typed kind, exactly one inline or HTTP input, inline byte cap and staged failure reporting. Nested artifact shape remains opaque; installation is distinct from catalog activation. Work: R03, R06, V01. |
| `bbox_artifact_list` | detail; retired kind; summary | remaining | Collection pages exist, but a huge description or supersession chain can overflow one receipt row. No exact artifact receipt reader; offset movement is not disclosed in the response. Work: R04, R05. |
| `bbox_artifact_remove` | call | restricted | Operator-directed removal with confirmation guard. Preserve historical receipt versus runtime removal distinctions; no destructive acceptance in this pass. Work: V01. |
| `bbox_artifact_supersede` | call | remaining | Useful lifecycle operation, but returns raw ArtifactMetadata including source URL, project path, warnings and chain. Synthetic disclosure/size proof required; no live secret leak asserted. Work: R03, R05. |

### [src/tools/attention.rs](../../../src/tools/attention.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_inbox` | get | retained | Section row/preview caps and expansion hints remain; adapter appends visibility/provenance afterward. Prove complete envelope under high diagnostic fanout. Work: R05, V01. |
| `bbox_pin` | delete; list; set. Detail refinements: list exact id/full | adjusted | Host-owned identity, early scope/action validation, live summary pages and exact pin JSON implemented. Project-filter read and invalid scope verified live; wrong-action field validation remains incomplete. Work: R06, V01. |

### [src/tools/config.rs](../../../src/tools/config.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bro_mcp` | add; allow; clear_filters; disallow; get; list; remove; sync. Detail refinements: get_filters; list exact inventory | adjusted; sync retired | Scope/action validation, selected-store list/get, exact redacted server/filter inventory and HTTP/SSE add implemented. sync is a callable retired refusal; project configuration remains daemon-owned. Work: R06, R07, V01. |

### [src/tools/dispatch.rs](../../../src/tools/dispatch.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bro_allocator_probe` | clear; read; update. Detail refinements: exact probe body | remaining | Save errors propagate and exact probe pages exist. Corrupt loads become empty, contradictory clear/update/cooldowns silently choose, and update can persist before invalid cursor rejection. Work: R02, R03, V01. |
| `bro_allocator_status` | candidate preview; status. Detail refinements: config body; preview body; in_flight body; probes body; leases body | adjusted | Independent compact inventories and exact config/preview/in-flight/probes/leases planes implemented. Current versus expired runtime quota distinguished; all inventories and allocation preview still computed before paging. Work: R03, R08, V01. |
| `bro_allocator_trace` | get. Detail refinements: exact trace body | adjusted | Compact summary and exact change-bound trace pages implemented. Opaque raw diagnostic prose is not guaranteed credential-sanitized. Work: R03, V01. |
| `bro_broadcast` | dispatch | remaining | Pre-effect member/receipt caps and ordinary-loop per-child outcomes implemented. Later filter resolution can early-return after prior admission; final team save has no durable-result status. Work: R02, V01. |
| `bro_cancel` | call | retained | Distinct cancellation control; no peer task controlled in this pass. Work: V01. |
| `bro_exec` | dispatch; request_key replay | retained | Fresh dispatch, selector alternatives and request-key replay preserve reservation/admission/unknown outcomes; no fresh provider probe needed for this docs pass. Work: V01. |
| `bro_interrupt` | call | retained | Interrupt without replacing session continuity remains distinct from cancel and steer. Work: V01. |
| `bro_prune` | apply; preview; retro option | remaining | Invalid provider becomes no filter; empty task_ids removes the ID restriction, defaulting to the failed-task sweep when status is omitted; apply is default. Complete IDs are returned after effects and disk persistence is only requested. Lead the queue with pre-effect selector refusal. Work: R01, R02, R05. |
| `bro_resume` | request_key replay; resume | retained | Existing-session continuation with request-key replay and owner refusal remains distinct from fresh exec and thread reads. Work: V01. |
| `bro_retro` | call | restricted | Effectful resumed model turn, not passive detail. Prune retro docs say before dropping, while code drops first; correct ordering and partial outcome language. Work: R06, V01. |
| `bro_status` | debug; report; result; structured_exit; summary | retained | Bounded summary, debug and exact result/report/structured-exit planes implemented. Debug remains field-specific evidence, not a universal redaction guarantee. Work: R03, V01. |
| `bro_steer` | call | retained | In-flight user input preserves current task continuity; distinct from resume after terminal work. Work: V01. |
| `bro_wait` | wait | remaining | Unknown-task refusal and timeout snapshot maintained. Unvalidated negative or unrepresentable timeout reaches Duration conversion; validate before waiting. Work: R01, V01. |
| `bro_when_all` | task_ids; team | adjusted | Whole selection validated before waiting; 64-target cap, duplicate semantics and bounded mixed-outcome receipts implemented. Unknown-only refusal verified live; shared duration validation remains. Work: R01, V01. |
| `bro_when_any` | task_ids; team | adjusted | Whole selection validated before waiting; bounded terminal/success distinctions implemented. Unknown-only refusal verified live; negative or huge finite duration can panic after valid selection. Work: R01, V01. |

### [src/tools/doctor.rs](../../../src/tools/doctor.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_doctor` | full; summary | remaining | Section-targeted collection and full JSON pages implemented, but catalog producers cap findings before DoctorReport. Exact pages cannot recover those omitted findings. Work: R04, R08. |

### [src/tools/gaps.rs](../../../src/tools/gaps.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_gap` | global; project | retained | First-class deduplicated substrate record. Project mutation is queued to the checkout owner; this pass observed admission before independently checking delivery. Work: V01. |
| `bbox_gap_resolve` | resolve; supersede | retained | Resolution and paired supersession remain distinct. Paired admission does not imply atomic owner delivery; existing transport dependency owns that guarantee. Work: V01. |
| `bbox_gap_update` | update | retained | Existing-record edits compose through the owner queue; do not recreate occurrences to update evidence. This pass uses the existing audit gap and leaves it unresolved. Work: V01. |
| `bbox_gaps` | debug; exact id; full; summary | remaining | Typed filters, summary pages and visibility signals work. Exact ID/full still serialize the whole record, with no body cursor when a single record exceeds the envelope. Work: R04, R05. |

### [src/tools/graph.rs](../../../src/tools/graph.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_blame` | anchor; git fallback | restricted | Indexed anchor provenance and local Git fallback remain distinct authority lanes; an origin result is evidence of authorship/history, not correctness. Work: V01. |
| `bbox_bundle_evidence` | full; none; summary | remaining | Entity/path/edge count caps exist, but full properties, metadata and echoed question are not a producer byte bound. Property readers recover individual values, not every oversized bundle envelope. Work: R04, R05. |
| `bbox_describe_schema` | agents; full; orientation | remaining | Default vocabulary is still large; installed-agent expansion is unpaged. Current default measured 15282 complete result bytes, one sample. Make orientation compact and agent inventory recoverable. Work: R05. |
| `bbox_edge_compact` | apply; apply with rebuild; preview | restricted | Distinct legacy compaction with backup and optional rebuild. A later rebuild error currently loses completed compaction evidence; preserve a partial receipt. Work: R02, V01. |
| `bbox_find_paths` | to; to_type | retained | Bounded traversal and direction-preserving cached paths remain useful. Move runbook-scale chooser prose to cold docs while keeping identity/readiness prerequisites. Work: R06, V01. |
| `bbox_inspect_entity` | edge_cursor; full; property; smart; summary | retained | Summary/smart/full, exact property pages and edge cursors preserve projection/freshness limits. Exact reads cannot expand content omitted during ingestion. Work: V01. |
| `bbox_project_graph_describe` | all; own; published. Detail refinements: schema body; descriptor body; variant page/selectors | adjusted | Compact descriptor, exact schema/descriptor pages and variant pagination/selectors implemented with content-bound cursors. Preserve source/checkout/hash identity. Work: V01. |
| `bbox_project_graph_list` | all; own; published | adjusted | Row/byte pages, deterministic order and nonzero-offset stamp checks implemented. Changed views refuse continuation; do not call the mutable inventory a permanent snapshot. Work: V01. |
| `bbox_project_graph_validate` | all; own; published. Detail refinements: errors row page; errors exact body | remaining | Error row and exact error pages implemented. Outer summary still collects every visible variant, unlike describe; high variant fanout needs a producer bound. Work: R05, V01. |
| `bbox_provenance_export` | export | restricted | Legacy all-project Git notes mutation has local CLI overlap. Later note-write failure loses already-completed counts; retain partial receipt and review real legacy consumers before removal. Work: R02, R07, V01. |
| `bbox_provenance_export_plan` | continuation; first page | retained | Bounded session-authoritative generation-bound plan has a real bro CLI consumer that restarts stale generations. Keep distinct from writing notes. Work: V01. |
| `bbox_provenance_import` | import | restricted | Local Git notes-to-sidecar operation; covered-project refusal precedes leases. Chooser should expose owner requirements; retirement needs legacy consumer/data review. Work: R06, R07, V01. |
| `bbox_ref_size` | file refs; indexed refs | restricted | Indexed-ref preflight works remotely; raw file refs need checkout authority. Ref cardinality alone does not bound arbitrary identities or aggregate reply bytes. Work: R05, V01. |

### [src/tools/knowledge.rs](../../../src/tools/knowledge.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_decide` | checkout-owner; global/local | retained | Operator-approved durable commitment with rationale and supersession. Owner admission, local persistence and publication remain distinct; transport repair is separately owned. Work: V01. |
| `bbox_forget` | checkout-owner; global/local | retained | Retirement/supersession differs from creating knowledge. Preserve explicit owner, approval and queued-publication semantics. Work: V01. |
| `bbox_knowledge` | exact system memory; packet category; query; system_memory category. Detail refinements: entry_detail; diagnostics_detail | remaining | Exact entry/system-memory/diagnostic pages and compact diagnostics implemented. Normal text still emits complete title/provider metadata; limit lacks a maximum, memory category ignores it, packet histograms grow. Work: R04, R05, R06. |
| `bbox_knowledge_link` | checkout-owner; global/local | retained | Durable knowledge relationships preserve owner and publication semantics; no new mutation acceptance claimed. Work: V01. |
| `bbox_learn` | checkout-owner; global/local | retained | Operator-approved rendered rules remain distinct from cold facts and active threads. Owner queue completion is not accepted publication. Work: V01. |
| `bbox_remember` | checkout-owner; global/local | retained | Cold durable recall retains operator approval and owner/publication distinctions, without ambient rendering. Work: V01. |

### [src/tools/mcp_surface.rs](../../../src/tools/mcp_surface.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_mcp_surface` | describe; list; replay. Detail refinements: replay policy page; describe policy page | adjusted | Typed actions, list-selector rejection, live row paging and exact policy/packet hints implemented and partly live checked. Complex predicates still display matches_surface=*; arbitrary policy strings and echoed selectors need growth proof. Work: R05, R06, R08. |

### [src/tools/notes.rs](../../../src/tools/notes.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_note` | create | adjusted | Host-owned project association uses catalog/filter identity without checkout write authority. Unresolved note projects deliberately preserve literal compatibility, unlike pin mutation; document the distinction. Work: R06, V01. |
| `bbox_note_resolve` | batch; single | remaining | Single/batch IDs validate before mutation and output is compact, but no batch cardinality/input-byte cap exists. Add a pre-effect bound without changing atomic selection validation. Work: R02. |
| `bbox_notes` | exact/full; summary. Detail refinements: exact JSON body cursor | adjusted | Live-offset summaries plus exact content-bound JSON readers implemented; stale and cross-note cursor tests exist. Exact reads preserve complete Unicode records. Work: V01. |

### [src/tools/packets.rs](../../../src/tools/packets.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_apply` | all; first. Detail refinements: findings page; exact result body | adjusted | First/all result summaries, finding pages and exact result JSON implemented. Exact reads suppress observation writes and bind input/result. Opaque entity and consequent validation still needs branch probes. Work: R06, V01. |
| `bbox_audit` | all; first. Detail refinements: mismatches page; exact result body | adjusted | Bounded mismatches/findings and complete exact result pages implemented. Changed input/result invalidates cursor; exact reads add no event. Probe malformed expected values and mode-specific skipped rows. Work: R06, V01. |
| `bbox_compile` | compile | remaining | Rules now include usable examples and restrictions, but nested JSON remains opaque and chooser is long. Keep compilation separate from evaluating/classifying entities. Work: R06, V01. |
| `bbox_packet_events` | query. Detail refinements: exact event body | adjusted | Closed operation/outcome enums, timestamp validation, append-aware cursor pages and exact event body implemented. Whole-log scan cost remains independent of response bounds. Work: R08, V01. |
| `bbox_packet_gap` | record | retained | Packet AST expressiveness has a distinct record surface; general MCP deficits use the gap store. Work: V01. |
| `bbox_packet_list` | detail; exact revision; summary | remaining | Row/byte pages and exact packet body exist. Detail classification histograms can overflow a row and require a summary retry; expose explicit live-offset and recovery semantics. Work: R04, R05, R06. |

### [src/tools/project_catalog.rs](../../../src/tools/project_catalog.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_project_attach` | attach | restricted | Local authority/identity marker required; transport-owned and remote-only attachment refuses. Recorded capabilities are observations. Stress alias nomination result fanout before claiming full bounds. Work: R05, V01. |
| `bbox_project_catalog_get` | aliases; attachments; observations; summary | retained | Aliases/attachments page with epoch refusal; observations returns one bounded projection and rejects paging controls. Recorded-authority wording is explicit. Nonzero get offset requires epoch; preserve that distinction from catalog list. Work: V01. |
| `bbox_project_catalog_list` | list | retained | Authoritative logical-project discovery in catalog mode, bounded rows with optional epoch continuation. Unavailable in bridge mode, so not a universal project_list replacement. Work: V01. |
| `bbox_project_default_attachment` | clear; set | retained | Path-free set/clear with epoch and bounded reason. Catalog choice is not present filesystem authority. Work: V01. |
| `bbox_project_detach` | detach | retained | Path-free transaction remains useful. census_row_removed=false conflates no row with lifecycle-guard failure; distinguish partial auxiliary cleanup from catalog detach. Work: R02, V01. |
| `bbox_project_promote` | apply | restricted | Existing local/offline authority lane with improved chooser and transport-owned refusal. No remote capability inferred from a truthful refusal. Work: V01. |
| `bbox_project_publisher_advance` | advance attachment apply; advance attachment preview; advance candidate apply; advance candidate preview; establish attachment apply; establish attachment preview; establish candidate apply; establish candidate preview | retained | Source exclusivity, candidate ref, CAS, dry-run and pointer-swap uncertainty implemented across establishment/advance branches. Preserve admission versus accepted publication. Work: V01. |
| `bbox_project_publisher_bind` | bind | restricted | Useful attachment-only rebinding; covered projects refuse. Put the restriction in chooser before the failing call. Work: R06, V01. |
| `bbox_project_publisher_status` | get. Detail refinements: health exact body; connector exact body | remaining | Compact health/connector and exact content/epoch-bound pages implemented; CAS identity survives. Raw auto_advance.last_attempt still escapes projection; detail_limit validation and chooser disagree. Work: R05, R06, R08. |
| `bbox_project_scope_migrate` | apply; preview | restricted | Local/offline authority and operator-owned acknowledgement remain explicit; transport-owned scope refuses before probes. Planning cost is separate from reply size. Work: R08, V01. |

### [src/tools/projects.rs](../../../src/tools/projects.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_project_eject` | apply; preview | restricted | Local-only migration of central knowledge into repo-owned files; the project stays registered. Partial repository, central-store and marker outcomes are now explicit; missing-root lifecycle belongs to its existing owner. Work: V01. |
| `bbox_project_init` | init | restricted | Checkout identity/config initialization remains useful. Lead chooser with the actual owning host/collector prerequisite. Work: R06, V01. |
| `bbox_project_list` | list | restricted | Unpaged compatibility root view still has bridge parity and documentation consumers. Catalog list is unavailable in bridge mode; establish bridge replacement before consolidation. Work: R04, R07. |
| `bbox_project_register` | register | restricted | Source-host checkout registration is distinct from logical catalog listing. Clarify owner prerequisite and catalog versus bridge follow-up reader. Work: R06, V01. |
| `bbox_project_rename` | apply; preview | restricted | Explicit local administrator operation, dry-run and partial failure reporting implemented. Transport-owned refusal is intentional. Work: V01. |
| `bbox_project_unregister` | apply; preview | restricted | Compatibility action has real mode split: bridge unregister versus catalog detach, with offline logical retirement guidance. Preserve useful data/consumers before consolidation. Work: R07, V01. |

### [src/tools/render.rs](../../../src/tools/render.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_absorb` | global; project | restricted | Callable compatibility no-op, now labeled and omitted from hot docs. Still does view/diagnostic work; no successful import outcome. Confirm external consumers before removing discovery. Work: R07, R08. |
| `bbox_bootstrap` | call | retired | Immediate compatibility refusal with replacement hint. Hidden from default discovery and hot docs, still served on ops; removal requires compatibility decision. Work: R07. |
| `bbox_lint` | get | remaining | Unpaged issue text and full titles/duplicate groups remain, followed by session diagnostics. Recovery hint uses obsolete blackbox_review; add bounded findings and correct reader. Work: R04, R05, R06, R08. |
| `bbox_render` | both; global; project | restricted | Host/global CLI and managed project owner lanes remain useful. Chooser hides prerequisites; appended diagnostics still need complete-envelope proof. Work: R05, R06, V01. |
| `bbox_review` | approve; list; reject. Detail refinements: get exact record | adjusted | Bounded live queue and exact record pages implemented. Local mutation validation exists, but queued approve/reject drops forbidden cursor/limit before validation; move validation before enqueue. Work: R02, R08, V01. |

### [src/tools/roadmap.rs](../../../src/tools/roadmap.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_roadmap` | create; default_template; delete; get; link; list; next; promote; render; repair_links; search; unlink; update | restricted | All 13 branches remain executable: state, links, ranking, promotion and rendering. Existing owner gap covers consumer/data decision. List/search lack continuation and exact get/render bodies remain unpaged; ownership of a retirement decision does not discharge those contracts. default_template is missing from action prose; repeated promote hints incorrectly use bro_resume for thread IDs. Work: R04, R05, R06, R07. |

### [src/tools/roster.rs](../../../src/tools/roster.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bro_brofile` | clear_provider_default; create; delete; get; get_provider_default; list; list_accounts; list_provider_defaults; set_account; set_provider_default. Detail refinements: get_account; list_accounts exact inventory | adjusted | Exact selected-scope brofile/account pages, redacted inventory and strict config mutation implemented. Read helpers still collapse malformed/unreadable stores to empty/not-found; some summary identities grow and project locality needs clarity. Work: R02, R04, R05, R06, V01. |
| `bro_dashboard` | summary | retained | Bounded status/filter projection and early selector validation implemented. Ordinary context telemetry stays omitted. Work: V01. |
| `bro_providers` | detail; summary | retained | Summary versus selected provider detail, invalid-provider refusal and peak advisories implemented. Discovery is not account membership or successful provider admission. Work: V01. |
| `bro_report` | call | remaining | Required message still stores and echoes unbounded message/needs/data before global response guard. Bound the admitted payload/receipt and expose exact report recovery. Work: R02, R05. |
| `bro_team` | create; delete_template; dissolve; get; get_template; list; list_templates; roster; save_template | adjusted | Bounded inventories, exact team/template pages, member caps, inert legacy advisors and project-template refusal implemented. Dissolve ignores cancellation/removal errors; delete failure can look like missing. Work: R02, R06, V01. |

### [src/tools/sessions.rs](../../../src/tools/sessions.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_embed_partitions` | list; prune apply; prune preview; scrub apply; scrub preview. Detail refinements: list exact inventory; prune exact preview inventory; prune route selector | adjusted | Validated list/prune/scrub choices, paged and exact preview inventory, explicit age/route, pre-effect prune batch refusal and partial deletion counts implemented. Scrub still scans one entire mapped partition; stress arbitrary route/entity fields. Work: R05, R08, V01. |
| `bbox_embed_status` | coverage; debug; diagnostics; recall probe; summary | retained | Coverage, graph diagnostics and recall probes remain explicit expensive opt-ins; cheap status preserves failures/losses. Default 64-route diagnostic selection is not a bound on all caller-supplied names or nested reports. Work: R05, R08, V01. |
| `bbox_messages` | native; retained-conversation | retained | Exact session/locator reads retain body/count bounds and native projection limitations; they do not read the caller filesystem. Work: V01. |
| `bbox_reembed` | start | retained | Starts convergence rather than pruning. Required bucket route supports all/backfill and explicit transcript guard; receipt says refill started, enqueue result only logged. Expose route vocabulary and observation limits. Work: R06, R08, V01. |
| `bbox_reindex` | queue; wait | retained | Queue admission and explicit wait-for-completion are separate. Empty-project authority stays operator supplied; no collected-source publication claim from a daemon scan. Work: V01. |
| `bbox_session` | get | retained | Exact indexed session metadata has a distinct role from message pages and global session discovery. Work: V01. |
| `bbox_sessions_list` | list | retained | Recency list has actual default/max 30/100; chooser lacks limits, project and empty-result semantics. Preserve source freshness caveats. Work: R06, V01. |
| `bbox_stats` | get | retained | Cached index/segment counts with stated 60-second cache limit; no source-freshness inference. Work: V01. |
| `bbox_topics` | get | retained | Top terms are a distinct session profile; not a substitute for reading evidence. Work: V01. |

### [src/tools/storage_gc.rs](../../../src/tools/storage_gc.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_storage_gc` | apply; preview; receipt candidates; receipt deleted; receipt errors; receipt exclusions; receipt full; receipt packets; receipt summary | retained | Preview/apply and immutable summary/candidate/deleted/error/exclusion/full/packet receipt readers stay distinct. Partial stages survive; reading receipts never authorizes or repeats deletion. Work: V01. |

### [src/tools/storage_health.rs](../../../src/tools/storage_health.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_storage_health` | files; summary | retained | Relative diagnostic file pages are byte bounded with explicit live offsets. Summary/files still scan before projection; paths identify daemon storage, not caller recovery files. Work: R08, V01. |

### [src/tools/storage_migration.rs](../../../src/tools/storage_migration.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_storage_migrate_legacy_edges` | apply; preview | adjusted | One selector resolver now handles preview/apply paths, IDs and aliases. Preview pages; apply refuses page selectors and requires one project. Still plans before slicing; specialist consumer review remains. Work: R07, R08, V01. |

### [src/tools/threads.rs](../../../src/tools/threads.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_thread` | continue; get; link; open; promote; rename; resolve. Detail refinements: get summary explicit/default; get notes; get sessions; get edges; get note; get handoff | remaining | Default summary, history and exact note/handoff pages implemented. Explicit summary fails live. Mutation bag ignores get-only fields; truncated topic/session/edge metadata need valid exact recovery breadcrumbs. Work: R04, R06, V01. |
| `bbox_thread_list` | list | retained | Bounded summaries and catalog/historical-path filters work without checkout leases. Last-activity ordering is a live view; updates can move rows. Work: V01. |

### [src/tools/tool_calls.rs](../../../src/tools/tool_calls.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_tool_calls` | query | retained | Typed kind/time, candidate-offset continuation and bounded previews exist; empty nonterminal pages are deliberate. Oversized locator reports recovery unavailable; full-envelope escaped-field proof remains. Work: R05, V01. |

### [src/tools/transcripts.rs](../../../src/tools/transcripts.rs)

| Tool | Actions / detail / authority branches | Disposition | Current contract and next work |
| --- | --- | --- | --- |
| `bbox_cite` | claim | retained | Claim-origin retrieval remains useful. A retrieved statement is not proof of its truth. Work: V01. |
| `bbox_context` | native; retained-conversation | retained | Native coordinate reads disclose indexed projection/freshness limits; retained conversation reads use scoped landing-store authority. Do not claim current source completeness from indexed retrieval. Work: V01. |
| `bbox_corpus_search` | query | retained | Real harness id/text consumer requires this compatibility shape. Empty-index path can trigger indexing; overlap in name does not justify retirement. Work: R08, V01. |
| `bbox_discover_seed_entities` | query | retained | Adds notable edges to hybrid retrieval and requires a complete graph read view; not an identical search alias. Work: V01. |
| `bbox_hybrid_search` | debug; default | retained | Default retains identity and degradation, debug adds ranking telemetry. Top-k is selection, not an exhaustive inventory; adversarial metadata/debug bounds remain verification work. Work: R05, V01. |
| `bbox_search` | fulltext; smart | adjusted | Typed SearchRecovery now routes native, retained conversation, thread and generic entity hits to valid readers or explicit unavailability. Current live query only exercised native hits; other types have inspected regressions. Work: V01. |

## Dependency and closeout boundary

The existing gaps for checkout delivery/replay (gap-92bd3d34), provenance
readiness (gap-71db0a11), producer onboarding (gap-78c4fa64), missing-checkout
retirement (gap-0f2ec093) and commit-to-publication latency (gap-f48dd98d) remain
the named ownership references from the handoff. Their present failure state
was not re-tested in this pass, so none is asserted as a current blocker. Inspect
the precise dependency only when a surface claim requires it.

Roadmap gap-56c74f23 was read live: unresolved, blocking_level=none. It owns a
consumer/data decision, not a prerequisite blocking unrelated caller fixes.
The broad audit gap-7a2513c9 is updated in place and stays unresolved; original
pre-fix examples are preserved in the historical checkpoint. The thread remains
open for implementation and branch acceptance, with R01 first.

Close rows on evidence for that branch. Retention rationale, green suites,
catalog counts, safe refusal and working summary pagination alone do not prove
exact recovery, durable effects, confidentiality or every authority mode.
