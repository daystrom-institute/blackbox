---
title: "MCP surviving surface inventory"
kind: design
lifecycle: implemented
corpus: blackbox-design
topic:
  - surfaces
  - mcp
brief: "The 64 served MCP tools by owner, kind, agent-facing surface visibility and 2026-07-28 protocol projection."
---

# MCP surviving surface inventory

The daemon serves 64 MCP tools from 16 adapter files under `src/tools/`. This
is the current inventory the
[target surface](mcp-2026-07-28-target-surface.md) and the
[rmcp migration plan](rmcp-3-migration-plan.md) project onto protocol shapes.
The [surviving action audit](mcp-survivor-action-audit.md) and
[audit coverage](mcp-audit-coverage.md) describe a 109-tool catalog at their
own source revisions; a tool absent from the tables below is not served and
answers with the ordinary unknown-tool error.

## Surfaces

Surfaces are the built-in table in
`crates/bbox-config/src/default_surfaces.toml`, merged with daemon
configuration. Visible tool counts for the built-in table:

| Surface | Visible tools | Shape |
| --- | --- | --- |
| `ops` | 64 | Full catalog |
| `default` | 28 | Everything except the operator tools |
| `interactive` | 28 | Same set as `default` |
| `agent-internal` | 25 | `default` without `bro_exec`, `bro_resume`, `bro_cancel` |
| `readonly` | 11 | Allowlist of transcript, search, knowledge and status reads |

36 tools are visible on `ops` only: artifact management, project
registration and catalog administration, the publisher tools, the project
graph tools, index and embedding maintenance, storage tools, the allocator
tools, `bro_mcp`, `bro_prune`, `bbox_doctor` and `bbox_stats`. An agent on
any other surface reaches project graphs only through `bbox_hybrid_search`
and `bbox_inspect_entity`.

## Reading the tables

- **Kind** is what a call does: a read, a search, a catalog read (bounded
  pages over durable ids), a mutation, a dispatch or dispatch control, a
  blocking read (holds the response with progress ticks), maintenance
  (queued or staged background work), or an admin mutation (project and
  catalog administration requiring checkout or operator authority).
- **Agent-facing surfaces** lists `default`, `interactive`,
  `agent-internal` and `readonly` membership; `all` means all four and
  `ops only` means none of them. Every tool is on `ops`.
- **Protocol projection** names the 2026-07-28 shape the target surface
  assigns: `task` (a task handle in place of a bespoke background
  lifecycle), `tasks/get` and `tasks/cancel` (the extension method the tool
  maps to), `resource:` (the `blackbox://` catalog it enumerates), `live
  view` (a `ttlMs` read), and `approval` (an operator confirmation that can
  become an elicitation round). `none` means the tool stays a plain tool.

### `src/tools/artifacts.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_artifact_install` | mutation | ops only | none |
| `bbox_artifact_list` | catalog read | ops only | resource: artifact receipts |
| `bbox_artifact_remove` | mutation | ops only | none |
| `bbox_artifact_supersede` | mutation | ops only | none |

### `src/tools/config.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bro_mcp` | read and mutation | ops only | none |

### `src/tools/dispatch.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bro_allocator_probe` | read and mutation | ops only | none |
| `bro_allocator_status` | diagnostic read | ops only | none |
| `bro_allocator_trace` | diagnostic read | ops only | none |
| `bro_cancel` | dispatch control | default, interactive | tasks/cancel |
| `bro_exec` | dispatch | default, interactive | task |
| `bro_prune` | maintenance | ops only | none |
| `bro_resume` | dispatch | default, interactive | task |
| `bro_status` | read | all | tasks/get |
| `bro_steer` | dispatch control | default, interactive, agent-internal | none |
| `bro_wait` | blocking read | default, interactive, agent-internal | none |
| `bro_when_all` | blocking read | default, interactive, agent-internal | none |
| `bro_when_any` | blocking read | default, interactive, agent-internal | none |

### `src/tools/doctor.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_doctor` | diagnostic read | ops only | none |

### `src/tools/gaps.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_gap` | mutation | default, interactive, agent-internal | none |
| `bbox_gap_resolve` | mutation | default, interactive, agent-internal | none |
| `bbox_gap_update` | mutation | default, interactive, agent-internal | none |
| `bbox_gaps` | catalog read | default, interactive, agent-internal | resource: gap |

### `src/tools/graph.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_edge_compact` | maintenance | ops only | task; approval |
| `bbox_inspect_entity` | read | all | none |
| `bbox_project_graph_describe` | catalog read | ops only | resource: project graph |
| `bbox_project_graph_list` | catalog read | ops only | resource: project graph |
| `bbox_project_graph_validate` | diagnostic read | ops only | none |

### `src/tools/knowledge.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_forget` | mutation | default, interactive, agent-internal | none |
| `bbox_knowledge` | catalog read and search | all | resource: knowledge, system memory |
| `bbox_learn` | mutation | default, interactive, agent-internal | none |

### `src/tools/project_catalog.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_project_attach` | admin mutation | ops only | none |
| `bbox_project_catalog_get` | catalog read | ops only | resource: catalog project |
| `bbox_project_catalog_list` | catalog read | ops only | resource: catalog project |
| `bbox_project_default_attachment` | admin mutation | ops only | none |
| `bbox_project_detach` | admin mutation | ops only | none |
| `bbox_project_promote` | admin mutation | ops only | none |
| `bbox_project_publisher_advance` | admin mutation | ops only | approval |
| `bbox_project_publisher_bind` | admin mutation | ops only | approval |
| `bbox_project_publisher_status` | diagnostic read | ops only | live view |
| `bbox_project_scope_migrate` | admin mutation | ops only | none |

### `src/tools/projects.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_project_eject` | admin mutation | ops only | approval |
| `bbox_project_init` | admin mutation | ops only | none |
| `bbox_project_list` | catalog read | all | none |
| `bbox_project_register` | admin mutation | ops only | task |
| `bbox_project_rename` | admin mutation | ops only | none |
| `bbox_project_unregister` | admin mutation | ops only | approval |

### `src/tools/render.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_render` | mutation | default, interactive, agent-internal | none |

### `src/tools/roster.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bro_brofile` | read and mutation | default, interactive, agent-internal | resource: brofile |
| `bro_dashboard` | catalog read | all | live view |
| `bro_providers` | catalog read | default, interactive, agent-internal | resource: provider |

### `src/tools/sessions.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_embed_partitions` | maintenance | ops only | none |
| `bbox_embed_status` | diagnostic read | ops only | none |
| `bbox_messages` | read | all | none |
| `bbox_reembed` | maintenance | ops only | task |
| `bbox_reindex` | maintenance | ops only | task; approval |
| `bbox_session` | read | all | none |
| `bbox_sessions_list` | read | all | none |
| `bbox_stats` | diagnostic read | ops only | none |

### `src/tools/storage_gc.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_storage_gc` | maintenance | ops only | task; approval |

### `src/tools/storage_health.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_storage_health` | diagnostic read | ops only | none |

### `src/tools/threads.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_thread` | read and mutation | default, interactive, agent-internal | resource: thread |
| `bbox_thread_list` | catalog read | all | resource: thread |

### `src/tools/transcripts.rs`

| Tool | Kind | Agent-facing surfaces | Protocol projection |
| --- | --- | --- | --- |
| `bbox_context` | read | all | none |
| `bbox_hybrid_search` | search | all | none |

## Contract status

Every tool above has a row in the surviving action audit. The rows that
audit left as open corrections for tools still served are implemented:

| Tool | Contract now in place |
| --- | --- |
| `bro_wait`, `bro_when_all`, `bro_when_any` | A timeout that is not finite, nonnegative and representable is refused before any wait registers |
| `bro_prune` | An empty `task_ids` and an unknown provider are refused before selection |
| `bbox_gaps` | An exact id pages one oversized record through `body_limit` and `body_cursor` |
| `bbox_artifact_list`, `bbox_artifact_supersede` | Summary pages, an exact redacted inventory, exact installation receipts, and a projected supersede receipt |
| `bbox_project_graph_validate` | Multi-variant summaries page by `variant_limit` and `variant_offset` under a view stamp |
| `bbox_knowledge` | List and query limits clamp to 100; exact entry and diagnostic pages |
| `bbox_project_publisher_status` | `detail_limit` is validated with its `detail`; the last acceptance attempt carries explicit truncation markers |
| `bbox_thread` | Explicit `detail=summary` works; `detail=metadata` recovers topic, sessions and edges exactly |
| `bro_allocator_probe` | Contradictory clear and update fields are refused before any write |
| `bbox_doctor` | Summary pages rank every collected finding by `offset` and `limit` and never truncate a message or repair command; sections collect only when selected. Whether catalog producers still cap findings before the report was not re-verified |

Evidence for this table is source inspection of the adapters and their
parameter types, not a rerun of the isolated HTTP probe; the probe's last
recorded run covered 29 tools of the earlier catalog.
