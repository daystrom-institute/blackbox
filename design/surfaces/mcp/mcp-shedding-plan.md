---
title: "MCP Surface and Internals Shedding Plan"
kind: design
lifecycle: proposed
corpus: blackbox-design
topic:
  - surfaces
  - mcp
brief: "Cuts to the MCP tool surface and the internals behind it, sequenced to land before the rmcp 3 migration: surfaces as configuration, per-tool dispositions, knowledge model slimdown, provenance and packet removal, finished migration code."
---

# MCP Surface and Internals Shedding Plan

This plan shrinks the MCP tool surface and the internals that exist only to
serve it. It lands before the [rmcp 3.0 Migration Plan](rmcp-3-migration-plan.md):
every tool and store removed here is one fewer contract carried through the
migration's per-request wire head (Phase 1), task projection (Phase 2) and
resource projection (Phase 4). Phase 0 cost does not depend on tool count;
the later phases do.

## Dispositions

Every served tool gets exactly one disposition:

- **Keep**: stays on agent-facing surfaces.
- **Ops-only**: removed from every agent-facing surface (`default`,
  `interactive`, `agent-internal`, `readonly`) and kept on `ops`. Operators
  run it with `bro mcp call <tool> '<json>' --surface ops`; friendlier
  `bro` subcommands are optional sugar over that path. The handler and its
  logic stay.
- **Delete**: handler, parameters, docs and references removed. Backing
  logic goes too unless another survivor uses it.
- **Fold**: capability merges into a named survivor, then the tool is
  deleted.
- **Open**: needs an operator decision before it moves.

Context cost is not the main reason for these cuts. Claude Code, Codex and
the bro-harness all defer MCP schemas behind tool search, so a session
carries tool names up front and loads schemas on demand. The costs that
scale with tool count are near-miss schemas in tool-search results, per-tool
response contracts and tests, and the migration phases above.

## Stage 1: surfaces become configuration

Surface routing today is a rule packet in the `mcp-surface/routing` domain.
Every rule matches only the `surface` field, returning allow, disallow and
instructions, plus a final rule that denies unknown surfaces. That is a
lookup table.

Target shape, in daemon configuration:

```toml
[surfaces.interactive]
disallow = ["bbox_project_*", "bbox_storage_*"]
instructions = "..."

[surfaces.readonly]
allow = ["bbox_hybrid_search", "bbox_inspect_entity", "bbox_knowledge"]
instructions = "..."
```

- Same semantics as today: glob patterns; a non-empty `allow` is an
  allowlist; `disallow` wins over `allow`; an unknown surface is refused at
  connection time.
- Enforcement in `src/server/surface.rs` is unchanged: `list_tools`
  filters, `call_tool` rejects hidden tools.
- Surfaces load with daemon configuration. A change applies on config
  reload or restart; there is no runtime install path and no packet
  versioning.
- Phase 1 of the rmcp plan resolves the surface per request from this
  static map; the generation-keyed surface decision cache is no longer
  needed. Deny semantics (Q5) reduce to "unknown surface".
- Phase 4's resource dimension becomes a `resources` key on the same table.

Deleted with this stage: `bbox_mcp_surface`, `system-defaults/mcp-surfaces/`
routing packets, and the surface-packet evaluation path.

This stage comes first because it is the lever for every ops-only
disposition below: each becomes one config edit.

## Stage 2: tool dispositions

| Family | Keep | Ops-only | Delete / Fold | Open |
| --- | --- | --- | --- | --- |
| Retrieval | `bbox_hybrid_search`, `bbox_context`, `bbox_messages`, `bbox_session`, `bbox_sessions_list`, `bbox_inspect_entity` | `bbox_stats` | Fold `bbox_search` into `bbox_hybrid_search`. Delete `bbox_cite`, `bbox_topics`, `bbox_discover_seed_entities`, `bbox_ref_size`. Delete `bbox_corpus_search` and repoint the harness `corpus_search` alias at `bbox_hybrid_search`. | `bbox_find_paths`, `bbox_bundle_evidence`, `bbox_describe_schema`, `bbox_tool_calls` |
| Knowledge | `bbox_knowledge`, `bbox_learn`, `bbox_forget`, `bbox_render` | | Fold `bbox_remember` into `bbox_learn` (`render=false`). Delete `bbox_decide`, `bbox_knowledge_link`, `bbox_lint`, `bbox_review`, `bbox_absorb`, `bbox_bootstrap`. | |
| Work tracking | `bbox_thread`, `bbox_thread_list`, `bbox_gap`, `bbox_gaps`, `bbox_gap_update`, `bbox_gap_resolve` | | Delete `bbox_inbox`, `bbox_pin` | `bbox_note`, `bbox_notes`, `bbox_note_resolve` |
| Dispatch | `bro_exec`, `bro_resume`, `bro_status`, `bro_wait`, `bro_when_all`, `bro_when_any`, `bro_steer`, `bro_cancel`, `bro_dashboard`, `bro_providers`, `bro_brofile` | `bro_prune`, `bro_allocator_status`, `bro_allocator_trace`, `bro_allocator_probe`, `bro_mcp` | Delete `bro_retro`, `bro_broadcast`, `bro_interrupt`, `bro_report`, `bro_agent_list`, `bro_agent_get`, `bro_agent_describe`, `bro_agent_search`, `bro_agent_dispatch` | `bro_team` |
| Projects | `bbox_project_list` | `bbox_project_register`, `_init`, `_rename`, `_unregister`, `_eject`, `_catalog_list`, `_catalog_get`, `_attach`, `_detach`, `_default_attachment`, `_promote`, `_publisher_bind`, `_publisher_advance`, `_publisher_status`, `bbox_project_graph_list`, `_describe`, `_validate` | Delete `bbox_project_scope_migrate` (Stage 6) | |
| Index and storage | | `bbox_reindex`, `bbox_reembed`, `bbox_embed_status`, `bbox_embed_partitions`, `bbox_storage_gc`, `bbox_storage_health`, `bbox_edge_compact`, `bbox_doctor` | Delete `bbox_storage_migrate_legacy_edges` (Stage 6) | |
| Artifacts | | `bbox_artifact_install`, `_list`, `_remove`, `_supersede` | | Re-check once packets are gone |
| Packets | | | Delete `bbox_compile`, `bbox_apply`, `bbox_audit`, `bbox_packet_list`, `bbox_packet_events`, `bbox_packet_gap` (Stage 3) | |
| Provenance | | | Delete `bbox_blame`, `bbox_provenance_export`, `_export_plan`, `_import` (Stage 5) | |
| Surfaces | | | Delete `bbox_mcp_surface` (Stage 1) | |

End state: 28 agent-facing tools, 35 ops-only, 37 deleted or folded, 8 open.

Per-tool notes:

- `bbox_absorb` and `bbox_bootstrap` are compatibility stubs: one is a
  no-op, the other returns a refusal. Nothing depends on either.
- `bbox_pin`: arc-bound context belongs in the dispatch brief or the
  thread. Removing it also removes the pin store (`bbox-stores` pins),
  dispatch-time pin injection, the `scoped-pins` system memory, and step 4
  of the persistence guide's lane ladder.
- `bro_interrupt` is covered by `bro_cancel` plus `bro_resume`, and
  `bro_steer` for mid-turn input.
- `bbox_project_list` stays as the one agent-facing project reader so
  agents can resolve project ids and aliases. An alternative is to list
  known projects in the error when a `project` selector does not resolve.
- `bbox_render` stays agent-facing because agents render after a knowledge
  write. Rendering on write would let it move to ops-only.
- The project onboarding skill (`src/server/onboarding_skill.rs`) names
  `bbox_project_register`, `bbox_project_catalog_list` and
  `bbox_project_publisher_status`; it switches to the CLI form, which
  matches registration being a checkout-host operation. `doctor.rs` and
  error remediation strings that name ops-only tools switch to the CLI form
  the same way.
- `bro_report`: dispatched work is followed through `bro_status`,
  `bro_wait` and `bro_dashboard`. Its task report store, the roster
  `report` fields, `bro_status detail=report` and the dispatch milestone
  directive go with it.

Dispatch context: the `recall`, `task_shape` and `orchestrator` directives
and the per-turn directive cadence (protocol variant, harness volatile-lane
injection) are removed. The completion contract is the only directive; it
moves with the notes decision, and the directive plumbing (cadence,
`needs_scope`, standing-slot routing) goes once it has a home. The dispatch
context then carries persona and the `<bbox_scope>` block.

Every deletion also scrubs the tool from the generated tool reference
(`bbox-tool-docs`), system memories, prompts, docs, surface config, brofile
tool allowlists (`src/orchestration/brofile.rs`) and harness surface tests.

## Stage 3: packet engine removal

Surface routing is the packet engine's only live runtime consumer once
Stage 1 lands. The one other code path, an arc-bound warning inside
`bbox_learn`, applies a `content-classification/arc-bound` packet only if
one is installed, and none is. The remaining installed packets are one-off
rubrics with no code consumer.

Deleted: `bbox_compile`, `bbox_apply`, `bbox_audit`, `bbox_packet_list`,
`bbox_packet_events`, `bbox_packet_gap`; the `bbox-packets` crate; the
packet store and event log; the packet artifact kind and the default
packets under `system-defaults/agentic-corpus/packets/`; the arc-bound
warning; the `sm-rule-packets` system memory and the rule-packet section of
the tool guidance. `bbox-gaps`, `bbox-indexing` and `bbox-providers` drop
their packet dependencies.

## Stage 4: knowledge model slimdown

Knowledge keeps one write lane (`bbox_learn`), one reader
(`bbox_knowledge`), retirement (`bbox_forget`) and rendering
(`bbox_render`). Project entries are one JSON file per entry under
`.bbox/knowledge/`, so git history carries prior versions and the reason
for each change.

Entry fields kept: `id`, `title`, `content`, `category`, `cluster`,
`scope`, `project`, `project_id`, `priority`, `render`, `created_at`,
`updated_at`.

Fields removed:

| Field or mechanism | Replacement |
| --- | --- |
| `supersedes`, `superseded` status | `bbox_forget` plus a new entry; git history for project entries |
| `links`, the edge kinds, `bbox_knowledge_link` | none |
| `rationale` | part of `content` |
| `variants` (per-provider text) | none |
| `expires_at`, `review_at`, `render_placement` | none |
| `status` other than active | deletion |
| `approval` states and the review queue | approval happens before the write |
| `source` | none |
| `weight` | ordering by priority, then title |

Candidates to confirm while implementing: `providers` (few entries use
it), `recall_count` and `last_recalled` (a ranking signal that writes on
every read), and `decay` (confirm whether anything ages entries).

The `decision` category goes. `AcceptedKnowledgeCategoryV1::Decision` is a
versioned publication schema value, so already-published rows need a read
mapping or a one-time rewrite, not just a deleted variant. Existing
decision entries are triaged with the operator: ledger rows are deleted;
real invariants are re-saved as `convention` with the rationale folded
into the text, each re-saved text approved by the operator.

`bbox_remember` becomes `bbox_learn` with `render=false`. The persistence
guide, `persistence-taxonomy`, `render-lifecycle` and `docs/knowledge-store.md`
describe the single lane.

## Stage 5: tool-call provenance removal

Blame and provenance join a code line's `git blame` commit to the commit
stamped on file-touch edges. That stamp is the checkout HEAD when the
indexer processed the event, so backfill and reindex attach later commits
to earlier edits; squash and rebase orphan the stamps; edits made outside
indexed sessions never get edges. Line-to-session attribution cannot be
made reliable on this basis.

Deleted:

- `bbox_blame`, `bbox_provenance_export`, `bbox_provenance_export_plan`,
  `bbox_provenance_import`; `bro blame` and `bro provenance`.
- `blame.rs`, `provenance.rs` and `provenance_plan.rs` in `bbox-mcp-tools`.
- The `bbox-provenance` crate (the Git-note protocol) and its use in
  `bbox-git-source` and `bbox-code-collector`;
  `src/server/provenance_import.rs` and `provenance_authority.rs`.
- `EDITED_FILE` and `READ_FILE` edge emission in `tool_edges.rs`, the
  `anchor.*` metadata, `commit_anchor_index` in `EdgeIndex`, their
  sidecar and migration handling, the file-to-transcript neighbour views in
  `bbox-providers`, and their entries in `describe_schema`.
- The stored file-touch edges.

"Which sessions touched this file" remains answerable through transcript
search on the path.

## Stage 6: finished migration code and dead crates

- **Project catalog v1 migration**: committed on the primary deployment.
  Deleted: `project_catalog_migration.rs` and its facade test, the
  migration lock, `bbox_project_scope_migrate`, and the offline
  `blackbox project-catalog` command. The daemon refuses a pre-migration
  catalog instead of migrating it.
- **Cutovers and overlap code**: git transport, knowledge transport,
  code-source locality, render locality and blame locality cutovers;
  migration inventories in the edge sidecar, corpus index and vectors;
  `legacy_migration.rs`, `bridge_parity.rs`, `resolver_compat.rs`, and
  `bbox_storage_migrate_legacy_edges`. Each goes once its completion is
  confirmed on every live deployment. The knowledge "legacy compatibility"
  lane goes with the knowledge transport cutover.
- **Dead crate**: `bbox-source-graph` has no dependents.
- **Retired-tool families still wired in**: `bbox-whiteboards` (capture and
  discharge in the binary, catalog stamper, graph provider),
  `bbox-system-events`, and `bbox-inbox` with `bbox_inbox`.

## Open decisions

- **Graph family.** After Stage 5 the edge index holds thread, session,
  brofile and knowledge relationships. Decide whether `bbox_find_paths`,
  `bbox_bundle_evidence` and `bbox_describe_schema` still earn their place,
  and with them how much of the edge store and sidecar remains.
  `bbox_inspect_entity` stays as the reader for refs returned by search.
- **Notes, gaps and threads.** Three work-tracking stores overlap. Decide
  whether notes fold into gaps or threads.
- **Knowledge publishing machinery.** Provisional and published overlays
  and the knowledge-source crates publish project entries that already live
  in git. Revisit once Stage 4 has slimmed the model.
- **`bbox_tool_calls`, `bro_team`.** Low use; keep or delete.

## Sequencing

1. Stage 1 (surfaces as config), then the Stage 2 ops-only moves and plain
   deletions, which become config edits and handler removals.
2. Stage 3 (packets), which depends on Stage 1.
3. Stages 4 and 5, independent of each other.
4. Stage 6, gated on per-deployment completion checks.
5. rmcp migration Phase 0.

The [target-surface doc](mcp-2026-07-28-target-surface.md)'s task
candidates and resource catalogs are revised against the surviving surface
before Phase 2 and Phase 4 start.

## Validation

Per stage: `cargo check`, `cargo nextest run --workspace`, `cargo clippy`
and `scripts/lint-concurrency.sh`, lane-side. Then:

- `tools/list` on each agent-facing surface returns exactly the Keep set;
  on `ops` it returns Keep plus Ops-only.
- An ops-only tool round-trips through `bro mcp call --surface ops`.
- A hidden tool called on an agent-facing surface is rejected.
- An unknown surface is refused.
- A search for each removed tool name finds no references outside git
  history.
- `bbox_render` output and `bro render global` show no removed tools in
  generated guidance.
