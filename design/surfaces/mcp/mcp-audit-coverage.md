---
title: "MCP audit coverage"
kind: design
corpus: blackbox-design
lifecycle: proposed
topic:
  - surfaces
  - mcp
brief: "Complete registered-tool inventory grouped by adapter owner and audit finding."
---

# MCP audit coverage

The [orchestration retirement](../../orchestration/bro-execution-boundary-and-retirement.md)
is deployed from `2dd385a2c6f4`. Its
[exact disposition map](../../orchestration/bro-execution-retirement-map.json)
passes against the live ops catalog: all 82 retired names are absent, with 109
survivors including the new bounded `bbox_tool_calls` history reader. The
inventory below is the original 190-tool audit baseline, not a current catalog.
Retirement does not mark every surviving action as audited. Native background
collection and the bro execution boundary are deployed and verified.

Source snapshot: `919b8f4a`, 2026-09-05. The inventory enumerates all 190
named tool declarations in 30 adapter files under `src/tools`. The connected
session exposes 106 of these tools; specialist and restricted families still
belong in the audit. Names below are exact, not wildcard approximations.

See the [audit](mcp-response-and-contract-audit.md) for evidence, priorities,
recommended contracts, and validation. Every family has been triaged; the table
does not label unprobed actions as passing. Findings F01 (whole-response sizing)
and F12 (outcome semantics) are cross-cutting, including families where they
are not repeated below. No production mutations were used to test these tools.

| Adapter owner | Registered tools | Review focus |
| --- | --- | --- |
| [agents.rs](../../../src/tools/agents.rs) | `bro_agent_list`, `bro_agent_get`, `bro_agent_describe`, `bro_agent_search`, `bro_agent_dispatch` | F06, F07, F10: Page summaries; distinguish installed manifest from resolved dispatch contract; keep callable input requirements. |
| [artifacts.rs](../../../src/tools/artifacts.rs) | `bbox_artifact_install`, `bbox_artifact_list`, `bbox_artifact_supersede`, `bbox_artifact_remove` | F03, F06, F09: Replace caller-local install assumptions; page artifact summaries; remove storage paths and audit raw configuration serialization. |
| [atoms.rs](../../../src/tools/atoms.rs) | `atom_list`, `atom_get`, `atom_describe`, `atom_search`, `atom_invoke`, `atom_status`, `atom_resume`, `atom_delegate` | F06, F07, F08, F10: Page discovery; separate invocation state from trace while preserving effects/obligations; clarify get/describe. |
| [attention.rs](../../../src/tools/attention.rs) | `bbox_pin`, `bbox_inbox` | F03, F05, F06: Bound aggregate inbox dimensions and pin bodies; preserve scope/lifecycle; make import side effects explicit. |
| [badgey.rs](../../../src/tools/badgey.rs) | `badgey_exec`, `badgey_resume`, `badgey_ask`, `badgey_dismiss`, `badgey_status`, `badgey_list`, `badgey_scout`, `badgey_collect`, `badgey_triage_inbox`, `badgey_close_loops`, `badgey_proposals_list`, `badgey_ensure_for_channel`, `badgey_apply_proposal`, `badgey_proposal_begin_apply`, `badgey_proposal_complete_apply` | F06, F08, F10, F12: Page proposals and instances; separate status/drafts/events; evaluate question alias and pinned proposal shims. |
| [config.rs](../../../src/tools/config.rs) | `bro_mcp`, `bro_slack_bind`, `bro_slack_link_record`, `bro_slack_link_lookup` | F03, F07, F09: Identify configuration owner; redact credentials; type action-specific requirements; retain channel identity semantics. |
| [consultant/mod.rs](../../../src/tools/consultant/mod.rs) | `consultant_proposals_list`, `consultant_apply_proposal`, `consultant_proposal_begin_apply`, `consultant_proposal_complete_apply` | F06, F10, F12: Page proposal summaries and exact drafts; preserve begin/complete workflow semantics during consolidation. |
| [dispatch.rs](../../../src/tools/dispatch.rs) | `bro_exec`, `bro_resume`, `bro_allocator_status`, `bro_allocator_trace`, `bro_allocator_probe`, `bro_wait`, `bro_when_all`, `bro_when_any`, `bro_broadcast`, `bro_status`, `bro_prune`, `bro_retro`, `bro_cancel`, `bro_steer`, `bro_interrupt` | F03, F07, F08, F12: Status/result separation, explicit worker locality, bounded fan-out results, and honest failure/admission. |
| [doctor.rs](../../../src/tools/doctor.rs) | `bbox_doctor` | F03, F13: Useful summary precedent; separate format from detail and label server-owned diagnostics. |
| [gaps.rs](../../../src/tools/gaps.rs) | `bbox_gap`, `bbox_gaps`, `bbox_gap_resolve`, `bbox_gap_update` | F05, F06, F12: Summary DTO, exact detail, bounded pages, scoped diagnostics, and publication-aware mutation receipts. |
| [graph.rs](../../../src/tools/graph.rs) | `bbox_inspect_entity`, `bbox_project_graph_list`, `bbox_project_graph_describe`, `bbox_project_graph_validate`, `bbox_describe_schema`, `bbox_find_paths`, `bbox_bundle_evidence`, `bbox_ref_size`, `bbox_edge_compact`, `bbox_blame`, `bbox_provenance_export`, `bbox_provenance_export_plan`, `bbox_provenance_import` | F02, F03, F07, F11, F12: Compact orientation/inspection/bundles, typed failures, actual edge vocabulary, generation-aware provenance and owner-side filesystem operations. |
| [knowledge.rs](../../../src/tools/knowledge.rs) | `bbox_learn`, `bbox_remember`, `bbox_decide`, `bbox_knowledge`, `bbox_knowledge_link`, `bbox_forget` | F05, F07, F12: Bound primary results and sidecars separately; keep scoped provenance; make exact expansion and publication state clear. |
| [mcp_surface.rs](../../../src/tools/mcp_surface.rs) | `bbox_mcp_surface` | F07, F10, F13: Retain as specialist discovery/debugging; type actions; avoid injecting replay diagnostics into general chooser. |
| [notes.rs](../../../src/tools/notes.rs) | `bbox_note`, `bbox_notes`, `bbox_note_resolve` | F05, F06: Good preview/full split; add continuation and explicit aggregate bounds; preserve resolution and scope. |
| [orchestrate.rs](../../../src/tools/orchestrate.rs) | `bro_orchestrate_author`, `bro_orchestrate_run`, `bro_arc_signal`, `bro_arc_status`, `bro_arc_result`, `bro_arc_cancel`, `bro_signals`, `bro_webhook_replay`, `bro_webhook_deliveries`, `bro_webhook_install`, `bro_webhook_list`, `bro_webhook_remove`, `bro_poller_install`, `bro_poller_list`, `bro_poller_remove`, `bro_cron_install`, `bro_cron_list`, `bro_cron_remove`, `bro_cron_upcoming`, `bro_workflow_install`, `bro_workflow_list`, `bro_workflow_remove` | F03, F06, F07, F08, F09, F12: Page registry/arc lists, project summaries instead of full specs, expose install schemas, scope secrets and separate state from histories. |
| [packets.rs](../../../src/tools/packets.rs) | `bbox_compile`, `bbox_apply`, `bbox_audit`, `bbox_packet_list`, `bbox_packet_events`, `bbox_packet_gap` | F06, F07, F13: Keep useful summaries and deterministic evaluation; add continuation; bound event/audit mismatch detail and expose AST schema. |
| [project_catalog.rs](../../../src/tools/project_catalog.rs) | `bbox_project_catalog_list`, `bbox_project_catalog_get`, `bbox_project_attach`, `bbox_project_detach`, `bbox_project_default_attachment`, `bbox_project_promote`, `bbox_project_scope_migrate`, `bbox_project_publisher_bind`, `bbox_project_publisher_advance`, `bbox_project_publisher_status` | F03, F06, F12: Page identities; move attachment detail off default; preserve epoch/publication state; require checkout-owner proofs for mutations. |
| [projects.rs](../../../src/tools/projects.rs) | `bbox_project_register`, `bbox_project_init`, `bbox_project_rename`, `bbox_project_unregister`, `bbox_project_eject`, `bbox_project_list` | F03, F10, F12: Reconcile legacy and catalog operations; refuse unsupported locality before side effects; report partial completion precisely. |
| [render.rs](../../../src/tools/render.rs) | `bbox_render`, `bbox_absorb`, `bbox_lint`, `bbox_review`, `bbox_bootstrap` | F03, F10, F13: Owner-side render/bootstrap; retire no-op absorb from normal discovery; keep scoped lint/review findings. |
| [roadmap.rs](../../../src/tools/roadmap.rs) | `bbox_roadmap` | F03, F07, F10: Resolve graph-native retirement and consumer migration before expanding the action bag or polishing local render. |
| [roster.rs](../../../src/tools/roster.rs) | `bro_dashboard`, `bro_report`, `bro_providers`, `bro_brofile`, `bro_team` | F03, F06, F07, F08, F09: Prior fixes landed; further remove daemon-binary availability, reject invalid filters, redact accounts and bound report/rollup dimensions. |
| [sessions.rs](../../../src/tools/sessions.rs) | `bbox_session`, `bbox_messages`, `bbox_reindex`, `bbox_reembed`, `bbox_embed_partitions`, `bbox_embed_status`, `bbox_topics`, `bbox_sessions_list`, `bbox_stats` | F02, F03, F06, F12, F13: Repair native ingestion/drill-down; keep retrieval limits and health opt-ins; report source freshness and queued maintenance honestly. |
| [storage_gc.rs](../../../src/tools/storage_gc.rs) | `bbox_storage_gc` | F03, F06, F13: Server-owned candidate detail is useful for approval; bound pages with stable plan identity and retain operator controls. |
| [storage_health.rs](../../../src/tools/storage_health.rs) | `bbox_storage_health` | F03, F13: Keep aggregate default; make per-file detail bounded and explicitly server-owned. |
| [storage_migration.rs](../../../src/tools/storage_migration.rs) | `bbox_storage_migrate_legacy_edges` | F03, F10, F12, F13: Specialist migration surface; review continued need and concrete plan/completion semantics, not default caller exposure. |
| [system_events.rs](../../../src/tools/system_events.rs) | `system_event_emit`, `system_event_list`, `system_event_open`, `system_event_compact`, `reaction_install`, `reaction_list`, `reaction_replay`, `reaction_execute`, `reaction_deliveries`, `reaction_retry`, `identity_list`, `identity_get` | F06, F07, F09, F13: Summary/event-detail split; bound causation fan-out; expose reaction spec schemas; redact integration payloads where needed. |
| [threads.rs](../../../src/tools/threads.rs) | `bbox_thread`, `bbox_thread_list` | F03, F06, F07, F12: Page continuity scans, keep lifecycle and real handoff handles, validate actions and mutation completion. |
| [transcripts.rs](../../../src/tools/transcripts.rs) | `bbox_corpus_search`, `bbox_search`, `bbox_hybrid_search`, `bbox_discover_seed_entities`, `bbox_cite`, `bbox_context` | F02, F03, F04, F07: Source-neutral retrieval; concise ranked evidence and scoped degradation; retain required compatibility projection. |
| [whiteboards.rs](../../../src/tools/whiteboards.rs) | `whiteboard_open`, `whiteboard_register`, `whiteboard_post`, `whiteboard_state`, `whiteboard_annotate`, `whiteboard_vote`, `whiteboard_transition`, `whiteboard_conflicts`, `whiteboard_summarize`, `whiteboard_archive` | F06, F07, F08: Bound post/annotation/vote histories; summaries and exact expansion must preserve blind-phase and role visibility. |
| [workspace.rs](../../../src/tools/workspace.rs) | `work_tool_calls`, `work_smart_read`, `work_bash`, `work_git_status`, `work_git_log`, `work_git_diff`, `work_git_show`, `work_git_commit` | F02, F03, F07, F10: Distinguish indexed tool-call recall from worker-owned execution; keep workflow restriction and bound stdout/read/diff bodies. |

## Inventory method

The name inventory was extracted from `#[tool(name = "...")]` declarations,
then checked for uniqueness and grouped by source owner. Router declaration
count is 190, consistent with the concurrency lint count from the preceding
implementation checks. This is a source declaration count, not a measurement
of every possible packet-selected session catalog.

Re-run the declaration scan after changing the router. Review changed parameter
schemas and handler projections with their domain DTOs; the same persistence
type can serve multiple adapters and need different projections. The inventory
intentionally stores no live response bodies or installation-specific objects.


## Verification checkpoints

The original inventory above is a triage snapshot, not an exhaustive execution
claim. The response audit records subsequent deployed milestones, exact
verification revisions, and measured complete result sizes. Live checks cover
provider/brofile discovery, a synthetic Astra dispatch and result continuation,
retrieval/gaps, native transcript backfill and drill-down, atom pagination,
agent/workflow/trigger catalogs, event/health/doctor summaries, graph stored
property pages and bundles, project catalog detail selection, arc JSON pages,
and invalid cron admission. Synthetic tests cover confidentiality, oversized
continuation, schema contracts, scope refusal, and partial mutation outcomes.
Proposal dependency installation changed live artifact/runtime state, but no
Slack post was sent. Remaining host permission and upstream-content limitations
are stated separately from verified tool behavior.
