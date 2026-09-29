---
title: "MCP Surface and Internals Shedding Plan"
kind: design
lifecycle: implemented
corpus: blackbox-design
topic:
  - surfaces
  - mcp
brief: "The MCP tool surface and the internals behind it, cut ahead of the rmcp 3 migration: surfaces as configuration, per-tool dispositions, the slim knowledge model, packet and provenance removal, finished migration code, and the gates that remain."
---

# MCP Surface and Internals Shedding Plan

This design fixes the MCP tool surface and the internals that serve it
ahead of the [rmcp 3.0 Migration Plan](rmcp-3-migration-plan.md): every tool
and store cut here is one fewer contract carried through the migration's
per-request wire head (Phase 1), task projection (Phase 2) and resource
projection (Phase 4). Phase 0 cost does not depend on tool count; the later
phases do.

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

## Stage 1: surfaces are configuration

A surface is a lookup table in daemon configuration:

```toml
[surfaces.interactive]
disallow = ["bbox_project_*", "bbox_storage_*"]

[surfaces.readonly]
allow = ["bbox_hybrid_search", "bbox_inspect_entity", "bbox_knowledge"]
```

Built-in surfaces live in `crates/bbox-config/src/default_surfaces.toml`;
config tables override or add surfaces by name. Surface instructions are not
carried: nothing delivered them to clients.

- Glob patterns; a non-empty `allow` is an allowlist; `disallow` wins over
  `allow`; an unknown surface is refused at connection time.
- Enforcement in `src/server/surface.rs` is unchanged: `list_tools`
  filters, `call_tool` rejects hidden tools.
- Surfaces load with daemon configuration. A change applies on config
  reload or restart; there is no runtime install path and no packet
  versioning.
- Phase 1 of the rmcp plan resolves the surface per request from this
  static map, with no surface decision cache. Deny semantics (Q5) reduce to
  "unknown surface".
- Phase 4's resource dimension becomes a `resources` key on the same table.

Deleted with this stage: `bbox_mcp_surface`, `system-defaults/mcp-surfaces/`
routing packets, and the surface-packet evaluation path. Every ops-only
disposition below is one config edit.

## Stage 2: tool dispositions

| Family | Keep | Ops-only | Delete / Fold | Open |
| --- | --- | --- | --- | --- |
| Retrieval | `bbox_hybrid_search`, `bbox_context`, `bbox_messages`, `bbox_session`, `bbox_sessions_list`, `bbox_inspect_entity` | `bbox_stats` | Fold `bbox_search` into `bbox_hybrid_search`. Delete `bbox_cite`, `bbox_topics`, `bbox_discover_seed_entities`, `bbox_ref_size`. Delete `bbox_corpus_search` and repoint the harness `corpus_search` alias at `bbox_hybrid_search`. | `bbox_find_paths`, `bbox_bundle_evidence`, `bbox_describe_schema`, `bbox_tool_calls` |
| Knowledge | `bbox_knowledge`, `bbox_learn`, `bbox_forget`, `bbox_render` | | Fold `bbox_remember` into `bbox_learn` (`render=false`). Delete `bbox_decide`, `bbox_knowledge_link`, `bbox_lint`, `bbox_review`, `bbox_absorb`, `bbox_bootstrap`. | |
| Work tracking | `bbox_thread`, `bbox_thread_list`, `bbox_gap`, `bbox_gaps`, `bbox_gap_update`, `bbox_gap_resolve` | | Delete `bbox_inbox`, `bbox_pin` | `bbox_note`, `bbox_notes`, `bbox_note_resolve` |
| Dispatch | `bro_exec`, `bro_resume`, `bro_status`, `bro_wait`, `bro_when_all`, `bro_when_any`, `bro_steer`, `bro_cancel`, `bro_dashboard`, `bro_providers`, `bro_brofile` | `bro_prune`, `bro_allocator_status`, `bro_allocator_trace`, `bro_allocator_probe`, `bro_mcp` | Delete `bro_retro`, `bro_broadcast`, `bro_interrupt`, `bro_report`, `bro_agent_list`, `bro_agent_get`, `bro_agent_describe`, `bro_agent_search`, `bro_agent_dispatch` | `bro_team` |
| Projects | `bbox_project_list` | `bbox_project_register`, `_init`, `_rename`, `_unregister`, `_eject`, `_catalog_list`, `_catalog_get`, `_attach`, `_detach`, `_default_attachment`, `_promote`, `_scope_migrate`, `_publisher_bind`, `_publisher_advance`, `_publisher_status`, `bbox_project_graph_list`, `_describe`, `_validate` | | |
| Index and storage | | `bbox_reindex`, `bbox_reembed`, `bbox_embed_status`, `bbox_embed_partitions`, `bbox_storage_gc`, `bbox_storage_health`, `bbox_edge_compact`, `bbox_doctor` | Delete `bbox_storage_migrate_legacy_edges` (Stage 6) | |
| Artifacts | | `bbox_artifact_install`, `_list`, `_remove`, `_supersede` | | |
| Packets | | | Delete `bbox_compile`, `bbox_apply`, `bbox_audit`, `bbox_packet_list`, `bbox_packet_events`, `bbox_packet_gap` (Stage 3) | |
| Provenance | | | Delete `bbox_blame`, `bbox_provenance_export`, `_export_plan`, `_import` (Stage 5) | |
| Surfaces | | | Delete `bbox_mcp_surface` (Stage 1) | |

End state: 28 agent-facing tools, 36 ops-only, 36 deleted or folded, 8 open.

Per-tool notes:

- `bbox_pin`: arc-bound context belongs in the dispatch brief or the
  thread. The pin store (`bbox-stores` pins), dispatch-time pin injection,
  the `scoped-pins` system memory and the persistence guide's pin lane go
  with it. A state directory's pin file stays a project-catalog owner, and
  the dispatch-context parser accepts and drops a `pins` block from older
  payloads.
- `bro_interrupt` is covered by `bro_cancel` plus `bro_resume`, and
  `bro_steer` for mid-turn input.
- `bbox_project_list` stays as the one agent-facing project reader so
  agents can resolve project ids and aliases. An alternative is to list
  known projects in the error when a `project` selector does not resolve.
- `bbox_render` stays agent-facing because agents render after a knowledge
  write. Rendering on write would let it move to ops-only.
- The project onboarding skill (`src/server/onboarding_skill.rs`) names
  `bbox_project_register`, `bbox_project_catalog_list` and
  `bbox_project_publisher_status` in their CLI form, which matches
  registration being a checkout-host operation. `doctor.rs` and error
  remediation strings name ops-only tools in the CLI form the same way.
- `bro_report`: dispatched work is followed through `bro_status`,
  `bro_wait` and `bro_dashboard`. Its task report store, the roster
  `report` fields, `bro_status detail=report` and the dispatch milestone
  directive go with it.
- `bro_agent_*`: nothing dispatches agents; brofiles plus `bro_exec` carry
  roles. The agent artifact kind is retired, not deleted, so persisted
  catalogs load: `agent` stays a kind beside `workflow`, `atom`, `cron` and
  `packet`, catalog metadata, the project catalog owner snapshot and the
  retirement inventory parse it, install and boot restore refuse it, the
  default listing hides it, an explicit kind filter shows its receipts as
  retired, and removal works. Agent install validation, manifests, the
  agent embedding bucket, agent edges and the agent entity type go. An
  embed config's `agent_manifest` route key loads and is ignored.

Dispatch context: the dispatch context carries the persona, the completion
contract and the `<bbox_scope>` block. The completion contract is the only
directive: `standing` cadence with `needs_scope`, sent only on dispatches
that carry a contract. The `recall`, `task_shape` and `orchestrator`
directives and the per-turn cadence (protocol variant, harness volatile-lane
injection) are gone; the parser accepts `per_turn` directives from older
payloads and persisted side-state and drops them. The completion contract
moves with the notes decision (see Open decisions); the directive plumbing
(cadence, `needs_scope`, standing-slot routing) goes once the contract has
that home, leaving persona and the `<bbox_scope>` block.

Every deletion also scrubs the tool from the generated tool reference
(`bbox-tool-docs`), system memories, prompts, docs, surface config, brofile
tool allowlists (`src/orchestration/brofile.rs`) and harness surface tests.

## Stage 3: no packet engine

With surfaces in configuration, nothing consumes rule packets at runtime.

Deleted: `bbox_compile`, `bbox_apply`, `bbox_audit`, `bbox_packet_list`,
`bbox_packet_events`, `bbox_packet_gap`; the `bbox-packets` crate; the
packet store and event log, the self-heal scanner and the packet compile
admin route; the packet entity type and inspect provider; the default
packets under `system-defaults/agentic-corpus/packets/`; the arc-bound
warning; the `sm-rule-packets` system memory and the rule-packet section of
the tool guidance. `bbox-gaps`, `bbox-indexing` and `bbox-providers` carry
no packet dependency.

The packet artifact kind is retired, not deleted, so persisted catalogs
load: installed receipts parse and list under an explicit kind filter,
removal works, and install and boot restore refuse it. A state directory's
`packets/` tree stays a project-catalog owner through
`project_catalog_packet_tree` in `bbox-indexing`; no store opens it.

## Stage 4: slim knowledge model

Knowledge keeps one write lane (`bbox_learn`), one reader
(`bbox_knowledge`), retirement by deletion (`bbox_forget`) and rendering
(`bbox_render`). Project entries are one JSON file per entry under
`.bbox/knowledge/`, so git history carries prior versions and the reason
for each change.

Entry fields kept: `id`, `title`, `content`, `category`, `cluster`,
`scope`, `project`, `project_id`, `providers`, `priority`, `render`,
`render_placement`, `created_at`, `updated_at`, `recall_count`,
`last_recalled`.

- `providers` is the per-provider render filter.
- `render_placement` stays because rendering reads it (inline or satellite
  topic placement).
- `recall_count` and `last_recalled` are host-local recall telemetry that
  boosts hybrid-search ranking; repo-owned entries keep them in the
  gitignored stats sidecar, never in the committed file.

Fields removed:

| Field or mechanism | Replacement |
| --- | --- |
| `supersedes`, `superseded` status | `bbox_forget` plus a new entry; git history for project entries |
| `links`, the edge kinds, `bbox_knowledge_link` | none |
| `rationale` | part of `content` |
| `variants` (per-provider text) | none |
| `expires_at`, `review_at`, `decay` | none |
| `status` other than active | deletion |
| `approval` states and the review queue | approval happens before the write |
| `source` | none |
| `weight` | ordering by priority, then title |

Categories are `profile`, `convention`, `steering`, `build`, `tool`,
`memory` and `workflow`. There is no `decision` category: a stored
`decision` reads as `convention`. `AcceptedKnowledgeCategoryV1::Decision`
is a versioned publication schema value, so published rows keep it and the
reader maps it; nothing rewrites them.

Every reader (central stores, repo entry files, overlays, published sources
and version-1 accepted rows) applies the same legacy rules: removed fields
are ignored, a stored `rationale` is appended to `content`, a stored
`decision` category reads as `convention`, and a record whose stored status
is not active or whose stored expiry has passed is skipped. Writes emit only
the kept fields. Rendered category sections and recall ties order by
priority, then title.

`bbox_learn` with `render=false` writes an indexed-only recall entry. The
persistence guide, `persistence-taxonomy`, `render-lifecycle` and
`docs/knowledge-store.md` describe the single lane.

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
- The stored file-touch edges, purged from the durable lanes at startup.

The Git-source contract, store and HTTP routes carry Git history only; the
store removes its provenance trees on open, and collector enrollment files
that carry the provenance key keep loading. The Git transport cutover
captures no provenance evidence: its marker rows name their members
explicitly, and rows written earlier name them through the provenance map
keys. The project-catalog inventory reads provenance notes through a local
capture.

"Which sessions touched this file" is answerable through transcript search
on the path.

## Stage 6: finished migration code and dead crates

Migration and overlap code goes once its completion is confirmed on every
live deployment. Nothing the daemon's startup, catalog open, GC, doctor,
genesis, backfill, rebuild, retirement or the cutovers reach is removed.

Removed:

- **Project catalog v1 migration execution**: the `migrate` and `verify`
  subcommands of the offline `blackbox project-catalog` CLI, and the
  migration facade's configured-apply and verify entries. The daemon never
  imports v1 state; a `MigratedV1` catalog is verified at every open through
  its origin marker, receipt binding and transaction journal. The facade's
  preflight and rehearsal apply stay as the producer of migrated fixtures for
  the genesis, backfill, rebuild and history-materializer tests. The
  migration lock, layout and error types, the legacy commit namespace
  inventory asset, `LegacyPathStoreKindV1` (persisted as `source_store`) and
  GC protection of `project-catalog-migration-assets` stay, because catalog
  open, genesis, backfill, rebuild, retirement and the cutovers use them.
- **Startup legacy-path migration** (`legacy_migration.rs`), which moved
  `~/.claude-shared` and `~/.bro` state into the configured paths.
- **`bbox_storage_migrate_legacy_edges`** with its extraction planner and
  apply path. Startup recovery of pending edge migrations and the loader's
  reading of the explicit and observed lanes stay.

Kept, each with the condition that gates its removal:

- **Git transport cutover**: a Published repo covered by the cutover.
- **Knowledge transport cutover** and its legacy compatibility lane: a
  verify with no stale rows and every project covered.
- **Code-source locality and render locality cutovers**: a completion
  verify on every live deployment; read-only inspection cannot confirm it.
- **`resolver_compat.rs`**: no recorded compatibility-lane hits.
- **`bridge_parity.rs`**: bridge mode stops being the fresh-state path.
- **Migration inventories** in the edge sidecar, corpus index and vectors:
  genesis, backfill and retirement read them.

Also in this stage:

- **Dead crates**: `bbox-source-graph`, `bbox-system-events`, and
  `bbox-inbox` with `bbox_inbox` are deleted.
- **Whiteboards**: the runtime, graph provider and `whiteboard:` entity
  family are deleted. `bbox-whiteboards` keeps only the board record shapes
  and the project-catalog owner adapters (capture, stamp and verify),
  because the v1 migration's closed owner contract requires a whiteboard
  lane in its inventory. Edge rows with a whiteboard endpoint stay inert on
  disk.

`bbox_project_scope_migrate` is the v2 scope move (relpath or repository
authority), not v1 migration code; it is ops-only.

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

## Remaining gates

- Each Stage 6 kept item goes when its named condition holds on every live
  deployment.
- The completion-contract directive and its plumbing go with the notes
  decision.
- The open decisions above.
- rmcp migration Phase 0 follows this design. The
  [target-surface doc](mcp-2026-07-28-target-surface.md)'s task candidates
  and resource catalogs are revised against the surviving surface before
  Phase 2 and Phase 4 start.

## Validation

`cargo check`, `cargo nextest run --workspace`, `cargo clippy` and
`scripts/lint-concurrency.sh`, lane-side. Then:

- `tools/list` on each agent-facing surface returns exactly the Keep set;
  on `ops` it returns Keep plus Ops-only.
- An ops-only tool round-trips through `bro mcp call --surface ops`.
- A hidden tool called on an agent-facing surface is rejected.
- An unknown surface is refused.
- A search for each removed tool name finds no references outside git
  history.
- `bbox_render` output and `bro render global` show no removed tools in
  generated guidance.
