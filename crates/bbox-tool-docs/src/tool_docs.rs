//! Single source of truth for the agent-facing tool reference.
//!
//! Every `bbox_*` / `bro_*` MCP tool registered in `main.rs` must have
//! a matching stanza in `TOOL_DOCS`. A unit test enforces this.
//!
//! On startup, `sync_into_knowledge` publishes a compact inline bootstrap and
//! topic-scoped satellite entries. `TOOL_DOCS` is the source for both detailed
//! guidance and tool discovery. Deep runbooks remain available through memory.
//!
//! Adding or changing a tool = one edit here. No hand-curated drift.

use std::borrow::Cow;

use anyhow::Result;

use bbox_knowledge::knowledge::{
    Category, GuidanceTopic, KnowledgeEntry, Priority, RenderPlacement, Scope,
};

pub const TOOL_DOC_ENTRY_ID: &str = "bb-tool-reference";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    Transcripts,
    Graph,
    ProjectGraphs,
    Projects,
    ProjectCatalog,
    Knowledge,
    Threads,
    Gaps,
    Artifacts,
    Orchestration,
    StorageHealth,
    Operations,
}

impl ToolCategory {
    fn heading(&self) -> &'static str {
        match self {
            Self::Transcripts => "Transcripts",
            Self::Graph => "Agentic graph",
            Self::ProjectGraphs => "Reflective project graphs",
            Self::Projects => "Projects",
            Self::ProjectCatalog => "Project catalog administration",
            Self::Knowledge => "Knowledge",
            Self::Threads => "Threads",
            Self::Gaps => "Gap notes",
            Self::Artifacts => "Artifact catalog",
            Self::Orchestration => "Bro orchestration",
            Self::StorageHealth => "Storage health",
            Self::Operations => "Operations",
        }
    }

    fn intro(&self) -> &'static str {
        match self {
            Self::Transcripts => {
                "Search and read across every Claude Code / Codex / Gemini session the host has recorded. Reach for these when the user asks about past conversations, when you need to cite the origin of a rule, or when you need context around a prior decision."
            }
            Self::Graph => "Inspect entities, graph vocabulary, paths, bundles, and retrieval.",
            Self::ProjectGraphs => {
                "Read project-owned reflective graph generations. Operator tools, served on the `ops` surface: `bro mcp call <tool> '<json>' --surface ops`."
            }
            Self::Projects => {
                "Resolve registered project roots with `bbox_project_list`. Registration and the other project administration tools are operator tools, served on the `ops` surface: `bro mcp call <tool> '<json>' --surface ops`."
            }
            Self::ProjectCatalog => {
                "Durable project-catalog administration: attach and detach local checkouts, select the default attachment, promote a legacy-local project to its committed scope, migrate a published scope, and rebind the publisher attachment. Every one of these refuses with `error.project_catalog_inactive` while the version-1 registry is the runtime authority; the proofless-authority operations (catalog add, alias accept and reject, retire) live on the offline `blackbox project-catalog` CLI instead. Operator tools, served on the `ops` surface: `bro mcp call <tool> '<json>' --surface ops`."
            }
            Self::Knowledge => {
                "Memory lanes: `bbox_learn` for operator-approved rendered rules and, with `render=false`, approved cold recall."
            }
            Self::Threads => {
                "Track non-dispatchable work that spans sessions (investigations, QC walks, debugging, refinement loops). Lighter than the full dispatch pipeline, heavier than memory. Use `kind=work_item` for orchestrator-led propose→execute→review→refine loops."
            }
            Self::Gaps => {
                "First-class substrate gap-note store. File a gap when the blocker is in the blackbox substrate or shared agent workflow — a missing tool primitive, MCP surface, refactor atom, workflow shape, ontology edge, or runbook that agents in other projects could plausibly hit too — not in the current product codebase. Project-scoped gaps are repo-owned (committed under `<project>/.bbox/gaps/`, travel with the checkout); cross-project substrate gaps go to the central host store with `scope=\"global\"`. `bbox_gap` files (typed, validated, deduped by `dedupe_key`), `bbox_gaps` filters by typed fields, `bbox_gap_resolve` closes out (with structured supersession), `bbox_gap_update` edits in place. See `sm-gap-notes` via `bbox_knowledge` for the full envelope, vocabularies, and lifecycle."
            }
            Self::Artifacts => {
                "Versioned catalog for brofiles. Supply artifact JSON inline or by HTTP(S) URL. Explicit retired-kind filters retrieve historical receipts. Operator tools, served on the `ops` surface: `bro mcp call <tool> '<json>' --surface ops`."
            }
            Self::Orchestration => {
                "Dispatch agents across the providers listed by bro_providers. Prefer named `bro` targeting (a brofile name resolves provider, account, lens and context) over raw provider. Core pattern: `bro_exec` to launch, `bro_wait` or `bro_when_all` to block, `bro_resume` for follow-ups (never `bro_exec` again: it starts fresh with no memory). For ensembles: several `bro_exec` calls + `bro_when_all` (blind deliberation) or `bro_when_any` (race). For provider-default suppression and minimal probe context, pull `sm-brofile-context` via `bbox_knowledge`."
            }
            Self::StorageHealth => {
                "Read-only storage inventory for edge sidecar hygiene. Operator tools, served on the `ops` surface: `bro mcp call <tool> '<json>' --surface ops`."
            }
            Self::Operations => {
                "Day-2 operational health surfaces: aggregate daemon/corpus/route status with classified findings and suggested next commands. Operator tools, served on the `ops` surface: `bro mcp call <tool> '<json>' --surface ops`."
            }
        }
    }
}

fn deferred_system_memory(category: ToolCategory) -> Option<&'static str> {
    match category {
        ToolCategory::Gaps => Some("sm-gap-notes"),
        ToolCategory::Orchestration => Some("sm-bro-dispatch-patterns"),
        ToolCategory::ProjectGraphs => Some("sm-agentic-opening-sequence"),
        _ => None,
    }
}

const HOT_RENDER_CATEGORIES: &[ToolCategory] = &[
    ToolCategory::Transcripts,
    ToolCategory::Graph,
    ToolCategory::Projects,
    ToolCategory::Knowledge,
    ToolCategory::Threads,
    ToolCategory::Artifacts,
    ToolCategory::Orchestration,
];

#[derive(Debug, Clone, Copy)]
pub struct ToolDoc {
    pub name: &'static str,
    pub category: ToolCategory,
    pub summary: &'static str,
    pub when_to_use: &'static str,
    pub example: Option<&'static str>,
}

pub const TOOL_DOCS: &[ToolDoc] = &[
    // ── Transcripts ──────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_hybrid_search",
        category: ToolCategory::Graph,
        summary: "Search the corpus (transcripts, code, docs, commits, knowledge, threads, graph vertices) with BM25 and vectors. Filter conversations by role, account, source, author or channel; mode=fulltext takes raw Tantivy/Lucene syntax. Conversation hits carry read coordinates for bbox_context and bbox_messages. Use debug for ranking diagnostics.",
        when_to_use: "Step 2 of the agentic opening sequence (`sm-agentic-opening-sequence`). Use as the default search for any topical question, including prior conversations. Pass `project=$cwd` (or a registered project_id) when querying about your local repo to avoid cross-project keyword pollution. Trust topical hits: top seed is canonical for the query even when wording doesn't exactly match (vector lane catches paraphrases). The query language: adjacent terms broaden recall, quoted phrases stay exact, `-term` excludes; `mode=\"fulltext\"` takes raw Tantivy/Lucene boolean syntax with conjunction semantics. For conversations, narrow with `doc_type=\"transcript\"`, `role` (`user` finds who established a rule), `account`, `include_subagents=false`, and `exclude_self=true` for current-turn searches; once narrowed to conversations, `project` scopes them by recorded working directory. `source` filters the lane a document came from (`glm`, `claude`, `codex`, `gemini`, `slack`, ...): comma-separated for several, and a `-` prefix excludes one, so `source=\"slack\"` searches only ingested Slack conversations and `source=\"-slack\"` everything else. For \"what's in a channel\" questions use `channel=` (a name, leading `#` accepted, or an id; names resolve through the current roster so a renamed channel matches its whole history); plain queries match channel names too. `author` filters conversation documents by provider user id. Filters apply to every lane before ranking. Conversation hits carry `conversation` coordinates (`session_id`, `file_path`, `byte_offset`, timestamp, account, channel, permalink, and an `exact_read` page reader when available) that feed bbox_context, bbox_messages and bbox_session unchanged. Model rerank (hosted cross-encoder, [embed.rerank], default rerank-2.5-lite) is the DEFAULT and degrades to the heuristic path on API failure (degraded.rerank_unavailable); pass rerank=\"heuristic\" to skip the cross-encoder call when latency matters more than precision, or rerank=\"none\" for raw fusion order. Project graph vertices participate like any typed entity: `project` also scopes them by their stamped project id; repeatable `graph_source` picks planes (`published`, `connector`; unset = all; only `published` has indexed documents, so `connector` is accepted but returns no graph hits) and `graph_ids` names graphs within the resolved project, both applied before ranking so excluded vertices never consume rank positions. Vertex hits carry `graph_id`, `graph_source`, `graph_vertex_type`, `graph_generation`, and `graph_logical_ref`.",
        example: Some(
            r#"bbox_hybrid_search(query="triad implementation", limit=10, project="/home/me/repos/erlang-test")"#,
        ),
    },
    ToolDoc {
        name: "bbox_context",
        category: ToolCategory::Transcripts,
        summary: "Read surrounding indexed events by opaque locator and offset, or page an exact native stored record using its recovery handle. Native replies disclose projection and freshness limits.",
        when_to_use: "Use a search hit's file_path and byte_offset unchanged. file_path is an opaque stored locator, never a file to open. Native context_lines counts indexed events before/after the target (default 5, max 25), with 400-byte previews. For exact stored fields, copy an exact_read hint's arguments: indexed-transcript handle, byte_offset, body_limit (default/max 4096 bytes). Concatenate body.text and continue body.next_cursor as body_cursor with the same handle and offset; omit context_lines for these pages. Handles bind the corpus, stored document and native source/session; deletion, segment replacement or content changes require repeating discovery. This reader also recovers tool-call targets/outcomes. Indexed projections may already be parser-truncated and do not establish source completeness or freshness. Slack locators resolve through the conversation landing store using digit-encoded message timestamps and cannot use native record handles.",
        example: None,
    },
    ToolDoc {
        name: "bbox_session",
        category: ToolCategory::Transcripts,
        summary: "Summary metadata for a single session.",
        when_to_use: "Use an exact indexed session ID for retained message count, sources, and indexed time range. This summary does not establish source completeness or freshness. Read messages with bbox_messages; no producer filesystem access is required.",
        example: None,
    },
    ToolDoc {
        name: "bbox_messages",
        category: ToolCategory::Transcripts,
        summary: "Page stored messages by exact session ID or opaque transcript locator. Native replies disclose projection and freshness limits.",
        when_to_use: "Provide exactly one of session_id or file_path from search. An indexed-transcript handle in file_path selects that native source/session without exposing its source-host path. Native pages sort by locator then source byte offset; next_offset advances by the actual returned count under the byte cap. from_end pages from the tail. max_content_length limits preview bytes; zero returns up to 12000 retained bytes per native message, not original-source recovery. Rows carry exact_read arguments for bbox_context body pages; omitted selector fields remain exact through that reader. Native replies explicitly report projection-only completeness and source observations separately. Slack file_path selects a channel; its channel/date session_id selects a day through the landing store.",
        example: None,
    },
    ToolDoc {
        name: "bbox_reindex",
        category: ToolCategory::Transcripts,
        summary: "Queue a full or incremental search-index update. Returns after admission by default; wait=true is for internal migrations that require completion.",
        when_to_use: "Rarely — background reindexer runs every 120s. Interactive calls return as soon as the single writer actor accepts the pass; a duplicate request reports that a pass is already active. Use `full=true` after corpus corruption or schema changes. `wait=true` is reserved for internal migrations that require completion. `accept_empty_projects` is an operator acknowledgement: name the projects whose empty local root should purge normally (clearing their `empty_root_refused` health), never set it on the operator's behalf.",
        example: None,
    },
    ToolDoc {
        name: "bbox_reembed",
        category: ToolCategory::Transcripts,
        summary: "Request an embedding rebuild for a configured route.",
        when_to_use: "Use after changing embedding routes or provider dimensions to kick convergence immediately; a background residue sweeper otherwise drives every non-transcript route to full coverage on its own, including residue past the per-route queue cap, so a `stalled` route converges without repeated manual calls. E3 performs the rebuild. Routes include knowledge, code, docs, git_message, threads, graph (project-graph vertices whose schema opts them into embedding; rebuilt from the installed published views, and the one route that also tombstones vectors of vertices no longer embed-eligible), and guarded transcripts; `backfill` sweeps every route except transcripts (idempotent: already-embedded items dedupe at enqueue). Use max_entities for progressive refills. Transcript rebuilds require include_transcripts=true because they read the transcript corpus.",
        example: None,
    },
    ToolDoc {
        name: "bbox_embed_partitions",
        category: ToolCategory::Transcripts,
        summary: "Vector partition lifecycle on daemon-owned storage: list partitions with route mapping, dims, dtype, compatibility family, active_count, last_write (paged; limit default 20, max 100); prune orphaned partitions; scrub misattributed vectors from a mapped partition (dry-run default).",
        when_to_use: "Use action=\"list\" to see vector partitions and whether any configured bucket currently maps to them (orphans show mapped=false; hybrid search skips them under degraded.skipped_partitions); the inventory is paged (limit default 20, max 100; follow next_offset), and continuation must assume the live partition set can change between pages. Unknown actions and action-mismatched fields (apply/older_than_days/route against the chosen action) fail BEFORE any vector-store scan. After a deliberate model/route migration, use action=\"prune\" with older_than_days=<N>: only partitions BOTH unmapped by current route config AND idle beyond that age are candidates, and nothing deletes without apply=true (dry-run default); prune decisions cover the full projection even when the returned inventory is paged. bbox_reembed never prunes; reclaiming a vector space is a separate operator decision. After a bucket attribution change, action=\"scrub\" with route=<mapped partition id> classifies every vector against CURRENT attribution and (with apply=true) deletes rows whose entities now belong to a different route; index-missing entities and non-project_file rows are always kept. Scrub requires local access to the daemon's vector store and index; it reports classification counts for one route and does not page. list/prune preview body_limit/cursor pages recover complete inventory and mappings. Prune apply accepts an optional route selector and refuses batches above eight candidates or the identity byte budget before deleting; row pagination is not a mutation selector. A deletion failure stops the batch and reports unattempted_count. Scrub likewise stops on its first deletion failure.",
        example: Some("bbox_embed_partitions(action=\"prune\", older_than_days=30)"),
    },
    ToolDoc {
        name: "bbox_embed_status",
        category: ToolCategory::Transcripts,
        summary: "Read embedding health. Scan and probe opt-ins can be expensive; oversized reports use session snapshots with exact cursor recovery.",
        when_to_use: "Use when vector search degrades. The default reports availability, health, queue depth, session_indexed_count (successes since daemon startup, not corpus size), nonzero capped_count (enqueues rejected at the queue cap - residue the sweeper will refill, not a drop), dropped_count (permanently un-embeddable poison), and sanitized error without walking the source corpus or HNSW. Zero retry/drop/cap counters and absent values are omitted. debug=true restores provider/model configuration and routine diagnostic fields; it does not enable expensive work. Pass include_coverage=true for exact per-route source/indexed counts and stalled-coverage classification; this walks every embedding-source document and can take minutes on a large corpus. Pass include_diagnostics=true with an optional bounded diagnostic_routes list for deadline-bounded connectivity diagnostics; unavailable is reported separately from healthy. Pass recall_probe_route for sampled self-recall; explicit probes can take seconds on large partitions and refuse busy routes. Recall probes only inspect loaded partitions; unknown names report vector_partition_not_loaded without creating storage. Null self_recall means no HNSW graph yet or a warming store. Path-shaped recall names refuse before collection. Later graph/probe failures retain completed observations with error.embedding_observation_partial on every exact page. diagnostic_routes accepts 1..=64 nonempty names, each at most 256 bytes. Contradictory scan/probe selectors refuse before collection. Oversized reports automatically return immutable session snapshots; body_limit=4..4096 forces exact paging. Repeat the original selectors with cursor=body.next_cursor and concatenate body.text as JSON. Continuation never repeats scans or probes. Snapshots expire after ten minutes, daemon/session loss, or oldest-first eviction (four reports, 16 MiB total per MCP session). Reports above 8 MiB explicitly refuse storage after collection; narrow opt-ins before retrying.",
        example: Some(
            "bbox_embed_status(include_diagnostics=true, diagnostic_routes=[\"voyage-1024\"])",
        ),
    },
    ToolDoc {
        name: "bbox_sessions_list",
        category: ToolCategory::Transcripts,
        summary: "Browse retained indexed sessions by latest indexed activity. Filter by source, exact account, or registered project identity/recorded path text. Pages default to 30 sessions, maximum 100, with next_offset and byte limits. Source freshness is not assessed. Session-name filters are unavailable because names are not indexed.",
        when_to_use: "Use when you need to find a retained session by recency, project, source, or account without a concrete text query. See `sm-transcript-retrieval` via `bbox_knowledge` for retrieval ladders.",
        example: None,
    },
    ToolDoc {
        name: "bbox_stats",
        category: ToolCategory::Transcripts,
        summary: "Indexed document and segment counts, cached up to 60s.",
        when_to_use: "Check whether the index is populated. Does not assess source coverage, freshness, disk size or edge totals. Use targeted search or source status to verify a particular session or publication.",
        example: None,
    },
    // ── Agentic graph ────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_inspect_entity",
        category: ToolCategory::Graph,
        summary: "Inspect an entity's stored properties. Project graph vertices, and entities an evidence binding names, also carry their graph edges; filter those with edge_types and direction. property_mode selects summary, smart, or full; property retrieves exact text in pages.",
        when_to_use: "Use after search to verify a ref and read its stored properties. A ref resolves only when its provider's store holds it; symbol refs never resolve. Project graph vertices carry their graph edges and evidence bindings: select edge_types and direction (out, in, both). property_mode is summary, smart (default, 300-character text previews), or full; invalid values fail. Edges page at 100 maximum; follow edge_page.next_cursor as edge_cursor with the same selection. Read a property key from properties or property_projection.omitted_keys with property=<key>; body.next_cursor continues via property_cursor. property_limit is 4..4096 UTF-8 bytes, default 4096. Cursors reject changed selections or source revisions. Full/property reads recover stored provider values; *_preview fields do not expand upstream content. Commit content is the indexed message; evidence.content_completeness marks ingestion truncation. Schema-authored absent relations remain explicit; generic empty scaffolding is omitted. Evidence properties retain assertion authority, source generation, endpoint freshness, and unresolved states. No embedded rendered text mirror is returned.",
        example: Some(
            r#"bbox_inspect_entity(entity_ref="knowledge:abc12345", property="content")"#,
        ),
    },
    ToolDoc {
        name: "bbox_project_graph_list",
        category: ToolCategory::ProjectGraphs,
        summary: "List visible project graphs in bounded pages (default 20, max 100, also byte-budgeted) ordered by graph_id then source. Continue with next_offset plus expected_view_stamp.",
        when_to_use: "Discover graph ids. Every read is published-only. Each entry carries two count families: vertex_count/edge_count count the REFLECTED graph (authored rows plus schema-as-data type vertices and meta:INSTANCE_OF edges), while authored_vertex_count/authored_edge_count count only rows from vertices.jsonl/edges.jsonl; compare authored_* against your source files. source names the authority plane: published or connector (read-only connector-managed projection, never writable through a checkout lane). The inventory is a live view, not a snapshot: published installs replace whole entries, so a changed view_stamp refuses nonzero offsets and you restart at offset 0. Empty inventories return total 0 with an empty graphs array; a missing graph id under describe/validate is error.not_found.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_graph_describe",
        category: ToolCategory::ProjectGraphs,
        summary: "Describe one visible project graph. detail=summary (default) stays compact: identity, generation, authority plane, retrieval state, and schema counts without the schema body; multi-variant summaries page with totals. detail=schema or detail=descriptor recovers the exact JSON in bounded body pages.",
        when_to_use: "Read generation identity and word-index participation without pulling the schema. The summary keeps both count families (reflected vertex_count/edge_count versus authored_* from the jsonl sources) plus the retrieval block: policy flags, indexed counts and generations, and the excluded-type COUNT (the exact sorted list lives in the schema body under index_policy.retrieval_excluded_types, recovered by detail=schema). One graph id can be visible as several variants (published and connector entries, including byte-identical repeats); the summary pages them with variant_limit/variant_offset plus expected_view_stamp, and a changed variant set refuses continuation. For exact reads select one variant with source and expected_content_hash from its list entry; a repeated hash alone stays ambiguous, and selection narrows what the read returned and never widens authority. Body pages are 4..4096 UTF-8 bytes (body_limit, default 4096), continue with cursor=body.next_cursor, and concatenate body.text before parsing; cursors bind to the exact selected variant and refuse any graph, selection, or content change. An entry accepted invalid carries schema:null and refuses detail reads with error.graph_payload_unavailable; use bbox_project_graph_validate for its diagnostics.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_graph_validate",
        category: ToolCategory::ProjectGraphs,
        summary: "Validate one visible project graph. detail=summary (default) pages error rows (default 20, max 100) with errors_total; detail=errors recovers the complete error array as exact JSON body pages.",
        when_to_use: "Inspect kernel diagnostics. Reports the same sources as list: published and connector. When several variants share the graph id, select one with source and expected_content_hash from its list entry; selection narrows what the read returned and never widens authority. Continue summary pages with next_error_offset plus expected_error_stamp; a changed error set (or variant) refuses with error.graph_errors_changed and you restart at error_offset 0, while nonzero offsets without a stamp are refused outright. detail=errors pages the complete array through the shared body reader: body pages are 4..4096 UTF-8 bytes (body_limit, default 4096), continue with cursor=body.next_cursor, and concatenate body.text before parsing; cursors bind to the exact selected variant and refuse any graph, selection, or content change. A valid graph reports valid:true with an empty errors array and errors_total 0; detail=errors on it returns the exact array []. Summary variants use variant_limit/variant_offset with expected_view_stamp and next_offset. Nonzero variant offsets require the previous stamp; changes refuse continuation. Error paging and exact errors still require one selected variant. Variant paging fields are invalid for detail=errors.",
        example: None,
    },
    ToolDoc {
        name: "bbox_edge_compact",
        category: ToolCategory::Graph,
        summary: "Dry-run or apply legacy edge sidecar compaction for one project. Removes append-only derived edges from edges/<project_id>.jsonl while retaining explicit/provenance/malformed lines; apply defaults false and writes a backup before replacement.",
        when_to_use: "Use when a legacy top-level edge sidecar has grown from repeated full reindex replay and its disk footprint matters. Call first with `apply=false` (default) for exactly one project_id, inspect removed/retained counts, then call with `apply=true` for that same project if the dry-run scope is acceptable.",
        example: Some(r#"bbox_edge_compact(project_id="d723917f", apply=false)"#),
    },
    // ── Projects ─────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_project_register",
        category: ToolCategory::Projects,
        summary: "Register an absolute project path and schedule background agentic-corpus indexing. A path visible to the daemon uses local checkout authority. A remote path automatically routes to the fresh checkout-host collector whose enroll_roots most specifically contain it; optional producer selects among an equal-depth tie. Collector enrollment scaffolds `.bbox` and updates its host-local sidecar, so no per-project collector or daemon config edit is needed. When the enrolled receipt reports identity_committed=false, commit exactly the returned commit_paths on published_ref. Repeating the same path reuses pending work and registration is idempotent. In catalog mode this is the find-or-create composite: the checkout attaches to the project owning its committed scope or a new Published/LegacyLocal project is minted. Use bbox_project_catalog_list for logical projects in catalog mode, or bbox_project_list for bridge registered roots.",
        when_to_use: "Use for both daemon-local and checkout-host paths. For remote enrollment, commit exactly the returned paths on the returned published ref.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_init",
        category: ToolCategory::Projects,
        summary: "Initialize a locally visible project `.bbox` workspace using daemon checkout authority. Creates `.bbox/config.toml`, `.bbox/mcp.json`, `.bbox/local/.gitignore` and default subdirectories, and records the durable repo_id for Git projects. Idempotent by default; force=true refreshes replaceable skeleton files but always merge-preserves identity-bearing config.toml. If the daemon cannot stat the path, call bbox_project_register instead; it routes to the checkout-host collector, scaffolds the project during enrollment, and returns the exact paths to commit.",
        when_to_use: "Use for daemon-local checkout initialization. For remote paths, use bbox_project_register and commit the returned paths.",
        example: Some(r#"bbox_project_init(path="/home/me/repos/blackbox", force=true)"#),
    },
    ToolDoc {
        name: "bbox_project_rename",
        category: ToolCategory::Projects,
        summary: "Local administrator operation; transport-owned catalog projects refuse with error.project_admin_locality_required because no remote relocation lane is implemented. A bridge failure after registry admission reports error.project_rename_partial with completed effects and old/new recovery coordinates. Rename a registered bbox project root while preserving its project_id and migrating project-scoped bbox state. Accepts project (project_id, registered canonical_path, or absolute path), new_path (absolute directory path), optional move_on_disk (default false), and optional dry_run. Updates project registry, knowledge, threads, Slack channel bindings, pollers, and crons, then reindexes project files. In catalog mode rename is attachment relocation: the moved checkout must carry the same checkout-id marker and resolve the same scope, the ledger records the historical path, owner-store rows are never rewritten, and move_on_disk is refused (move first, then rename).",
        when_to_use: "Use after renaming a repo directory, or with `move_on_disk=true` to let bbox move the directory first. Prefer `dry_run=true` before changing several project names so the affected state counts are visible.",
        example: Some(
            r#"bbox_project_rename(project="d723917f", new_path="/home/me/repos/blackbox", dry_run=true)"#,
        ),
    },
    ToolDoc {
        name: "bbox_project_eject",
        category: ToolCategory::Projects,
        summary: "Local administrator migration; apply requires exact base-checkout mutation authority and refuses transport-owned projects with error.project_admin_locality_required. No remote ejection lane is implemented. Migrate a registered project's central-store knowledge entries into the repo's committed .bbox/knowledge/ (one file per entry), so the project's durable knowledge travels with the checkout. Accepts project (project_id, registered canonical_path, or absolute path) and optional dry_run. Entries are written without the absolute project path (location encodes scope), dropped from the central store, and a clean schema-epoch marker is written by this explicit operator action. dry_run=true reports the count without writing. Commit the resulting .bbox/ files to publish them.",
        when_to_use: "Run once per existing project to move pre-migration central knowledge into the repo, then commit the .bbox/ files. New project-scope writes already land in .bbox/ automatically; eject is for backfilling entries created before the repo-owned cutover. Prefer dry_run=true first to see the count.",
        example: Some(r#"bbox_project_eject(project="/home/me/repos/blackbox", dry_run=true)"#),
    },
    ToolDoc {
        name: "bbox_project_list",
        category: ToolCategory::Projects,
        summary: "Compatibility attached-root discovery for bridge callers; remote-only catalog projects are not listed. List registered project roots with their project_id, repo_id (null for non-git), canonical_path, registered_at, and is_git_repo flag. Idempotent read; safe to call repeatedly. project_ids are stable across daemon restarts. Use this to check whether a path is already registered before asking the operator to register it.",
        when_to_use: "Use to inspect registered roots or confirm symlink aliases collapsed.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_unregister",
        category: ToolCategory::Projects,
        summary: "Unregister a project root from the bbox project registry. Accepts project (project_id, registered canonical_path, or absolute path). Removes the registry entry only; does NOT delete project-scoped state (knowledge, threads, Slack bindings, pollers, crons) keyed on the project_id, which is derived from the canonical realpath and is stable across unregister+re-register. By default refuses when refs still exist and returns the counts; pass force=true to orphan them, or bbox_project_rename to migrate first. dry_run=true previews counts without mutating the registry. In catalog mode unregister is detach: the attachment is marked detached with census deregistration scoped to its checkout and scope pair, every logical store keeps its rows, and catalog deletion is the offline project-catalog retire surface.",
        when_to_use: "Use to drop a stale or accidentally-registered project root without hand-editing projects.json. Prefer `dry_run=true` first to see what is still attached, then `bbox_project_rename` to migrate or `force=true` to accept orphaning. Compatibility unregistration can complete before auxiliary watcher cleanup fails; status=partial preserves the committed registry change.",
        example: Some(
            r#"bbox_project_unregister(project="/home/me/repos/dead-project", dry_run=true)"#,
        ),
    },
    // ── Project catalog administration ───────────────────────────────
    ToolDoc {
        name: "bbox_project_catalog_list",
        category: ToolCategory::ProjectCatalog,
        summary: "List project summary pages (default 20, maximum 100), ordered by project_id. Continue with next_offset and expected_catalog_epoch from the previous page to reject catalog changes. Filter by query; use bbox_project_catalog_get for aliases, connector observations, and attachment details. Returns error.project_catalog_inactive on the version-1 registry.",
        when_to_use: "Use to see the complete catalog, including projects with no local checkout, and to read the current epoch before any administration call. `bbox_project_list` still reports the attached version-1 rows.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_catalog_get",
        category: ToolCategory::ProjectCatalog,
        summary: "Read one project by exact selector. Default detail=summary returns identity, scope, epoch, alias previews (3 accepted and 3 pending, with totals), and recorded attachment counts/default. detail=aliases returns exact alias rows and offline operator accept arguments; detail=attachments returns recorded host-local rows, not proof of live checkout access; detail=observations returns producer-reported connector coordinates, not identity or freshness. Alias/attachment pages default to 20, clamp limit to 1..=100, and obey a byte budget. Continue with next_offset and expected_catalog_epoch; nonzero offset requires that epoch and changes refuse. No unbounded full option and no checkout probes. Returns error.project_catalog_inactive on the version-1 registry.",
        when_to_use: "Use when you need one project's aliases, pending nominations, repo history, or the attachments this host carries for it. Pair with `bbox_project_catalog_list` for the epoch.",
        example: Some(r#"bbox_project_catalog_get(project="p_4f6a1c9e5b2d47a8b0c3e1f5a9d76b24")"#),
    },
    ToolDoc {
        name: "bbox_project_attach",
        category: ToolCategory::ProjectCatalog,
        summary: "Local administrator operation: add an already initialized checkout to a project with existing daemon checkout authority. Transport-owned or remote-only projects return error.project_admin_locality_required before probes; source enrollment uses the checkout-host collector. The daemon never mints checkout identity here. The daemon probes the path off-lock (canonical checkout top, checkout identity, kind: base, linked worktree, or managed clone, committed scope at HEAD, observed capabilities) and the catalog transaction revalidates identity and uniqueness. A published project accepts only a checkout whose committed config proves the same scope exactly; a mismatch returns the scope-migration or promotion refusal instead of attaching. Well-formed, non-colliding aliases declared by the committed config are recorded as pending nominations, never accepted automatically. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use to give an existing catalog project a working checkout on this host. Read the epoch from `bbox_project_catalog_list` first. A scope mismatch refusal names promotion or scope migration as the next step; do not retry attach.",
        example: Some(
            r#"bbox_project_attach(project="p_4f6a1c9e5b2d47a8b0c3e1f5a9d76b24", path="/home/me/repos/blackbox", expected_catalog_epoch=7, audit_reason="new laptop checkout")"#,
        ),
    },
    ToolDoc {
        name: "bbox_project_detach",
        category: ToolCategory::ProjectCatalog,
        summary: "Detach one attachment: the row is marked detached with a timestamp, every logical store, entity ref, and generation is left untouched, and the catalog keeps its data. Census and watcher deregistration is scoped to the detached attachment's checkout and scope pair only, so a monorepo checkout carrying sibling attachments for other projects keeps their census rows and watcher coverage. Requires expected_catalog_epoch and a bounded audit_reason. After partial cleanup, repeat with the returned epoch to retry cleanup without reattaching; the validated retry advances the catalog epoch and preserves the original detach timestamp. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use when a checkout is going away or should stop being a local source for the project. Detach keeps every stored row: it is not deletion, and re-attaching later restores the local source. Catalog detachment can complete before auxiliary watcher/census cleanup fails; status=partial preserves the committed detach with cleanup outcomes. Retry partial cleanup with the same attachment and a fresh expected_catalog_epoch. An already detached attachment remains detached; the validated cleanup retry advances the epoch while retaining the attachment record.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_default_attachment",
        category: ToolCategory::ProjectCatalog,
        summary: "Record or clear the operator-selected default local-source attachment for one project. Path operations use it when no session pin and no explicit selector is present. The selection is host-local attachment data, never catalog data; it must name an active attachment of the same project, and omitting attachment_id clears it. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use when a project has several attachments on this host and path operations should prefer one. Omit `attachment_id` to clear the preference.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_promote",
        category: ToolCategory::ProjectCatalog,
        summary: "Promote a legacy-local catalog project to the published scope its checkouts now prove. Requires verified daemon checkout authority for every active attachment; transport-owned projects return error.project_admin_locality_required before probes. An administrator with the authoritative catalog and checkouts can use blackbox project-catalog promote; it does not call the remote daemon. Requires the exact project_id, the designated attachment, and the proposed repo_id and bbox_root_relpath. The daemon probes every active attachment of the project at HEAD; each one must prove the exact proposed scope or the promotion refuses with per-attachment diagnostics, and the designated attachment cannot overrule siblings. An owned scope refuses and points at the offline compatibility workflow rather than merging. One pair transaction flips the scope, writes the attachment-proved promotion record with its proof, and performs the repo-history authority transition. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use after a legacy-local project's checkouts have committed their repo_id and every active attachment resolves the same scope. Needs the exact project_id, which register refusals hand you.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_scope_migrate",
        category: ToolCategory::ProjectCatalog,
        summary: "Local administrator operation requiring verified daemon checkout authority for every active attachment. Transport-owned projects return error.project_admin_locality_required before probes; no remote attached-migration lane is implemented. Attachment-proved scope migration for a published catalog project: kind=relpath-move for a monorepo relocation, kind=repo-authority-change for a recorded-authority change. The daemon probes every active attachment at HEAD (and, for a relpath move, the relocated directory, which must exist) and the pair transaction rewrites the catalog scope, relocates the attachments, appends host-local path bindings, and writes the migration record with its proof. A repo-authority change requires acknowledge_repo_authority_change, which agents pass through from operator input and never default or infer. dry_run validates the complete mutation and commits nothing. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use when a published project moves inside its monorepo (`relpath-move`) or changes recorded repository authority (`repo-authority-change`). Run `dry_run=true` first. Only pass `acknowledge_repo_authority_change` when the operator explicitly authorized the authority change.",
        example: Some(
            r#"bbox_project_scope_migrate(project_id="p_4f6a1c9e5b2d47a8b0c3e1f5a9d76b24", expected_old_repo_id="r_9c1d", expected_old_relpath=".", new_repo_id="r_9c1d", new_relpath="services/api", kind="relpath-move", attachment_id="att_1b7d3f", dry_run=true, expected_catalog_epoch=7, audit_reason="monorepo relocation")"#,
        ),
    },
    ToolDoc {
        name: "bbox_project_publisher_bind",
        category: ToolCategory::ProjectCatalog,
        summary: "Local checkout administration: rebind a published project to another capable attachment. Transport-owned projects refuse with error.knowledge_transport_authoritative. Rebind the accepted-publication pointer of a published project to another of its attachments. The pointer's ref, accepted commit, accepted scope, generation, and payload bytes are unchanged: only the attachment binding moves, so the strict pointer and generation agreement holds identically before and after. The new attachment's object database must already contain the pointer's accepted commit, and a project with no pointer refuses rather than inventing one. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use after detaching or replacing the checkout that carried the publisher binding, so a later publication advance has a live attachment. Fetch the accepted commit into the new checkout first.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_publisher_advance",
        category: ToolCategory::ProjectCatalog,
        summary: "Operator moves of one published project's accepted-publication pointer that automatic acceptance does not make. Every Ready candidate from the bound producer on the accepted scope and configured ref is accepted as it finalizes, and a project with no pointer is established by its first valid candidate from the owning producer, so this tool never accepts routine content. operation=rebind moves the pointer onto a candidate from a different producer or ref (the candidate's ref becomes the configured ref), onto a producer from an attachment binding, or establishes a project whose first candidate was refused; operation=scope_move publishes at the catalog's current scope after a scope migration, clearing the bridge; operation=rollback serves a specific earlier Ready candidate from the bound producer, scope, and ref until the next candidate finalizes. Select exactly one source: source_generation_id names a Ready remote candidate whose producer, scope, ref, commit, and lanes come from pinned immutable evidence (full_ref is refused), while attachment_id with full_ref publishes an uncovered project's attached checkout (rebind or scope_move only). The pointer is compare-and-swapped against the pointer this call read, so a concurrent acceptance refuses the move instead of being overwritten. dry_run validates and writes nothing. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use only for a move acceptance does not make: after the collector's configured ref or producer changes (rebind), after a catalog scope migration (scope_move), or to serve an earlier candidate while a bad commit is reverted on the ref (rollback). Read bbox_project_publisher_status first; its acceptance.last_attempt names the refusal that calls for the move. Run dry_run=true first.",
        example: Some(
            r#"bbox_project_publisher_advance(project_id="p_4f6a1c9e5b2d47a8b0c3e1f5a9d76b24", operation="rollback", source_generation_id="kps_9f2c...", expected_catalog_epoch=7, audit_reason="serve the last good candidate until the revert merges")"#,
        ),
    },
    ToolDoc {
        name: "bbox_project_publisher_status",
        category: ToolCategory::ProjectCatalog,
        summary: "Read one catalog project's accepted-publication status: state, scope/ref/commit identity, typed source binding, advance availability, the generation_id and pointer_sha256 identities, and the latest candidate acceptance attempt. Default health and connector sections are compact bounded summaries that keep stale, unavailable, queued, and partial signals visible with total, status, and omission counts; recorded rows are observations, not live filesystem authority. Oversized summary strings become explicit size-and-truncation markers (diagnostics keep a bounded prefix) whose exact bytes live only in detail pages. detail=health returns the complete runtime view, detail=connector the complete connector view, and detail=acceptance the latest candidate acceptance attempt as exact bounded body pages; replay detail.body.next_cursor while the body is unchanged. Connector detail requires a connector-scoped project. Observational, path-free, and takes no checkout lease; see design/daemon-runtime/publisher-auto-advance.md for deep mechanics. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use to diagnose unavailable, prior-fallback, detached, or unserved published knowledge, and before bbox_project_publisher_advance. acceptance.last_attempt says why the latest Ready candidate was or was not accepted; detail=acceptance recovers it exactly. Use detail=health or detail=connector for exact diagnostics; a changed body refuses continuation. detail_limit and detail_cursor require detail; limits outside 4..4096 are rejected before collection.",
        example: None,
    },
    // ── Knowledge ────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_learn",
        category: ToolCategory::Knowledge,
        summary: "Persist an operator-approved rule or convention that should bind future sessions; rendered into provider markdown files. Pass render=false for an indexed-only recall entry that search finds but no rendered file carries. Use for narrative rules (\"we always X\", \"never Y\") only after the operator has approved the exact content and scope.",
        when_to_use: "Use only after the operator has approved the exact text and scope for a standing user rule that must outlive the current edit AND would still be correct a year from now with all current arcs complete. Anti-trigger: content naming a specific migration, phase, active arc, current initiative, or \"finish X before Y\" sequencing is arc-bound; it belongs in the dispatch brief or the work-item thread. Not for one-off task constraints, not for facts you discovered yourself (those belong in a thread note or your final report). Query `bbox_knowledge` first to avoid duplicate entries. On a transport-governed estate, project-scoped writes ride the checkout-owner backchannel: the daemon enqueues the committed `.bbox/knowledge/` bytes and the collector applies them within one cycle; commit the file to publish. See `sm-persistence-taxonomy` via `bbox_knowledge` for the deeper split.",
        example: Some(
            r#"bbox_learn(content="use rustls, not openssl", category="convention", scope="project", project="/repo/x")"#,
        ),
    },
    ToolDoc {
        name: "bbox_knowledge",
        category: ToolCategory::Knowledge,
        summary: "Query durable knowledge entries by free-text or filters. Use early when prior decisions, conventions, remembered facts, or system runbooks could change the answer. Also surfaces a bounded system-memory sidecar; system memories include system_memory:<id> refs usable with bbox_inspect_entity. Pass category=\"system_memory\" to list memory metadata.",
        when_to_use: "Use near the start of tasks where durable knowledge-store context could matter: prior decisions, project conventions, rendered rules, remembered facts, or system runbooks. This is not the surface for active threads (`bbox_thread_list`) or transcript history (`bbox_hybrid_search`). Prefer a short phrase from the user's request over a single generic keyword; adjacent terms broaden recall, quoted phrases stay exact, `AND` / `OR` work explicitly, and `-term` excludes. If the first query is empty or too broad, try one sharper phrase. Use `mode=substring` for literal whole-query matching. Add `project=<cwd>` when looking for an entry to update or replace; `project` also accepts a project_id or a registered operator alias and matches entries by project identity, and a value that resolves to no registered project keeps literal substring matching and says so in the response diagnostics. System memories can also be paged by canonical `sm-*` ID. Oversized structured entry content or metadata becomes a bounded preview whose detail recovery arguments carry the canonical entity_ref and the same filters; pass entry_detail=<entity_ref>, then concatenate body.text pages from detail_cursor through next_cursor and parse the complete JSON. Pass diagnostics_detail=true with the same filters to page exact omitted diagnostics. Content changes invalidate cursors; restart the same read without detail_cursor. offset continues the selected ranked knowledge page or system-memory catalog. Requests cap at 100 rows and 16 KiB selectors; complete-envelope budgeting can return fewer. Follow structuredContent.page.next_offset for knowledge. The selection is live, so concurrent changes can move rows.",
        example: Some(r#"bbox_knowledge(query="retry policy")"#),
    },
    ToolDoc {
        name: "bbox_forget",
        category: ToolCategory::Knowledge,
        summary: "Delete a knowledge entry.",
        when_to_use: "Entry is stale or replaced: forget it, then `bbox_learn` the replacement. Git history keeps prior versions of project entries. Pass project to select a checkout-owner project when IDs overlap or unrelated publications are unavailable; the selected project must contain the entry, with no fallback to another owner. Omit project for global or local-store entries. Unscoped mutations refuse when a unique owner cannot be established.",
        example: None,
    },
    ToolDoc {
        name: "bbox_render",
        category: ToolCategory::Knowledge,
        summary: "Render entries into CLAUDE.md / AGENTS.md / GEMINI.md.",
        when_to_use: "Use to publish standing approved knowledge into managed files. `global` patches the DAEMON HOST's memory files; when the daemon is remote (cage) or its store is isolated it refuses with `error.global_render_authority` instead of writing files nobody reads. To refresh an operator host's global files from a remote daemon, run `bro render global` ON THAT HOST (`--check` previews): it requests `bbox_render(scope=\"global\", global_plan={host_common_target})` and applies the returned managed bodies locally with backups. `project` renders the project's provider files (CLAUDE.md / AGENTS.md / GEMINI.md, which include PROJECT.md by reference) plus `.bbox/guidance` satellites IN THE CHECKOUT THAT OWNS IT: the code collector whose producer grant covers the project's published scope applies a daemon-built plan and returns a path-free receipt (`status`, per-output `dispositions`, `current`). The plan carries published knowledge only. Hand-authored provider files are preserved (`refused`); generated ones are replaced. If the owner has not answered within the wait, the response is `render_pending` with an `operation` id: call `bbox_render(project, operation)` to retrieve that operation's recorded receipt (it never re-applies; a newer render makes it historical). Refusals name the fix: `error.render_owner_*` for an owner that is stale, lacks the render lane, or does not hold the checkout, and `error.render_locality_required` when no owner covers the project (enroll the checkout with `bbox-code-collector add <path>` on its host). Do not use render as a way to keep active-work guidance hot across turns; that belongs in the dispatch brief or the work-item thread. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
        example: Some(r#"bbox_render(scope="project", project="/repo/x")"#),
    },
    // ── Threads ──────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_thread",
        category: ToolCategory::Threads,
        summary: "Open / continue / resolve / promote / rename / link a work thread. action=get returns a bounded summary; detail=notes|sessions|edges pages history, detail=note|handoff|metadata reads exact bodies through content-bound cursors. Metadata recovers complete topic, session and edge fields.",
        when_to_use: "Use for investigations or QC walks that span sessions. Before `action=open`, call `bbox_thread_list` to avoid duplicate threads. Use `kind=work_item` for orchestrator-led execution loops. For context recovery, `action=get` defaults to a bounded summary; page the history with `detail` and follow `next_offset`, then read one full note (`detail=note,note_index=N`) or the handoff doc (`detail=handoff`) exactly. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene. Explicit detail=summary is supported. detail=metadata recovers complete topic/name/session/edge metadata using body_limit/cursor. Wrong-action fields are rejected before mutation.",
        example: Some(r#"bbox_thread(action="get", id="thread-12345678", detail="notes")"#),
    },
    ToolDoc {
        name: "bbox_thread_list",
        category: ToolCategory::Threads,
        summary: "List thread summary pages (default 20, maximum 100), ordered by last activity then id. Continue with next_offset; use bbox_thread(action=get,id=...) for a bounded summary, then detail pages.",
        when_to_use: "Before starting work on a topic (continuity check). Use `status` for lifecycle (`open`, `active`, `resolved`, `promoted`) and `min_idle_days` to return only threads idle for at least N days. Filter by `kind=work_item`. Workflow-origin arc threads are hidden by default; pass `include_workflows=true` when you need historical workflow records.",
        example: None,
    },
    // ── Gap notes ────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_gap",
        category: ToolCategory::Gaps,
        summary: "File a first-class substrate gap note into the repo-owned gap store.",
        when_to_use: "When the blocker is in the blackbox substrate or shared agent workflow: a missing tool primitive, MCP surface, refactor atom, workflow shape, ontology edge, or runbook that agents in other projects could plausibly hit too, NOT an ordinary TODO in the current product codebase and NOT a user-stated rule (those go to bbox_learn). Dedupe first with `bbox_gaps` and reuse the same `dedupe_key` (`<gap_kind>/<domain>/<slug>`); an open gap with that key dedupes by default (pass `allow_recurrence=true` to tally a recurrence). Project-scoped by default (committed in-repo under `.bbox/gaps/`); pass `scope=\"global\"` for cross-project substrate gaps. On a transport-governed estate the daemon holds no checkout authority: the call still works, but the daemon enqueues the committed-file bytes and the checkout-owner collector writes them into the checkout within one collector cycle (the response says where; commit the file to publish). See `sm-gap-notes` via `bbox_knowledge`.",
        example: Some(
            r#"bbox_gap(title="No primitive classifies entities by rate within a time window", gap_kind="tooling", domain="review-policy", wanted_capability="Classify entities by count/rate within a time window.", dedupe_key="tooling/review-policy/rate-window-predicate", impact="medium")"#,
        ),
    },
    ToolDoc {
        name: "bbox_gaps",
        category: ToolCategory::Gaps,
        summary: "List paginated gap summaries with typed filters. Exact id defaults to full detail when it fits; oversized records keep previews and exact recovery hints. Use id with body_limit for exact record JSON pages, or diagnostics_detail=true for exact source diagnostics. Continue body_cursor=body.next_cursor with unchanged filters.",
        when_to_use: "The mandatory dedupe step before `bbox_gap`: search open gaps by `dedupe_key`, `gap_kind`, `domain`, `impact`, or free-text `query` before filing. Also the triage surface - pass `json=true` for machine-readable records to group/extract, or `include_addressed=true` to see closed gaps. Addressed gaps are hidden by default for lists, shown by default for an exact `id`. `project` accepts a project_id, a registered operator alias, or a project path, and matches rows by project identity; a value that resolves to no registered project keeps literal substring matching and says so in `diagnostics`, so an empty list is never silent about an unresolvable filter. Source availability warnings are summarized by default; debug=true returns up to 10 bounded diagnostic previews. Narrow project for scope-specific diagnostics. body_limit/body_cursor with an exact id recovers complete JSON using the same filters. Reads are published-only. diagnostics_detail=true with body_limit/body_cursor and no id recovers exact scoped diagnostics. Oversized records return a bounded preview with an exact reader.",
        example: Some(r#"bbox_gaps(gap_kind="mcp_surface", include_addressed=false)"#),
    },
    ToolDoc {
        name: "bbox_gap_resolve",
        category: ToolCategory::Gaps,
        summary: "Resolve a gap note (acknowledged/addressed); optionally wire a structured supersession link.",
        when_to_use: "Close a gap as addressed, or keep it visible as acknowledged. superseded_by links both records and requires a different gap in the same project. An id-only call resolves the owner from published or outstanding queued records; pass project (registered id or alias) when ambiguous. On the checkout-owner transport, success means queued delivery, not committed publication. Chained edits preserve outstanding changes through delivery; a conflicting publication refuses the edit until reconciled in the owning checkout. Global gaps update directly. The implementing commit should carry an Addresses-Gap-Note trailer.",
        example: Some(
            r#"bbox_gap_resolve(id="gap-a1b2c3d4", resolution="addressed", note="implemented in commit abc123")"#,
        ),
    },
    ToolDoc {
        name: "bbox_gap_update",
        category: ToolCategory::Gaps,
        summary: "Edit an existing gap note's fields in place.",
        when_to_use: "Amend title, capability, impact, blocking level, evidence or notes without filing another gap. Supplied evidence replaces the evidence list. Omitted fields remain unchanged. An id-only call resolves the owner from published or outstanding queued records; pass project (registered id or alias) when ambiguous. On the checkout-owner transport, success means queued delivery, not committed publication. Sequential edits compose, including after delivery while publication is pending. A conflicting publication refuses the edit until reconciled in the owning checkout. Global gaps update directly.",
        example: Some(
            r#"bbox_gap_update(id="gap-a1b2c3d4", impact="high", evidence=["src/foo.rs:120", "thread-7f01324e"])"#,
        ),
    },
    // ── Artifact catalog ─────────────────────────────────────────────
    ToolDoc {
        name: "bbox_artifact_install",
        category: ToolCategory::Artifacts,
        summary: "Install a brofile from an inline artifact object or explicit HTTP(S) URL. Supply exactly one; caller filesystem paths are rejected. Workflow, agent, packet, atom, cron and team installation is retired.",
        when_to_use: "List before installing. The installer validates brofile JSON and records its version. Caller paths are never read by this tool.",
        example: Some(
            r#"bbox_artifact_install(kind="brofile", artifact={"name":"reviewer","provider":"brodex","lens":"Review correctness and explain material findings."})"#,
        ),
    },
    ToolDoc {
        name: "bbox_artifact_list",
        category: ToolCategory::Artifacts,
        summary: "List installed artifact summaries with live next_offset pages. body_limit/cursor recovers the complete redacted inventory; metadata=true with kind/name and optional version reads an exact installation receipt. Retired kinds require an explicit kind filter.",
        when_to_use: "Inventory check before installing or superseding producer machinery. Use kind/name filters to inspect a specific artifact family. body_limit/cursor without limit/offset recovers the complete filtered inventory as JSON pages. metadata=true with kind/name and optional version recovers a complete redacted installation receipt; omit list/detail selectors. Source URLs and daemon paths are withheld in both modes. Oversized summary fields carry omission markers. Offset pages are live; concurrent catalog changes can move rows.",
        example: Some(r#"bbox_artifact_list(kind="brofile")"#),
    },
    ToolDoc {
        name: "bbox_artifact_supersede",
        category: ToolCategory::Artifacts,
        summary: "Mark one installed artifact superseded by another artifact of the same kind.",
        when_to_use: "Use when a customized brofile/agent replaces an installed version but you want the old version retained for audit. The receipt reports catalog_updated and runtime_deactivated separately; status=partial preserves a completed catalog write if runtime cleanup fails. Source URLs and daemon paths are withheld.",
        example: Some(
            r#"bbox_artifact_supersede(kind="brofile", name="reviewer", superseded_by="reviewer-v2")"#,
        ),
    },
    ToolDoc {
        name: "bbox_artifact_remove",
        category: ToolCategory::Artifacts,
        summary: "Hard-remove one installed artifact.",
        when_to_use: "Use for obsolete catalog artifacts that should be pruned, not superseded. dry_run=true lists paths; dry_run=false requires confirm=true.",
        example: None,
    },
    // ── Orchestration (bro) ──────────────────────────────────────────
    ToolDoc {
        name: "bro_exec",
        category: ToolCategory::Orchestration,
        summary: "Launch a fresh agent task/session and return {taskId, sessionId}. Optional request_key prevents duplicate launch after a lost reply. Required selector: provide either `bro`, `provider`, or runtime allocation fields such as `tier`, `pool_name`, `pin_provider`, `pin_model`, or `capabilities`.",
        when_to_use: "Use to start a fresh agent session only. Supply request_key before dispatch when an uncertain reply may need retry: repeat the same key and inputs in the same bound workspace. Keys never expire automatically. Changed inputs refuse reuse. admission_incomplete means execution_unknown: inspect the returned taskId; the same key never relaunches. Without a key, another call can start another task. A dispatch selector is required: pass exactly one selector family: `bro` for a named brofile, `provider` for a raw ad-hoc provider, or allocator fields (`tier`, `tier_ladder`, `tier_mode`, `min_tier`, `max_tier`, `pool_name`, `pool_providers`, `capabilities`, `selection_policy`, `pin_provider`, `pin_account`, `pin_model`, `pin_effort`, `prefer_provider`) for pool-backed runtime allocation. Set the session's working directory with `cwd` (canonical name; `project_dir` is accepted as a deprecated alias). Fresh-session overrides such as `service_tier` apply after selector resolution. Prefer `bro:` over raw `provider:` so routing stays stable when a named brofile exists. For providers with a known schedule, peak_usage reports whether the attempt started during peak hours; it is advisory, not a quota measurement or rate guarantee. Request-key replays retain the original attempt time. Record `taskId`, `sessionId`, and any `selectionTraceId`; operators inspect allocation decisions with `bro mcp call bro_allocator_trace '{\"selection_trace_id\":\"<id>\"}' --surface ops`. Without an account pin, allocation uses only that provider's declared default or native credentials; unrelated global accounts are not candidates. For any follow-up on that same work, use `bro_resume`; another `bro_exec` starts fresh and has no continuity.",
        example: Some(
            r#"bro_exec(prompt="review this patch", cwd="/repo/x", tier="standard", pool_name="coding", durable=true)"#,
        ),
    },
    ToolDoc {
        name: "bro_resume",
        category: ToolCategory::Orchestration,
        summary: "Continue an existing session with a follow-up; single-flight per provider session. Optional request_key prevents duplicate continuation after a lost reply.",
        when_to_use: "Use for follow-ups on an existing bro session. peak_usage has the bro_exec advisory contract, evaluated at the continuation attempt start. request_key has the same durable retry contract as bro_exec; use a new key for each intentional turn. Workflow/atom-owned sessions refuse ordinary resume. Do not use `bro_exec` again when you need continuity. Pass the `session_id` and `provider` returned by the original dispatch. The working-directory override is `cwd` (canonical; `project_dir` accepted as a deprecated alias), usually unnecessary because resume auto-resolves the session's recorded cwd. For Brodex, pass `service_tier=\"priority\"` to force fast routing for the continuation or `service_tier=\"default\"` to persist standard routing. `pin_model` / `pin_effort` override the model and reasoning effort for the resumed turns (absent ⇒ brofile/session default). Never call `bro_resume` on a session while its previous task is still running: first `bro_wait(task_id=...)`, or `bro_cancel(task_id=...)` if you are abandoning that turn. If a prior turn failed but the session is still useful, resume it with recovery context before starting a fresh `bro_exec`. See `sm-bro-dispatch-patterns` via `bbox_knowledge` for workflow shapes.",
        example: Some(
            r#"bro_resume(session_id="<sessionId>", provider="brodex", prompt="add tests for the edge case we discussed")"#,
        ),
    },
    ToolDoc {
        name: "bro_allocator_status",
        category: ToolCategory::Orchestration,
        summary: "Read pool-backed runtime allocation config plus bounded in-flight, probe, lease, and preview-candidate pages.",
        when_to_use: "Use when debugging or auditing late-bound bro dispatch: inspect effective tier mappings, pools, selection policies, and current runtime state. in_flight, probes, leases, and preview.candidates are paged (default 20, maximum 100 rows); continue each section from its next_offset. Large sections become counts with exact recovery hints. detail=config|preview|in_flight|probes|leases returns exact JSON body pages; continue with body.next_cursor while preserving runtime selectors. candidate_offset pages preview.candidates. Read one exact lane record with bro_allocator_probe body paging. Pass tier/pool/capability/pin fields to preview the candidate table without spawning a task or writing a lease. Candidate peak_usage records the known provider schedule at selection time; it does not change eligibility or scoring. Unreadable probe state is explicitly unavailable. Exact status detail rejects row paging selectors.",
        example: Some(
            r#"bro_allocator_status(project_dir="/repo/x", tier="standard", pool_name="coding")"#,
        ),
    },
    ToolDoc {
        name: "bro_allocator_trace",
        category: ToolCategory::Orchestration,
        summary: "Read one exact allocation trace body, page by page.",
        when_to_use: "Use when bro_exec returned selectionTraceId and you need to explain why the allocator selected or rejected provider/account/model lanes. The response carries a compact summary (first 20 candidates) plus the exact redacted trace body paged with body.next_cursor (4096-byte default budget, cursor=offset:0 to resume, SHA256 revision bound restarts safely when the trace changes).",
        example: Some(r#"bro_allocator_trace(selection_trace_id="alloc-0123abcd")"#),
    },
    ToolDoc {
        name: "bro_allocator_probe",
        category: ToolCategory::Orchestration,
        summary: "Read, update, or clear allocator probe state for a provider/account lane.",
        when_to_use: "Use to record credential, quota, cooldown, and probe-confidence observations consumed by allocator scoring and bro_allocator_status previews. This mutates allocator/probes.json; write failures return error.probe_persistence_failed; a failure after replacement leaves durability unconfirmed, so inspect the record before retrying. Read-only inspection returns a compact probe status plus the exact redacted record paged with body.next_cursor. Use bro_allocator_status for multi-lane overviews. Validate clear/update/cooldown and detail contradictions before mutation. Strict locked read-modify-write preserves corrupt bytes and concurrent lane updates; errors after replacement may leave durability unconfirmed.",
        example: Some(
            r#"bro_allocator_probe(provider="codex", quota_status="exhausted", quota_confidence="runtime_rate_limit", cooldown_ms=300000)"#,
        ),
    },
    ToolDoc {
        name: "bro_wait",
        category: ToolCategory::Orchestration,
        summary: "Observe one task until completion; never launches follow-up work. Timeout returns a snapshot, not proof the task is dead. If the result is empty or suspicious, inspect bro_status(tail=N) before resuming, cancelling, or treating it as success.",
        when_to_use: "After `bro_exec` or `bro_resume` when you need the result. USE MAXIMUM TIMEOUT for provider work. On timeout, or when a completed result is empty/suspicious, call `bro_status(tail=N)` before deciding the task is stuck, treating it as success, cancelling it, or dispatching replacement work. Completed replies include small deliverables inline. resultTruncated/resultCursor continues through bro_status(detail=result,cursor=...). structuredExitOmitted requires bro_status(detail=structured_exit); follow body.next_cursor to reconstruct the JSON value.  Timeout must be finite, nonnegative and representable by the platform deadline; zero is an immediate snapshot. Validation precedes observer registration.",
        example: None,
    },
    ToolDoc {
        name: "bro_when_all",
        category: ToolCategory::Orchestration,
        summary: "Observe ALL selected tasks until completion; never launches follow-up work. Use for concurrent waits after explicit dispatch.",
        when_to_use: "Fan-out/fan-in pattern. Pair with several `bro_exec` dispatches for blind deliberation / provider comparison. USE MAXIMUM TIMEOUT. The whole selection is validated before waiting: task_ids non-empty, every ID known (pruned IDs reject), at most 64 occurrences; duplicates are preserved with one row each. all_completed means every selected task reached a terminal state (failed/cancelled included); all_succeeded separately requires every task to have succeeded; outcome_counts gives the mix, and running tasks keep timed_out rows, so a timeout never becomes success. Large aggregates compact later rows behind resultsTruncated (never drop a task); read exact bodies with bro_status(detail=result,cursor=...). structuredExitOmitted requires bro_status(detail=structured_exit); follow body.next_cursor to reconstruct the JSON value.  Timeout must be finite, nonnegative and representable by the platform deadline, validated before observers. Zero is an immediate snapshot.",
        example: None,
    },
    ToolDoc {
        name: "bro_when_any",
        category: ToolCategory::Orchestration,
        summary: "Block until the FIRST task completes; use for races instead of polling each task yourself.",
        when_to_use: "Racing providers / fast-path resolution. First result wins, others keep running unless cancelled. The whole selection is validated before waiting: task_ids non-empty, every ID known (pruned IDs reject), at most 64 occurrences; duplicates are preserved with one row each. any_completed means at least one task reached a terminal state (failed/cancelled included); any_succeeded separately requires a successful completion; outcome_counts gives the mix, and rows keep each task's own status with timed_out for those still running. Large aggregates compact later rows behind resultsTruncated (never drop a task); read exact bodies with bro_status(detail=result,cursor=...). structuredExitOmitted requires bro_status(detail=structured_exit); follow body.next_cursor to reconstruct the JSON value.  Timeout must be finite, nonnegative and representable by the platform deadline, validated before observers. Zero is an immediate snapshot.",
        example: None,
    },
    ToolDoc {
        name: "bro_status",
        category: ToolCategory::Orchestration,
        summary: "Read task progress. lastAssistantSnippet previews the latest assistant text (up to 256 characters), when known. detail=result or structured_exit returns exact body pages; replay body.next_cursor to continue. debug adds execution diagnostics.",
        when_to_use: "Ordinary status and wait replies omit context telemetry; debug=true includes context diagnostics. Exact body reads omit repeated snippets and routine event telemetry. Default summary includes state, progress, blockers and result availability. Result and structured_exit detail returns body.text with format, offset and total_bytes; pages contain at most 4096 UTF-8 bytes. Replay body.next_cursor unchanged with the same task and detail. A changed body rejects the cursor; restart from its first page. Reassemble JSON structured_exit pages before parsing. tail applies only to summary, capped at 50 events and 8192 serialized bytes. debug adds accounting and worker-owned transcript coordinates; those coordinates are not caller file paths.",
        example: None,
    },
    ToolDoc {
        name: "bro_dashboard",
        category: ToolCategory::Orchestration,
        summary: "Page recent task summaries for lookup; do not take over another operator's task.",
        when_to_use: "Defaults to 20 rows, maximum 100. Follow next_offset with the same filters; order is start time descending then task ID. Live state may change between pages. Unknown provider or status filters fail explicitly. Use bro_status for exact results, and coordination wait tools when awaiting completion.",
        example: None,
    },
    ToolDoc {
        name: "bro_steer",
        category: ToolCategory::Orchestration,
        summary: "Queue a user steer into a running bro-harness process without cancelling the active turn.",
        when_to_use: "Use when a running bro should incorporate extra direction but does not need to stop its current turn. If the task already finished, use bro_resume instead. Only live harness child processes are steerable.",
        example: Some(r#"bro_steer(task_id="...", prompt="Prefer the smaller scoped fix.")"#),
    },
    ToolDoc {
        name: "bro_cancel",
        category: ToolCategory::Orchestration,
        summary: "Cancel a running task (SIGTERM); check bro_status first unless the user explicitly asked to stop.",
        when_to_use: "Task is confirmed stuck, you intentionally abandon a lost race, or the user asked to stop. A wait timeout is not enough evidence by itself; call `bro_status` first and avoid cancelling tasks you did not create unless instructed.",
        example: None,
    },
    ToolDoc {
        name: "bro_prune",
        category: ToolCategory::Orchestration,
        summary: "Drop terminal tasks from the store + persisted tasks.json; filter by status/provider/age, or pass task_ids to drop only specific tasks you created.",
        when_to_use: "Stale failed/completed/cancelled tasks are cluttering bro_dashboard. Cleanup is part of external orchestration hygiene, but prune only terminal tasks and prefer filters that match work you created. Defaults to status=failed. Pass task_ids=[…] to drop exactly the tasks you created without a status-wide sweep of the shared store (matches any terminal status unless status is also given). Filter by provider or older_than_hours; use dry_run=true to preview. Running tasks are never touched. Pass retro=true to fire a fire-and-forget workload retrospective on each pruned task before it's dropped (the bro self-files substrate gaps via bbox_gap only if something is worth surfacing); tune with retro_min_turns / retro_max. Rejects explicit empty task_ids and invalid providers. Freezes a maximum 256-task/24 KiB encoded-ID selection before effects; narrow larger selections. persistence=requested distinguishes admission from completion. Retro starts after tasks are dropped.",
        example: Some(r#"bro_prune(task_ids=["abc123"])"#),
    },
    ToolDoc {
        name: "bro_providers",
        category: ToolCategory::Orchestration,
        summary: "List provider summaries with current peak_usage advisories; pass provider to list its model slugs and reasoning efforts.",
        when_to_use: "Discover providers with no arguments. Pass provider=\"brodex\" (or another returned provider id) for that provider's model slugs and model-specific effort support. peak_usage is the current provider schedule advisory: true during peak, false off-peak, omitted when no schedule is known. GLM uses weekdays 14:00-18:00 Singapore (UTC+8); DeepSeek uses weekdays 01:00-04:00 and 06:00-10:00 UTC. This does not measure quota or execution-worker availability.",
        example: None,
    },
    ToolDoc {
        name: "bro_brofile",
        category: ToolCategory::Orchestration,
        summary: "Manage brofiles and accounts. list/list_accounts return bounded summary pages; get/get_account return exact redaction-safe JSON body pages.",
        when_to_use: "Create, inspect, and manage reusable bro blueprints. `action=list` returns sorted summaries (default limit=20, max=100) with total and next_offset; provider and name filter before pagination. scope selects one store exactly: global (default) or project (requires project_dir; global rejects it); unknown scopes are refused before any store access. `action=get` returns the exact stored brofile as bounded JSON body pages: concatenate body.text using cursor=body.next_cursor, then parse; cursors bind scope, selected store identity, name, and content. `context.provider_defaults` controls provider-default suppression; see `sm-brofile-context` via `bbox_knowledge` before composing minimal probes or strict suppression brofiles. Accounts live only in the daemon-owned global store: `list_accounts` returns bounded rows (counts plus a truncated environment-key preview) and `get_account` pages the exact redacted key/policy projection; set_account replies are compact summaries. Credential values are never returned. Before `action=create`, call `action=list` first to avoid duplicates. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene. Account and brofile reads distinguish corrupt/unreadable stores from empty or not-found. get_provider_default/list_provider_defaults support exact body_limit/cursor pages. Catalog-owned project configuration refuses without an owner transport; global and compatibility bridge lanes remain available.",
        example: Some(r#"bro_brofile(action="list")"#),
    },
    ToolDoc {
        name: "bro_mcp",
        category: ToolCategory::Orchestration,
        summary: "Manage MCP servers + tool filters for dispatched bros.",
        when_to_use: "Explicitly add/remove MCP servers and manage dispatch-time tool filters in the daemon-owned store. scope selects exactly one store: global (default, under BRO_HOME on the daemon host) or project (the selector resolves daemon-side to the project store; project is required with scope=project and rejected for global). list(body_limit=4096) pages the exact redacted inventory including full identities; ordinary list pages server rows (default 20, max 100, continue with offset; a past-end offset is an honest empty page, and the next-page hint repeats the required project selector) with bounded name/pattern previews; get_filters (same scope) pages the exact filter inventory when previews truncate - never use a mutating reset as read recovery. get reads the selected store and returns the exact redacted config (name, endpoint origin, header/env key identity, exclude list) as bounded JSON body pages: concatenate body.text using cursor=body.next_cursor, then parse; cursors bind scope, store identity, name, and content. A missing server reply names which store it is absent from. add supports http and sse transports only; stdio is rejected with no add lane. sync is retired: it fails honestly without reading or resolving stored secrets because no dispatch provider consumes persistent provider-CLI registration; dispatched bros receive per-dispatch injection. Replies are redacted (endpoint origins and credential key names visible, values and stdio arguments withheld) and never echo daemon-local store paths. The daemon does not rewrite provider MCP configs. Before `action=add`, call `action=list` first. The default bro-tool disallow is mechanical recursion protection, not just prose guidance. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene. Catalog-owned project configuration refuses without an owner transport. Global and compatibility bridge lanes remain available; retired sync refuses before project resolution.",
        example: Some(
            r#"bro_mcp(action="disallow", pattern="mcp__blackbox__bro_*", scope="global")"#,
        ),
    },
    // ── Workflows ────────────────────────────────────────────────────

    // ── Atoms ───────────────────────────────────────────────────
    // ── Operations ──────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_doctor",
        category: ToolCategory::Operations,
        summary: "Diagnose Blackbox health with ranked, paginated findings. format selects summary text or JSON; detail=full returns exact bounded body pages (cursor/body_limit). Narrow with section: the section name is validated before collection and collects only that section.",
        when_to_use: "Use as the first call when asking \"what do I need to know about Blackbox right now?\"; replaces the scattered manual smoke checklist (bbox_stats, bbox_embed_status, bbox_project_list) with one ranked surface. Route findings distinguish real failures (action) from opt-in absence like unconfigured visual chunk kinds (info). Default detail=summary returns up to 20 findings (max 100) ordered worst severity, section, then message; next_offset continues. Section status is separate from the findings page. detail=full returns a JSON envelope with exact body pages (body_limit default/max 4096 bytes, minimum 4). Concatenate body.text and replay body.next_cursor as cursor with the same section and format. Health is collected on each page; changed evidence rejects continuation, so restart without cursor. A requested section is validated BEFORE collection and collects only that section's existing producer instead of the full report. format=json does not imply full detail. Restart pagination after a state change.",
        example: Some(r#"bbox_doctor(format="summary")"#),
    },
    // ── Storage health ──────────────────────────────────────────────
    ToolDoc {
        name: "bbox_storage_health",
        category: ToolCategory::StorageHealth,
        summary: "Read daemon-owned edge storage totals and the ten largest contributors. include_files=true returns file pages (limit default 20, max 100); follow next_offset. File paths are relative to daemon storage, not caller-readable paths. Use bbox_storage_gc for managed cleanup. Each call rescans the selected daemon storage before projecting totals or file pages. Manifest and retention warnings remain visible.",
        when_to_use: "Paths in file pages are relative diagnostic coordinates in daemon-owned storage, not caller filesystem paths. Manifest and retention warnings remain visible. Use when diagnosing storage growth or validating retention before GC. Observed history is reported separately and retained by explicit keep/no-cap policy unless an operator supplies a cap to GC.",
        example: None,
    },
    ToolDoc {
        name: "bbox_storage_gc",
        category: ToolCategory::StorageHealth,
        summary: "Preview (default) or apply storage GC. Returns bounded counts, estimated bytes, protection counts, stage outcomes, and a receipt_id; exact detail is opt-in and paged.",
        when_to_use: "Use after bbox_storage_health. dry_run=true never deletes; dry_run=false runs a fresh native plan, not a saved preview. Responses use status, apply_requested and counts, not an applied boolean. status=applied means every requested stage completed without reported errors; partial means apply had errors, not that nothing changed. GC is non-atomic: earlier deletions remain and failed tree removals may have partial effects. incomplete is a preview with stage errors. deleted_bytes_estimate uses plan-time sizes of fully removed edge candidates and is not exact disk-space recovery. unconfirmed_count covers eligible candidates not confirmed removed (including native skips and errors). detail=candidates/deleted/errors/exclusions/full returns exact JSON body pages, default/max 4096 bytes, minimum 4; concatenate body.text fragments to recover the selected JSON. The first operation response includes totals even with detail. Later detail reads contain only outcome, receipt_id, expires_in_seconds, detail and body; read receipt_id with default detail=summary to recover totals. Continue using receipt_id, the SAME detail, and body.next_cursor only. Cursor without receipt_id, non-default GC options on a receipt read, and unknown options are rejected before work. Receipt reads never plan or delete. Receipts are daemon-local immutable reports, NOT executable plans or durable records: retained up to 15 minutes and 16 receipts, subject to earlier eviction/restart. Download needed details promptly; an unavailable receipt never triggers GC. The cache targets 64 MiB of serialized detail, retaining an oversized newest receipt alone rather than discarding post-apply evidence. External sweepers must read detail=exclusions in full before acting; summary counts do not authorize a sweep. Rollback-marker refusal and protected roots remain enforced. Native defaults retain newest backups and observed history; inactive snapshot age/recent/grace preferences do not override per-workspace count (32) and byte (8 GiB) ceilings. Active snapshots stay protected. Dangling/legacy orphans auto-prune only after grace; explicitly unregistered storage requires prune_explicitly_unregistered=true.",
        example: Some(
            r#"bbox_storage_gc(detail="candidates"); bbox_storage_gc(receipt_id="<returned id>", detail="candidates", cursor="<body.next_cursor>")"#,
        ),
    },
    // ── Reactions ──────────────────────────────────────────────────

    // ── Identity ─────────────────────────────────────────────────────
];

pub const WORKFLOW_NOTES: &str = "\
## Retrieval cues

If a tool stanza says `See: sm-...`, fetch that runbook on demand with \
`bbox_knowledge(query=\"sm-...\")`. Keep primitive semantics hot; pull deep \
workflow guidance only when you need it.

## Query semantics

- `bbox_hybrid_search` defaults to `mode=smart`: adjacent terms broaden recall, \
quoted phrases stay exact, and `-term` excludes. Use `mode=fulltext` when you want \
raw Tantivy/Lucene boolean syntax and conjunction semantics.
- `bbox_knowledge` uses the same natural query language by default. Use \
`mode=substring` only when you want literal whole-query matching instead of \
broader recall.

## Roles and the core loop

- **Orchestrator**: dispatches, reviews, creates and links a `bbox_thread` \
when a dispatch needs a durable record, and records durable commitments.
- **Executor**: when running as a dispatched bro/task actor, does the work and \
returns its result. The final answer is the report; nothing else needs filing.

## Ambient scope block

Dispatched agents receive pre-bound IDs (`session`, `project`, `bro`, and \
sometimes `thread` / `work_item`). Use them instead of reconstructing context \
from transcript history.

## Hot-path conventions

- List before create.
- `bro_exec` starts fresh; `bro_resume` continues. If you want continuity, \
record the returned `taskId`/`sessionId` and resume that session explicitly; \
do not call a second `bro_exec` and expect memory.
- Treat `bro_dashboard` as shared lookup, not ownership transfer. Do not \
resume, cancel, or prune a bro/task created by another external \
session unless the user explicitly asks. Prefer handles returned by your own \
dispatch.
- Before declaring a bro dead or cancelling after a timeout, call \
`bro_status(task_id=..., tail=N)`. A timeout can mean thinking, tests running, \
rate limiting, or failure; status/tail is the evidence.
- Use `bro_when_all` for fan-out/fan-in and `bro_when_any` for races. Do not \
hand-roll sequential wait/poll loops when the coordination primitive exists.
- After external orchestration, clean up only what you created — but only after \
explicit operator confirmation: terminal status is not the same as done, so ask \
before cleanup, and ask the operator to prune terminal tasks \
(`bro mcp call bro_prune '{\"task_ids\":[\"<id>\"]}' --surface ops`; offer \
`retro=true`). Cleanup is operator-gated, not automatic.
- Memory lanes: `bbox_thread` (investigation state), \
`bbox_learn` (operator-approved standing rules; `render=false` for cold \
grep-able facts). Arc-bound hot context belongs in the dispatch brief or the \
work-item thread. The one-year test decides: would it still be correct a year \
from now with current arcs done?
- Compose gates, retries, schedules and review protocols in your caller. Blackbox \
executes and resumes bro turns; it does not choose the next application step.
- `bbox_learn` is for operator-approved, user-stated rules; agent-discovered facts \
belong in a thread note or the final report.
";

fn system_memory_hint(doc: &ToolDoc) -> Option<String> {
    let joined = format!("{} {}", doc.summary, doc.when_to_use);
    let start = joined.find("sm-")?;
    let suffix = &joined[start..];
    let end = suffix
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(suffix.len());
    let id = &suffix[..end];
    Some(format!(
        "  _See:_ `{id}` via `bbox_knowledge(query=\"{id}\")`\n"
    ))
}

// ── Filter translation helpers ───────────────────────────────────────

/// Bare names of every tool in the blackbox catalog. Used as the
/// universe for glob expansion by provider filter translators that
/// can't accept glob patterns natively (Codex's `disabled_tools` /
/// `enabled_tools`, Gemini's policy engine).
pub fn all_tool_names() -> Vec<&'static str> {
    TOOL_DOCS.iter().map(|d| d.name).collect()
}

/// Prefixed bro_* tools blocked by the default recursion guard.
pub fn recursion_guard_tool_names_prefixed() -> Vec<String> {
    let prefix = blackbox_mcp_prefix();
    TOOL_DOCS
        .iter()
        .filter(|d| d.name.starts_with("bro_"))
        .map(|d| format!("{}{}", prefix, d.name))
        .collect()
}

/// Prefix convention for blackbox-served tools in provider tool namespaces.
/// Defaults to `mcp__blackbox__`, but follows `BLACKBOX_MCP_NAME` at runtime
/// so dev/prod daemons can coexist with distinct MCP entries.
pub fn blackbox_mcp_prefix() -> String {
    bbox_util::util::blackbox_mcp_prefix()
}

// ── Rendering ────────────────────────────────────────────────────────

/// Assemble an optional reference. Provider entrypoints use `render_bootstrap`
/// and the topic satellites; this reference is never an automatic import.
pub fn render_markdown() -> String {
    let mut out = String::new();
    out.push_str(
        "Blackbox tool reference — the MCP tools this daemon exposes and when to reach for them. ",
    );
    out.push_str(
        "This entry is generated from `crates/bbox-tool-docs/src/tool_docs.rs` and refreshed on every daemon restart. ",
    );
    out.push_str("Do not hand-edit.\n\n");

    render_retrieval_workflow(&mut out);
    render_persistence_workflow(&mut out);

    for cat in HOT_RENDER_CATEGORIES {
        out.push_str(&format!("## {}\n\n", cat.heading()));

        if let Some(memory_id) = deferred_system_memory(*cat) {
            out.push_str(&format!(
                "On-demand runbook: `{memory_id}` via `bbox_knowledge(query=\"{memory_id}\")`.\n\n"
            ));
            continue;
        } else {
            out.push_str(cat.intro());
            out.push_str("\n\n");
            for doc in TOOL_DOCS.iter().filter(|d| d.category == *cat) {
                out.push_str(&format!(
                    "- **`{}`** — {}\n",
                    doc.name,
                    hot_summary(doc.summary)
                ));
                if let Some(ex) = doc.example {
                    out.push_str(&format!("  _Example:_ `{ex}`\n"));
                }
                if let Some(hint) = system_memory_hint(doc) {
                    out.push_str(&hint);
                }
            }
            out.push('\n');
        }
    }

    out.push_str(WORKFLOW_NOTES);
    out
}

fn render_retrieval_workflow(out: &mut String) {
    out.push_str("## Retrieval workflow\n\n");
    out.push_str("Use Blackbox retrieval when stored decisions, conversation history, or indexed code evidence can change the answer. A direct local edit or an already-authoritative live result does not require a graph walk.\n\n");
    out.push_str("Use a short phrase from the task, not a single generic keyword. Query `bbox_knowledge` for durable rules and decisions, and `bbox_hybrid_search` for conversation history, indexed code or mixed evidence. Inspect relevant hits before relying on them.\n\n");
    out.push_str("Use tool-returned canonical entity refs and suggested fixes. Retrieve `sm-agentic-opening-sequence` when the question needs its answer protocol or recipes.\n\n");
}

fn render_persistence_workflow(out: &mut String) {
    out.push_str("## CORE RULE: operator-approved persistence\n\n");
    out.push_str("**When the user states a rule, convention, or preference that may need to bind future sessions, do not immediately call `bbox_learn`.** First decide whether persistence is warranted, then present the proposed memory text, lane, and scope to the operator and wait for explicit approval. Mechanical enforcement in code/config can enforce the current edit but does not transmit intent to future sessions; persistence still requires approval unless the operator has already approved the exact memory write in the current turn.\n\n");
    out.push_str("Triggers (positive and negative bind equally): \"from now on\", \"always X\", \"never X\", \"we (don't) use Y\", \"prefer Y\", \"X is banned / retired / out of scope\", \"stop using X\", \"no more X\", \"house rule\", \"standing order\", \"keep X out of\", \"X must not\".\n\n");
    out.push_str("Lane selection - when preparing a persistence proposal, walk the ladder and stop at the first yes:\n\n");
    out.push_str("1. Is this investigation state tied to one debug/QC walk? → `bbox_thread`\n");
    out.push_str("2. Would the statement still be correct a year from now with all current arcs complete? → propose `bbox_learn`\n");
    out.push_str("3. Is it a cold searchable fact worth grepping for later but not worth every session loading? → propose `bbox_learn` with `render=false`\n\n");
    out.push_str("The one-year test at step 2 is the load-bearing filter. Content naming a specific migration, phase, active arc, current initiative, or \"finish X before Y\" sequencing fails it: it belongs in the dispatch brief or the work-item thread, not `bbox_learn`. Ephemeral task constraints (\"for this fix, skip tests\", \"just for today\") don't get persisted at all.\n\n");
    out.push_str("After implementing any user directive in code/config, explicitly ask yourself: did the user just state a standing rule? If yes, propose the exact storage text, lane, and scope before replying; only emit the storage call after the operator approves it.\n\n");

    out.push_str("**Scope selection.** Default to `project` for repo-local conventions. Choose `global` only when the user's phrasing explicitly reaches beyond this repo — \"across every project\", \"on every machine\", \"in every X I write\", \"I always X as a personal rule\", \"house rule on this machine\". Technology-scoped but project-agnostic statements (\"in all Rust code I write\", \"always prefer fd over find\") are `global`. Strong wording alone is not enough — \"we always use tokio here\" stays `project`. Presence of a current project does not imply `project` scope when the user states a cross-project personal rule. If both readings are plausible, choose `project`.\n\n");
}

fn hot_summary(summary: &'static str) -> Cow<'static, str> {
    // Cap at 200 bytes — long enough for one or two informative sentences per
    // tool, short enough that the rendered tool reference stays skimmable and
    // within the always-hot global-memory budget (asserted by
    // `rendered_tool_reference_stays_prompt_sized`). Earlier value of 12
    // truncated mid-word ("Hybrid BM25+ See MCP."); 240 let the reference creep
    // over budget as the tool catalog grew. Tighten this knob (not the budget,
    // per the render-hygiene convention) if the reference grows again.
    const MAX_SUMMARY_BYTES: usize = 200;
    if summary.len() <= MAX_SUMMARY_BYTES {
        return Cow::Borrowed(summary);
    }
    // Prefer breaking at a sentence boundary when one fits inside the cap.
    let end = summary[..MAX_SUMMARY_BYTES]
        .rfind(". ")
        .map(|idx| idx + 1)
        .unwrap_or_else(|| {
            // Fall back to the last word boundary so we don't truncate a
            // word in half. Walk backward from the cap to find the last space.
            summary[..MAX_SUMMARY_BYTES]
                .rfind(' ')
                .unwrap_or(MAX_SUMMARY_BYTES)
        });
    Cow::Owned(format!("{} See MCP.", summary[..end].trim()))
}

/// The always-loaded contract stays independent of catalog size.
pub fn render_bootstrap() -> String {
    "Use Blackbox when stored context or its operations are relevant to the task. \
     Read only the matching task guide; unrelated work needs no Blackbox opening sequence.\n\n\
     Durable rules and decisions require operator approval of the exact text before persistence. \
     List before creating dedupe-sensitive objects. Use returned canonical refs.\n"
        .to_string()
}

fn topic_for_category(category: ToolCategory) -> GuidanceTopic {
    match category {
        ToolCategory::Transcripts | ToolCategory::Graph | ToolCategory::ProjectGraphs => {
            GuidanceTopic::Retrieval
        }
        ToolCategory::Knowledge => GuidanceTopic::Persistence,
        ToolCategory::Threads | ToolCategory::Artifacts | ToolCategory::Orchestration => {
            GuidanceTopic::Orchestration
        }
        _ => GuidanceTopic::Operations,
    }
}

pub fn render_satellite(topic: GuidanceTopic) -> String {
    let mut out = String::new();
    match topic {
        GuidanceTopic::Retrieval => render_retrieval_workflow(&mut out),
        GuidanceTopic::Persistence => render_persistence_workflow(&mut out),
        GuidanceTopic::Orchestration => out.push_str(WORKFLOW_NOTES),
        _ => {}
    }
    for cat in HOT_RENDER_CATEGORIES.iter().copied().chain([
        ToolCategory::Gaps,
        ToolCategory::Operations,
        ToolCategory::StorageHealth,
        ToolCategory::ProjectCatalog,
        ToolCategory::ProjectGraphs,
    ]) {
        if topic_for_category(cat) != topic {
            continue;
        }
        out.push_str(&format!("## {}\n\n{}\n\n", cat.heading(), cat.intro()));
        if let Some(memory) = deferred_system_memory(cat) {
            out.push_str(&format!(
                "Detailed runbook: `bbox_knowledge(query=\"{memory}\")`.\n\n"
            ));
        }
        for doc in TOOL_DOCS.iter().filter(|doc| doc.category == cat) {
            out.push_str(&format!("- `{}`: {}\n", doc.name, doc.summary));
            if let Some(example) = doc.example {
                out.push_str(&format!("  Example: `{example}`\n"));
            }
        }
        out.push('\n');
    }
    out
}

// ── Sync into knowledge store ────────────────────────────────────────

pub struct SyncResult {
    /// true = upsert wrote to disk; false = content unchanged
    pub wrote: bool,
    pub bytes: usize,
}

/// Upsert the generated bootstrap and topic guides under stable IDs.
/// Idempotent: no-op if content and placement are unchanged.
pub fn sync_into_knowledge(kb: &mut bbox_knowledge::knowledge::Knowledge) -> Result<SyncResult> {
    let mut result = SyncResult {
        wrote: false,
        bytes: 0,
    };
    // Publish satellite source entries first, then replace the old monolithic entry.
    for topic in [
        GuidanceTopic::Retrieval,
        GuidanceTopic::Persistence,
        GuidanceTopic::Orchestration,
        GuidanceTopic::Operations,
    ] {
        let r = sync_entry(
            kb,
            &format!("bb-guide-{}", topic.slug()),
            &format!("Blackbox {} guide", topic.slug()),
            render_satellite(topic),
            RenderPlacement::Satellite { topic },
        )?;
        result.wrote |= r.wrote;
        result.bytes += r.bytes;
    }
    let r = sync_entry(
        kb,
        TOOL_DOC_ENTRY_ID,
        "Blackbox essentials",
        render_bootstrap(),
        RenderPlacement::Inline,
    )?;
    result.wrote |= r.wrote;
    result.bytes += r.bytes;
    Ok(result)
}

fn sync_entry(
    kb: &mut bbox_knowledge::knowledge::Knowledge,
    id: &str,
    title: &str,
    content: String,
    placement: RenderPlacement,
) -> Result<SyncResult> {
    let bytes = content.len();
    // Look for existing entry by stable ID
    let existing = kb.all_entries().iter().find(|e| e.id == id).cloned();

    if let Some(ref e) = existing {
        if e.content == content && e.render_placement == placement && e.render {
            return Ok(SyncResult {
                wrote: false,
                bytes,
            });
        }
    }

    let now = bbox_util::util::now_iso();
    let entry = KnowledgeEntry {
        render_placement: placement,
        id: id.to_string(),
        title: title.to_string(),
        content,
        cluster: None,
        category: Category::Tool,
        scope: Scope::Global,
        project: None,
        project_id: None,
        providers: Vec::new(),
        priority: Priority::Standard,
        render: true,
        created_at: existing
            .as_ref()
            .map(|e| e.created_at.clone())
            .unwrap_or_else(|| now.clone()),
        updated_at: now,
        recall_count: 0,
        last_recalled: None,
    };

    kb.upsert_generated(entry)?;
    Ok(SyncResult { wrote: true, bytes })
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_migrates_the_monolith_and_rerender_keeps_guides_deferred() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut kb =
            bbox_knowledge::knowledge::Knowledge::open(&root.join("knowledge.json")).unwrap();
        assert!(sync_into_knowledge(&mut kb).unwrap().wrote);
        let mut legacy = kb
            .all_entries()
            .iter()
            .find(|e| e.id == TOOL_DOC_ENTRY_ID)
            .unwrap()
            .clone();
        legacy.content = render_markdown();
        kb.upsert_generated(legacy).unwrap();
        assert!(sync_into_knowledge(&mut kb).unwrap().wrote);
        assert!(!sync_into_knowledge(&mut kb).unwrap().wrote);
        let request = bbox_knowledge::knowledge::GlobalRenderPlanRequestV1 {
            host_common_target: root.join("BLACKBOX.md").display().to_string(),
            offset: None,
            plan_sha256: None,
        };
        let plan = kb.global_render_plan(Some("agents"), &request).unwrap();
        let body = &plan.providers[0].body;
        assert!(
            body.len() < 2_500,
            "generated inline overhead: {} bytes",
            body.len()
        );
        assert!(!body.contains("bro_exec"));
        assert!(!body.contains("bbox_describe_schema"));
        assert_eq!(plan.satellites.len(), 4);
        for file in &plan.satellites {
            assert!(body.contains(&file.path));
        }
        assert_eq!(
            plan,
            kb.global_render_plan(Some("agents"), &request).unwrap()
        );
    }

    #[test]
    fn generated_bootstrap_stays_small_and_procedures_are_deferred() {
        let bootstrap = render_bootstrap();
        assert!(bootstrap.len() < 1_024);
        assert!(!bootstrap.contains("bbox_describe_schema"));
        assert!(!bootstrap.contains("bro_exec"));
        let retrieval = render_satellite(GuidanceTopic::Retrieval);
        assert!(retrieval.contains("bbox_hybrid_search"));
        assert!(!retrieval.contains("bro_exec"));
        assert!(!retrieval.contains("Cost of a wasted query"));
        assert!(!retrieval.contains("run this five-step sequence"));
        let operations = render_satellite(GuidanceTopic::Operations);
        assert!(operations.contains("bbox_gap"));
        assert!(operations.contains("sm-gap-notes"));
        let persistence = render_satellite(GuidanceTopic::Persistence);
        assert!(persistence.contains("operator-approved persistence"));
    }

    #[test]
    fn render_contains_hot_tool_names() {
        let md = render_markdown();
        for doc in TOOL_DOCS {
            if !HOT_RENDER_CATEGORIES.contains(&doc.category) {
                continue;
            }
            if deferred_system_memory(doc.category).is_some() {
                continue;
            }
            assert!(
                md.contains(doc.name),
                "rendered markdown missing {}",
                doc.name
            );
        }
    }

    #[test]
    fn render_defers_deep_tool_categories_to_system_memories() {
        let md = render_markdown();
        for (cat, memory_id) in [(ToolCategory::Orchestration, "sm-bro-dispatch-patterns")] {
            assert!(md.contains(&format!("## {}", cat.heading())));
            assert!(md.contains(&format!(
                "`{memory_id}` via `bbox_knowledge(query=\"{memory_id}\")`"
            )));
            for doc in TOOL_DOCS.iter().filter(|d| d.category == cat) {
                assert!(
                    !md.contains(&format!("- **`{}`**", doc.name)),
                    "deferred category rendered tool stanza for {}",
                    doc.name
                );
            }
        }
    }

    #[test]
    fn rendered_tool_reference_stays_prompt_sized() {
        let md = render_markdown();
        assert!(
            md.len() < 25_000,
            "rendered tool reference is too large for always-hot global memory: {} bytes",
            md.len()
        );
    }

    #[test]
    fn retired_roadmap_is_absent_from_discovery_and_rendered_guidance() {
        assert!(TOOL_DOCS.iter().all(|doc| doc.name != "bbox_roadmap"));
        assert!(!render_markdown().contains("bbox_roadmap"));
        assert!(!render_markdown().contains("## Roadmap"));
        assert!(
            parse_registered_tools()
                .iter()
                .all(|(name, _)| name != "bbox_roadmap")
        );
    }

    #[test]
    fn render_includes_workflow_notes() {
        let md = render_markdown();
        assert!(md.contains("## Roles and the core loop"));
        assert!(md.contains("Ambient scope"));
        assert!(md.contains("Retrieval cues"));
    }

    #[test]
    fn render_includes_system_memory_hint() {
        let md = render_markdown();
        assert!(md.contains("sm-bro-dispatch-patterns"));
        assert!(md.contains("bbox_knowledge(query=\"sm-bro-dispatch-patterns\")"));
        assert!(!md.contains("bro_orchestrate_run"));
        assert!(!md.contains("## Whiteboards"));
    }

    #[test]
    fn recall_guidance_prefers_phrase_queries() {
        let md = render_markdown();
        assert!(md.contains("short phrase"));
        assert!(md.contains("single generic keyword"));
        assert!(md.contains("bbox_knowledge(query=\"retry policy\")"));
        assert!(!md.contains("bbox_knowledge(query=\"retry\")"));
        assert!(!md.contains("query=<one keyword>"));
    }

    /// Parse `#[tool(...)]` attributes from Rust source files. Tolerates:
    ///   - single-line and multi-line attribute bodies
    ///   - `name` and `description` in any order
    ///   - arbitrary whitespace between `=` and the string literal
    ///
    /// Does NOT tolerate: escaped double-quotes inside the string literal
    /// (none of our descriptions need them). Returns (name, description)
    /// pairs. If either field is absent on a given attr, that attr is
    /// skipped — `every_registered_tool_has_a_doc` covers the missing-doc
    /// case separately.
    fn parse_registered_tools() -> Vec<(String, String)> {
        // The #[tool] registrations live in the root crate's src/ (this
        // crate holds only the doc stanzas).
        let src_dir = runtime_workspace_root().join("src");
        let mut paths = Vec::new();
        collect_rust_files(&src_dir, &mut paths);
        paths.sort();

        let mut out = Vec::new();
        for path in paths {
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
            out.extend(parse_registered_tools_from_source(&src));
        }
        out
    }

    /// Workspace root of the checkout the tests are RUNNING in, resolved at
    /// runtime — never `env!("CARGO_MANIFEST_DIR")`, which is baked at
    /// compile time: a test binary carried into a worktree by the seed_dirs
    /// CoW `target/` clone (or any cached build) would scan the checkout it
    /// was COMPILED in and silently pass on handlers the worktree added
    /// (gap-271a5847; bit for real during the badgey dissolution). Cargo and
    /// nextest both set the test cwd to the running checkout's package
    /// manifest dir, so walking up to the `[workspace]` manifest lands on
    /// the right tree; failing to find one is a loud error, never a fallback
    /// to the baked path.
    fn runtime_workspace_root() -> std::path::PathBuf {
        let cwd = std::env::current_dir().expect("test cwd must be readable");
        let mut cursor = cwd.as_path();
        loop {
            let manifest = cursor.join("Cargo.toml");
            if manifest.is_file()
                && std::fs::read_to_string(&manifest)
                    .map(|raw| raw.contains("[workspace]"))
                    .unwrap_or(false)
            {
                return cursor.to_path_buf();
            }
            cursor = cursor.parent().unwrap_or_else(|| {
                panic!(
                    "no [workspace] Cargo.toml above test cwd {} — cannot locate the \
                     running checkout's root src/ to scan for #[tool] registrations",
                    cwd.display()
                )
            });
        }
    }

    fn collect_rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|err| panic!("failed to read source dir {}: {err}", dir.display()));
        for entry in entries {
            let entry = entry.unwrap_or_else(|err| panic!("failed to read source entry: {err}"));
            let path = entry.path();
            if path.is_dir() {
                collect_rust_files(&path, out);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    fn parse_registered_tools_from_source(src: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut cursor = 0;
        while let Some(open) = src[cursor..].find("#[tool(") {
            let attr_start = cursor + open + "#[tool(".len();
            // Find the matching `)]` — simple paren-balance, which is
            // fine since our attr bodies never contain raw parens.
            let mut depth = 1;
            let mut i = attr_start;
            let bytes = src.as_bytes();
            let mut in_str = false;
            while i < bytes.len() && depth > 0 {
                let c = bytes[i] as char;
                if in_str {
                    if c == '\\' {
                        i += 2;
                        continue;
                    }
                    if c == '"' {
                        in_str = false;
                    }
                } else {
                    match c {
                        '"' => in_str = true,
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                }
                i += 1;
            }
            if depth != 0 {
                break;
            }
            let body = &src[attr_start..i - 1];
            cursor = i;

            let name = extract_string_arg(body, "name");
            let desc = extract_string_arg(body, "description");
            if let (Some(n), Some(d)) = (name, desc) {
                if n.starts_with("bbox_")
                    || n.starts_with("bro_")
                    || n.starts_with("badgey_")
                    || n.starts_with("consultant_")
                    || n.starts_with("work_")
                    || n.starts_with("atom_")
                    || n.starts_with("reaction_")
                    || n.starts_with("identity_")
                {
                    out.push((n, d));
                }
            }
        }
        out
    }

    /// Extract `key = "value"` from an attribute body. Whitespace-tolerant.
    /// Returns the value with `\"` and `\\` unescaped so it matches how
    /// Rust's compile-time string literals round-trip into runtime str.
    fn extract_string_arg(body: &str, key: &str) -> Option<String> {
        let needle = key.to_string();
        let mut start = 0;
        while let Some(pos) = body[start..].find(&needle) {
            let abs = start + pos;
            // Require preceding char to be non-identifier (start-of-body,
            // whitespace, or comma) so `description` doesn't match inside
            // some other identifier.
            let ok_before = abs == 0
                || matches!(
                    body.as_bytes()[abs - 1] as char,
                    ' ' | '\t' | '\n' | '\r' | ',' | '('
                );
            start = abs + needle.len();
            if !ok_before {
                continue;
            }
            let after = &body[start..];
            let after = after.trim_start();
            let Some(after) = after.strip_prefix('=') else {
                continue;
            };
            let after = after.trim_start();
            let Some(after) = after.strip_prefix('"') else {
                continue;
            };
            // Walk the string literal, honoring `\\` and `\"` escapes so
            // descriptions that quote `mode="first"` or `"we always X"`
            // round-trip correctly.
            let mut out = String::new();
            let mut chars = after.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => match chars.next()? {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        other => {
                            out.push('\\');
                            out.push(other);
                        }
                    },
                    '"' => return Some(out),
                    _ => out.push(c),
                }
            }
            return None;
        }
        None
    }

    #[test]
    fn every_registered_tool_has_a_doc() {
        // Asserts each #[tool]-registered name has a ToolDoc stanza.
        let registered: Vec<String> = parse_registered_tools()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(
            !registered.is_empty(),
            "no tools found under src/ — parse regressed"
        );

        let documented: std::collections::HashSet<&str> =
            TOOL_DOCS.iter().map(|d| d.name).collect();

        let missing: Vec<&str> = registered
            .iter()
            .filter(|n| !documented.contains(n.as_str()))
            .map(|s| s.as_str())
            .collect();

        assert!(
            missing.is_empty(),
            "tools registered under src/ without a ToolDoc stanza: {missing:?}"
        );

        let registered_set: std::collections::HashSet<&str> =
            registered.iter().map(|s| s.as_str()).collect();
        let extra: Vec<&str> = TOOL_DOCS
            .iter()
            .map(|d| d.name)
            .filter(|n| !registered_set.contains(n))
            .collect();
        assert!(
            extra.is_empty(),
            "ToolDoc stanzas without a matching #[tool] registration: {extra:?}"
        );
    }

    /// The search tools name the graph planes that have indexed documents,
    /// so a caller never reads an empty graph result from an unindexed plane
    /// as "no match". Drop a plane from the caveat when its indexing lands.
    #[test]
    fn search_tool_docs_name_only_indexed_graph_planes() {
        let doc = TOOL_DOCS
            .iter()
            .find(|doc| doc.name == "bbox_hybrid_search")
            .expect("missing tool doc for bbox_hybrid_search");
        assert!(
            doc.when_to_use.contains(
                "only `published` has indexed documents, so `connector` is accepted but \
                 returns no graph hits"
            ),
            "bbox_hybrid_search must state which graph_source planes have indexed documents"
        );
    }

    #[test]
    fn description_summary_parity() {
        // Fourth-surface invariant: the per-call chooser blurb in
        // `#[tool(description = ...)]` (src/**/*.rs) must equal the
        // managed-layer `ToolDoc.summary` (this file). They're the same
        // text to the agent — let them drift and the agent gets
        // contradictory guidance at the two surfaces.
        let registered = parse_registered_tools();
        let summaries: std::collections::HashMap<&str, &str> =
            TOOL_DOCS.iter().map(|d| (d.name, d.summary)).collect();

        let mut mismatches: Vec<String> = Vec::new();
        for (name, desc) in &registered {
            let Some(summary) = summaries.get(name.as_str()) else {
                continue;
            };
            if desc != *summary {
                mismatches.push(format!(
                    "\n  {name}:\n    main.rs    : {desc:?}\n    tool_docs  : {summary:?}",
                ));
            }
        }

        assert!(
            mismatches.is_empty(),
            "#[tool(description)] strings in main.rs must match the corresponding \
             ToolDoc.summary strings in tool_docs.rs. Mismatches:{}",
            mismatches.join(""),
        );
    }
}
