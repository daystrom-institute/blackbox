//! Single source of truth for the agent-facing tool reference.
//!
//! Every `bbox_*` / `bro_*` MCP tool registered in `main.rs` must have
//! a matching stanza in `TOOL_DOCS`. A unit test enforces this.
//!
//! On daemon startup, `sync_into_knowledge` upserts a fixed-ID global
//! knowledge entry (`bb-tool-reference`) rendered from the hot subset of
//! `TOOL_DOCS` + `WORKFLOW_NOTES`. Deep topics stay as system memories,
//! discoverable through `bbox_knowledge` when the agent actually needs the
//! runbook, rather than bloating every global render.
//!
//! Adding or changing a tool = one edit here. No hand-curated drift.

use std::borrow::Cow;

use anyhow::Result;

use bbox_knowledge::knowledge::{Approval, Category, KnowledgeEntry, Priority, Scope, Status};

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
    Notes,
    Gaps,
    Inbox,
    Artifacts,
    Packets,
    Orchestration,
    Roadmap,
    StorageHealth,
    Workspace,
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
            Self::Notes => "Side-channel notes",
            Self::Gaps => "Gap notes",
            Self::Inbox => "Attention / inbox",
            Self::Artifacts => "Artifact catalog",
            Self::Packets => "Rule-packets",
            Self::Orchestration => "Bro orchestration",
            Self::Roadmap => "Roadmap",
            Self::StorageHealth => "Storage health",
            Self::Workspace => "Tool-call history",
            Self::Operations => "Operations",
        }
    }

    fn intro(&self) -> &'static str {
        match self {
            Self::Transcripts => {
                "Search and read across every Claude Code / Codex / Gemini session the host has recorded. Reach for these when the user asks about past conversations, when you need to cite the origin of a rule, or when you need context around a prior decision."
            }
            Self::Graph => "Inspect entities, graph vocabulary, paths, bundles, and retrieval.",
            Self::ProjectGraphs => "Read project-owned reflective graph generations.",
            Self::Projects => "Register project roots for later file indexing.",
            Self::ProjectCatalog => {
                "Durable project-catalog administration: attach and detach local checkouts, select the default attachment, promote a legacy-local project to its committed scope, migrate a published scope, and rebind the publisher attachment. Every one of these refuses with `error.project_catalog_inactive` while the version-1 registry is the runtime authority; the proofless-authority operations (catalog add, alias accept and reject, retire) live on the offline `blackbox project-catalog` CLI instead."
            }
            Self::Knowledge => {
                "Memory lanes: `bbox_learn` for operator-approved rendered rules, `bbox_remember` for approved cold recall, `bbox_decide` for approved durable commitments, and `bbox_pin` for scoped active context."
            }
            Self::Threads => {
                "Track non-dispatchable work that spans sessions (investigations, QC walks, debugging, refinement loops). Lighter than the full dispatch pipeline, heavier than memory. Use `kind=work_item` for orchestrator-led propose→execute→review→refine loops."
            }
            Self::Notes => {
                "Structured side channel for *notable* observations surfaced during delegated work — orchestrators query `bbox_notes` / `bbox_inbox` at round boundaries. Seven kinds: `dispute`, `assumption`, `surprise`, `followup`, `blocked`, `learned`, `done`. Emit one only when you have something genuinely worth flagging; this is a signal channel, not a progress log, and silence is the right default when nothing is notable. A `done` note with a one-line acceptance summary is useful when an explicit caller contract asks for a structured sign-off — it is not required on every dispatch."
            }
            Self::Gaps => {
                "First-class substrate gap-note store. File a gap when the blocker is in the blackbox substrate or shared agent workflow — a missing tool primitive, MCP surface, refactor atom, workflow shape, ontology edge, or runbook that agents in other projects could plausibly hit too — not in the current product codebase. Project-scoped gaps are repo-owned (committed under `<project>/.bbox/gaps/`, travel with the checkout); cross-project substrate gaps go to the central host store with `scope=\"global\"`. `bbox_gap` files (typed, validated, deduped by `dedupe_key`), `bbox_gaps` filters by typed fields, `bbox_gap_resolve` closes out (with structured supersession), `bbox_gap_update` edits in place. See `sm-gap-notes` via `bbox_knowledge` for the full envelope, vocabularies, and lifecycle."
            }
            Self::Inbox => {
                "Attention aggregator: a single read that surfaces unresolved notes, stale threads, unverified knowledge, and failed tasks. Run at round boundaries, morning-brief style, and whenever you're unsure what needs attention next."
            }
            Self::Artifacts => {
                "Versioned catalog for packets, brofiles, simple agents and teams. Supply artifact JSON inline or by HTTP(S) URL. Explicit retired-kind filters retrieve historical receipts."
            }
            Self::Packets => {
                "Reusable judges compiled from examples or stated rules. If your task involves writing a priority-ordered rubric, ranking a batch against shared criteria, compressing an access table, coordinating sub-agents against identical standards, or classifying future cases the same way you classified past ones — compile a packet. `bbox_compile` authors the mechanism, `bbox_apply` evaluates any entity deterministically (no LLM), `bbox_audit` self-validates against known labels. Packets are portable: dispatch `packet_id` to sub-agents and every one of them produces bit-identical output. See `sm-rule-packets` via `bbox_knowledge` for the full runbook."
            }
            Self::Orchestration => {
                "Dispatch agents across the providers listed by bro_providers. Prefer named `bro` targeting (resolves provider + account + lens + context + session automatically) over raw provider. Core pattern: `bro_exec` to launch, `bro_wait` or `bro_when_all` to block, `bro_resume` for follow-ups (never `bro_exec` again — it starts fresh with no memory). For ensembles: `bro_broadcast` + `bro_when_all` (blind deliberation) or `bro_when_any` (race). For provider-default suppression and minimal probe/team context, pull `sm-brofile-context` via `bbox_knowledge`."
            }
            Self::Roadmap => {
                "Operator-directed prospective work tracker: designed-but-not-implemented features, refactors, explorations, tech debt, and risks. Roadmap interactions are performed only at the express direction of the operator; never use the roadmap to defer, postpone, or avoid requested implementation work. Inbox is reactive; threads are active work; knowledge is atemporal. Use `action=\"next\"` to rank accepted items, and `action=\"promote\"` to spin a roadmap item into a work thread."
            }
            Self::StorageHealth => "Read-only storage inventory for edge sidecar hygiene.",
            Self::Workspace => {
                "Search indexed historical tool calls with bbox_tool_calls. Execute file, shell and Git operations in the caller harness."
            }
            Self::Operations => {
                "Day-2 operational health surfaces: aggregate daemon/corpus/route status with classified findings and suggested next commands."
            }
        }
    }
}

fn deferred_system_memory(category: ToolCategory) -> Option<&'static str> {
    match category {
        ToolCategory::Gaps => Some("sm-gap-notes"),
        ToolCategory::Packets => Some("sm-rule-packets"),
        ToolCategory::Orchestration => Some("sm-bro-dispatch-patterns"),
        ToolCategory::StorageHealth => Some("sm-storage-health"),
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
    ToolCategory::Notes,
    ToolCategory::Inbox,
    ToolCategory::Artifacts,
    ToolCategory::Packets,
    ToolCategory::Orchestration,
    ToolCategory::Roadmap,
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
        name: "bbox_corpus_search",
        category: ToolCategory::Transcripts,
        summary: "Compatibility corpus lookup for harness capability projection. Returns ranked hits with stable id/text fields.",
        when_to_use: "Normally called through the harness's flat `corpus_search` alias. Direct MCP callers should prefer `bbox_hybrid_search` for the richer typed-entity surface.",
        example: Some(r#"bbox_corpus_search(query="session boundary", limit=10)"#),
    },
    ToolDoc {
        name: "bbox_search",
        category: ToolCategory::Transcripts,
        summary: "Search across all indexed transcripts. Default `mode=smart` broadens adjacent terms for recall; `mode=fulltext` gives raw Tantivy/Lucene-style boolean syntax.",
        when_to_use: "Use when you know the topic but not the exact session. Filter by account, project, or role early. Pass `exclude_self=true` for current-turn searches. `source` filters the lane a document came from (`glm`, `claude`, `codex`, `gemini`, `slack`, ...): comma-separated for several, and a `-` prefix excludes one, so `source=\"slack\"` searches only ingested Slack conversations and `source=\"-slack\"` searches everything else. Slack conversations are searchable by default; that one filter is how you include or exclude them. For \"what's in a channel\" questions reach for `channel=` first: it accepts a channel name (leading `#` accepted) or channel id, resolves names through the current roster to the stable channel id so a renamed channel still matches its whole history, and also matches documents stamped with the queried name. Plain queries match channel names too, so a bare `query=\"ops-incident-4565\"` surfaces that channel's messages even when no message body names it. Authorship on a conversation hit is identity, not turn kind, so filter who spoke with `author=<provider user id>`; the `role` lane only distinguishes human from app there. Conversation hits render the channel, the author, and a derived Slack permalink; their `file_path` (a `slack:<workspace>/<channel>` locator) and `session_id` (a per-channel-per-day bucket) both drill down directly through `bbox_context` / `bbox_messages`, resolved against the conversation landing store rather than a transcript file; `channel=` and the permalink remain the other two working paths. Next-step hints are entity-aware: transcript and Slack hits receive only validated indexed reader coordinates, thread hits receive `bbox_thread(action=get,id=...)`, and other typed hits receive their canonical `bbox_inspect_entity` ref (or an explicit no-reader note). Selector string arguments are JSON encoded so quotes and backslashes survive copying. See `sm-transcript-retrieval` for ladders.",
        example: Some(
            r##"bbox_search(query="import mapping", channel="#ops-incident-4565", source="slack")"##,
        ),
    },
    ToolDoc {
        name: "bbox_hybrid_search",
        category: ToolCategory::Graph,
        summary: "Search typed entities with BM25 and vectors. Returns bounded evidence hits and retrieval status. Use debug for ranking diagnostics.",
        when_to_use: "Step 2 of the agentic opening sequence (`sm-agentic-opening-sequence`). Use as the default search for any topical question. Pass `project=$cwd` (or a registered project_id) when querying about your local repo to avoid cross-project keyword pollution. Trust topical hits: top seed is canonical for the query even when wording doesn't exactly match (vector lane catches paraphrases). The query language: adjacent terms broaden recall, quoted phrases stay exact, `-term` excludes. Model rerank (hosted cross-encoder, [embed.rerank], default rerank-2.5-lite) is the DEFAULT and degrades to the heuristic path on API failure (degraded.rerank_unavailable); pass rerank=\"heuristic\" to skip the cross-encoder call when latency matters more than precision, or rerank=\"none\" for raw fusion order. Project graph vertices participate like any typed entity: `project` also scopes them by their stamped project id; repeatable `graph_source` picks planes (`published`, `provisional`, `connector`; unset = all) and `graph_ids` names graphs within the resolved project, both applied before ranking so excluded vertices never consume rank positions. Vertex hits carry `graph_id`, `graph_source`, `graph_vertex_type`, `graph_generation`, and `graph_logical_ref`.",
        example: Some(
            r#"bbox_hybrid_search(query="triad implementation", limit=10, project="/home/me/repos/erlang-test")"#,
        ),
    },
    ToolDoc {
        name: "bbox_discover_seed_entities",
        category: ToolCategory::Graph,
        summary: "Find seeds with notable_edges; inspect before answering; graph vertices: graph_source/graph_ids.",
        when_to_use: "Alternate Step 2 of the agentic opening sequence (`sm-agentic-opening-sequence`): same blender as `bbox_hybrid_search` but with `notable_edges` rendered for each seed. Reach for it when the next step will be `bbox_inspect_entity` and you want pre-vetted hops. Project graph vertices seed under the same parameters: `project` scopes them by stamped project id, `graph_source` picks planes (`published`, `provisional`, `connector`; unset = all), `graph_ids` names graphs (both applied before ranking), and vertex hits carry the `graph_id`, `graph_source`, `graph_vertex_type`, `graph_generation`, and `graph_logical_ref` identity fields.",
        example: Some(
            r#"bbox_discover_seed_entities(query="triad closure convergence test", limit=5)"#,
        ),
    },
    ToolDoc {
        name: "bbox_cite",
        category: ToolCategory::Transcripts,
        summary: "Trace a claim back to the turn that established it.",
        when_to_use: "Use when you need provenance for a rule, preference, or standing claim. Returns citations oldest-first so the origin surfaces first. See `sm-transcript-retrieval` via `bbox_knowledge` for retrieval ladders.",
        example: Some(r#"bbox_cite(claim="never kill processes by port")"#),
    },
    ToolDoc {
        name: "bbox_context",
        category: ToolCategory::Transcripts,
        summary: "Read surrounding indexed events using a search hit's opaque locator and offset. Native replies disclose projection and freshness limits.",
        when_to_use: "Use a bbox_search hit's file_path and byte_offset unchanged. file_path is an opaque stored locator, never a file to open. Native context_lines counts indexed events before/after the target (default 5, max 25), with 400-byte previews; bbox_messages expands retained content. Indexed projections may already be parser-truncated and do not establish source completeness or freshness. Slack locators resolve through the conversation landing store using digit-encoded message timestamps.",
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
        when_to_use: "Provide exactly one of session_id or file_path from search. Native pages sort by locator then source byte offset; next_offset advances by the actual returned count under the byte cap. from_end pages from the tail. max_content_length limits preview bytes; zero returns up to 12000 retained bytes per native message, not original-source recovery. Native replies explicitly report projection-only completeness and unknown freshness. Slack file_path selects a channel; its channel/date session_id selects a day through the landing store.",
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
        when_to_use: "Use after changing embedding routes or provider dimensions to kick convergence immediately; a background residue sweeper otherwise drives every non-transcript route to full coverage on its own, including residue past the per-route queue cap, so a `stalled` route converges without repeated manual calls. E3 performs the rebuild. Routes include knowledge, code, docs, git_message, notes, threads, agent_manifest, graph (project-graph vertices whose schema opts them into embedding; rebuilt from the installed published views, and the one route that also tombstones vectors of vertices no longer embed-eligible), and guarded transcripts; `backfill` sweeps every route except transcripts (idempotent — already-embedded items dedupe at enqueue). Use max_entities for progressive refills. Transcript rebuilds require include_transcripts=true because they read the transcript corpus.",
        example: None,
    },
    ToolDoc {
        name: "bbox_embed_partitions",
        category: ToolCategory::Transcripts,
        summary: "Vector partition lifecycle: list partitions with route mapping, dims, dtype, compatibility family, active_count, last_write; prune orphaned partitions; scrub misattributed vectors from a mapped partition (dry-run default).",
        when_to_use: "Use action=\"list\" to see every vector partition and whether any configured bucket currently maps to it (orphans show mapped=false; hybrid search skips them under degraded.skipped_partitions). After a deliberate model/route migration, use action=\"prune\" with older_than_days=<N>: only partitions BOTH unmapped by current route config AND idle beyond that age are candidates, and nothing deletes without apply=true (dry-run default). bbox_reembed never prunes; reclaiming a vector space is a separate operator decision. After a bucket attribution change, action=\"scrub\" with route=<mapped partition id> classifies every vector against CURRENT attribution and (with apply=true) deletes rows whose entities now belong to a different route; index-missing entities and non-project_file rows are always kept.",
        example: Some("bbox_embed_partitions(action=\"prune\", older_than_days=30)"),
    },
    ToolDoc {
        name: "bbox_embed_status",
        category: ToolCategory::Transcripts,
        summary: "Read embedding health. Scan and probe opt-ins can be expensive.",
        when_to_use: "Use when vector search degrades. The default reports availability, health, queue depth, session_indexed_count (successes since daemon startup, not corpus size), nonzero capped_count (enqueues rejected at the queue cap - residue the sweeper will refill, not a drop), dropped_count (permanently un-embeddable poison), and sanitized error without walking the source corpus or HNSW. Zero retry/drop/cap counters and absent values are omitted. debug=true restores provider/model configuration and routine diagnostic fields; it does not enable expensive work. Pass include_coverage=true for exact per-route source/indexed counts and stalled-coverage classification; this walks every embedding-source document and can take minutes on a large corpus. Pass include_diagnostics=true with an optional bounded diagnostic_routes list for deadline-bounded connectivity diagnostics; unavailable is reported separately from healthy. Pass recall_probe_route for sampled self-recall; explicit probes can take seconds on large partitions and refuse busy routes.",
        example: Some(
            "bbox_embed_status(include_diagnostics=true, diagnostic_routes=[\"voyage-1024\"])",
        ),
    },
    ToolDoc {
        name: "bbox_topics",
        category: ToolCategory::Transcripts,
        summary: "Top terms in a session by frequency.",
        when_to_use: "Quick 'what was this session about' without LLM summarization.",
        example: None,
    },
    ToolDoc {
        name: "bbox_sessions_list",
        category: ToolCategory::Transcripts,
        summary: "Browse sessions sorted by recency.",
        when_to_use: "Use when you need to find a session by recency, project, or name without a concrete text query. See `sm-transcript-retrieval` via `bbox_knowledge` for retrieval ladders.",
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
        summary: "Inspect properties and targeted edges. Filter edge_types and direction; per_type_limit=0 reads properties only. property_mode selects summary, smart, or full. Follow edge_page.next_cursor for more edges; property retrieves exact text in pages.",
        when_to_use: "Use after search to verify a ref and inspect relevant relations. Select edge_types and direction (out, in, both). property_mode is summary, smart (default, 300-character text previews), or full; invalid values fail. Edges page at 100 maximum; follow edge_page.next_cursor as edge_cursor with the same selection. Read a property key from properties or property_projection.omitted_keys with property=<key>; body.next_cursor continues via property_cursor. property_limit is 4..4096 UTF-8 bytes, default 4096. Cursors reject changed selections or source revisions. Full/property reads recover stored provider values; *_preview fields do not expand upstream content. Commit content is the indexed message; evidence.content_completeness marks ingestion truncation. Schema-authored absent relations remain explicit; generic empty scaffolding is omitted. Evidence properties retain assertion authority, source generation, endpoint freshness, and unresolved states. No embedded rendered text mirror is returned.",
        example: Some(
            r#"bbox_inspect_entity(entity_ref="knowledge:abc12345", edge_types="SUPERSEDES,DERIVED_FROM", direction="both")"#,
        ),
    },
    ToolDoc {
        name: "bbox_project_graph_list",
        category: ToolCategory::ProjectGraphs,
        summary: "List visible project graphs. Pass provisional (published, own, or all); visibility is accepted as a deprecated alias. Each entry reports two count families: vertex_count/edge_count are the REFLECTED graph (authored rows plus schema-as-data vertex/edge type definitions plus meta:INSTANCE_OF edges), while authored_vertex_count/authored_edge_count count only rows sourced from vertices.jsonl/edges.jsonl. Compare authored_* against your source files, not vertex_count/edge_count. Each entry's source names its authority plane: published, provisional, or connector (a read-only connector-managed source projection).",
        when_to_use: "Discover graph ids. Use authored_vertex_count/authored_edge_count when checking a count against your jsonl source files; use vertex_count/edge_count when reasoning about the full materialized graph an agent will traverse. Connector-source graphs are listed under every visibility policy because they are read-only projections rather than checkout state, and they are never writable through a checkout lane.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_graph_describe",
        category: ToolCategory::ProjectGraphs,
        summary: "Describe one visible project graph. Pass provisional (published, own, or all); visibility is accepted as a deprecated alias. The summary carries both count families: vertex_count/edge_count are the REFLECTED graph (authored rows plus schema-as-data vertex/edge type definitions plus meta:INSTANCE_OF edges), while authored_vertex_count/authored_edge_count count only rows sourced from vertices.jsonl/edges.jsonl. The retrieval block reports word-index participation: policy flags, excluded vertex types, indexed vertex count, embedded vertex count, and the indexed generation versus the accepted generation, so a graph that is not showing up in search can be diagnosed without reading a schema artifact.",
        when_to_use: "Read schema, generation identity, and word-index participation. Use authored_vertex_count/authored_edge_count when checking a count against your jsonl source files; use vertex_count/edge_count when reasoning about the full materialized graph an agent will traverse. On a connector-source graph the descriptor also carries source_connector and projection_version, and the schema may carry per-property index/embed annotations. When a graph is missing from search results, compare the retrieval block's indexed generation against its accepted generation before touching the schema.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_graph_validate",
        category: ToolCategory::ProjectGraphs,
        summary: "Validate one visible project graph. Pass provisional (published, own, or all); visibility is accepted as a deprecated alias.",
        when_to_use: "Inspect kernel diagnostics. Reports the same three sources as list: published, provisional, and connector.",
        example: None,
    },
    ToolDoc {
        name: "bbox_describe_schema",
        category: ToolCategory::Graph,
        summary: "Orient to entity types, edge families, and traversal. include_agents=true or mode=\"full\" adds installed agents.",
        when_to_use: "Use once for graph vocabulary and traversal orientation. Default omits agent catalogs; include_agents=true or mode=full adds them. mode=agents is a deprecated full alias; unknown modes fail. dispatch_adapter remains on each agent row. No rendered text mirror or duplicate agent grouping is returned.",
        example: Some("bbox_describe_schema()"),
    },
    ToolDoc {
        name: "bbox_find_paths",
        category: ToolCategory::Graph,
        summary: "Find direction-preserving graph paths from one EntityRef to another ref or entity type. Use after bbox_inspect_entity when a claim depends on a multi-hop chain; filter edge_types aggressively, keep max_depth small (default 3, max 5), and reuse returned path IDs with bbox_bundle_evidence. max_fanout (default 16, range 1..=64) caps the edges enumerated out of any single vertex; a capped expansion is reported explicitly under truncated_expansions rather than silently returning a prefix. Graph selection precedes neighbor enumeration: a hop into a graph the caller may not read is dropped before the frontier, and nothing in the response implies an unreadable graph exists. edge_types accepts a comma-separated string (e.g. 'CALLS,CALLED_BY') OR a JSON array of strings. Both shapes are equivalent. A target is required: a call with neither to nor to_type is refused with error.bad_input rather than answered as an empty result. Over project graphs under own or all visibility, to_type='project_graph_vertex' also matches provisional overlay vertices, so the logical type is enough; pass to_type='provisional_project_graph_vertex' only to target the overlay form exactly. Tenant-owned evidence bindings are traversable like any other edge and cross between graphs and out to project files, knowledge entries, and other entities. Their steps carry an `evidence.*` metadata family naming the binding, who asserted it, the observation or mapping it came from, and each endpoint's status (current, stale, missing, unauthorized, unresolved); the rendered path shows the aggregate freshness in brackets. A stale or missing endpoint is still walked and labeled, because a stale chain is still what was asserted; an unauthorized endpoint is never crossed.",
        when_to_use: "Step 4 of the agentic opening sequence (`sm-agentic-opening-sequence`): only when the answer depends on a chain, not a single entity. Prefer narrow `edge_types`, set `to` or `to_type` when known, and pass returned path IDs to `bbox_bundle_evidence` before making a provenance-sensitive claim. Always name a target: `to` for an exact ref, `to_type` for an open-ended walk; a call with neither is refused, not silently empty. On project graphs, pass the logical `to_type=\"project_graph_vertex\"` under any visibility; the provisional overlay type name is only needed to target overlay vertices exclusively. When truncated_expansions is non-empty, say so: the walk covered only the first max_fanout edges of the named vertices, and a wider cap or narrower edge_types is needed for completeness. State edge directions as the path returned them; do not invert from memory, and expect backward hops labeled `in` alongside forward `out` hops. When a path crosses an evidence binding, report its freshness: do not present a `stale` or `missing` hop as a current fact, and pass the path ID to `bbox_bundle_evidence` so the binding's provenance travels with the claim.",
        example: Some(
            r#"bbox_find_paths(from="knowledge:abc12345", edge_types="SUPERSEDES", max_depth=3)"#,
        ),
    },
    ToolDoc {
        name: "bbox_bundle_evidence",
        category: ToolCategory::Graph,
        summary: "Bundle entity refs and cached path IDs with provenance and current evidence freshness. Default summary bodies are 600 characters; full and none are explicit. Exact property pages are available through bbox_inspect_entity. Stale paths are explicit.",
        when_to_use: "Step 5 of the agentic opening sequence (`sm-agentic-opening-sequence`) — close the loop before answering. Pass `path_ids` from `bbox_find_paths` directly; do not reconstruct path text from memory (the server holds the validated graph). Use `property_mode=\"summary\"` when bundling broad knowledge/tool refs or other long entities. This tool packages evidence only; it does not synthesize the answer for you. When a bundled binding reports a non-current freshness, say so in the answer; the bundle records what was asserted, not that it is still true.",
        example: Some(
            r#"bbox_bundle_evidence(question="Why was this replaced?", entity_refs=["knowledge:abc12345"], path_ids=["P1"], property_mode="summary")"#,
        ),
    },
    ToolDoc {
        name: "bbox_ref_size",
        category: ToolCategory::Graph,
        summary: "Measure the byte payload size of entity refs. file refs resolve through a validated current checkout attachment selected by exact project_dir, authoritative session checkout, or an unambiguous registered project; project_file and project_file_v2 refs resolve to full indexed chunk content without checkout access; other refs resolve through entity providers and measure serialized provider-properties JSON. Accepts up to 500 refs; successful refs are canonicalized and unresolved/omitted refs are reported under degraded.",
        when_to_use: "Use when planning context-budget-sensitive dispatches. Pass the exact entity refs a downstream actor would need to read; the response returns per-ref byte counts, total_bytes, canonicalized successful refs, and unresolved/omitted refs without estimating from prose.",
        example: Some(r#"bbox_ref_size(project_dir="/repo/worktree", refs=["file:src/lib.rs"])"#),
    },
    ToolDoc {
        name: "bbox_edge_compact",
        category: ToolCategory::Graph,
        summary: "Dry-run or apply legacy edge sidecar compaction for one project. Removes append-only derived edges from edges/<project_id>.jsonl while retaining explicit/provenance/malformed lines; apply defaults false and writes a backup before replacement. With apply=true, rebuild=true forces a sidecar-only in-memory EdgeIndex rebuild even when compaction is already complete.",
        when_to_use: "Use when legacy edge sidecars have grown from repeated full reindex replay. Call first with `apply=false` (default) for exactly one project_id, inspect removed/retained counts, then call with `apply=true` for that same project if the dry-run scope is acceptable. Leave `rebuild=false` while compacting multiple projects; after the last project, call with `apply=true,rebuild=true` once to reload graph state.",
        example: Some(r#"bbox_edge_compact(project_id="d723917f", apply=false)"#),
    },
    ToolDoc {
        name: "bbox_blame",
        category: ToolCategory::Graph,
        summary: "Walk back from a code line to the conversation that produced it. Two modes: 1. Anchor-matching: the line's git blame commit matches a bbox-tracked tool-call anchor, returning the full session/brofile/arc/trigger chain. 2. Git-only fallback: no bbox anchor matches, returning git blame author info only, marked as non-bbox. Use this when you want to understand WHY a line exists, not just WHO wrote it.",
        when_to_use: "Use for WHY-this-line-exists questions; check anchor-matched vs git-only.",
        example: None,
    },
    ToolDoc {
        name: "bbox_provenance_export",
        category: ToolCategory::Graph,
        summary: "Legacy overlap adapter that writes bbox provenance Git notes from blackboxd. Prefer bro provenance export for checkout-local application; retain this tool when one call must cover all registered projects.",
        when_to_use: "Use only when the legacy all-registered-project export is required. For one checkout, run `bro provenance export` from that checkout instead.",
        example: None,
    },
    ToolDoc {
        name: "bbox_provenance_export_plan",
        category: ToolCategory::Graph,
        summary: "Return one deterministic, generation-bound provenance-note page for this MCP session's authoritative checkout. Project selection comes only from session context; callers may pass only cursor and generation pagination controls. Used by bro provenance export so Git-note writes stay checkout-local.",
        when_to_use: "Used by `bro provenance export`. The project is fixed by MCP session context; callers may supply only the returned cursor and generation for later pages.",
        example: None,
    },
    ToolDoc {
        name: "bbox_provenance_import",
        category: ToolCategory::Graph,
        summary: "Read bbox provenance git notes and replay them into the local EdgeIndex sidecar.",
        when_to_use: "Use after fetching or cloning bbox git notes from another machine.",
        example: None,
    },
    // ── Projects ─────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_project_register",
        category: ToolCategory::Projects,
        summary: "Register a project directory and schedule background agentic-corpus indexing. The path must be an absolute directory path (file paths and missing paths are rejected). Re-registering the same canonical path is idempotent — returns the existing record without modifying registered_at. project_id is derived from canonicalized realpath and is per-machine; repo_id derives from first commit SHA with remote fallback. In catalog mode this is the find-or-create composite: the checkout attaches to the project owning its committed scope (or a new Published/LegacyLocal project is minted), config-declared aliases become pending nominations, and a scope disagreement returns the exact promotion or scope-migration handoff instead of a second project. Use bbox_project_list to inspect registered projects.",
        when_to_use: "Use before S2+ needs a repo root. Symlink aliases collapse to one `project_id`; git repos also get `repo_id`.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_init",
        category: ToolCategory::Projects,
        summary: "Initialize a project-local .bbox workspace. Creates `.bbox/config.toml`, `.bbox/mcp.json`, `.bbox/local/.gitignore` and default subdirectories, and records the durable repo_id for Git projects. Idempotent by default; force=true refreshes replaceable skeleton files but always merge-preserves identity-bearing config.toml.",
        when_to_use: "Run once for a new repo before local config edits or project-scoped MCP overlays. It is safe to run repeatedly; config declarations and the recorded repo_id are preserved.",
        example: Some(r#"bbox_project_init(path="/home/me/repos/blackbox", force=true)"#),
    },
    ToolDoc {
        name: "bbox_project_rename",
        category: ToolCategory::Projects,
        summary: "Local administrator operation; transport-owned catalog projects refuse with error.project_admin_locality_required because no remote relocation lane is implemented. A bridge failure after registry admission reports error.project_rename_partial with completed effects and old/new recovery coordinates. Rename a registered bbox project root while preserving its project_id and migrating project-scoped bbox state. Accepts project (project_id, registered canonical_path, or absolute path), new_path (absolute directory path), optional move_on_disk (default false), and optional dry_run. Updates project registry, knowledge, threads, notes, pins, packets, Slack channel bindings, live teams, whiteboards, pollers, and crons, then reindexes project files. In catalog mode rename is attachment relocation: the moved checkout must carry the same checkout-id marker and resolve the same scope, the ledger records the historical path, owner-store rows are never rewritten, and move_on_disk is refused (move first, then rename).",
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
        summary: "List registered project roots with their project_id, repo_id (null for non-git), canonical_path, registered_at, and is_git_repo flag. Idempotent read; safe to call repeatedly. project_ids are stable across daemon restarts. Use this before bbox_project_register to check whether a path is already registered.",
        when_to_use: "Use to inspect registered roots or confirm symlink aliases collapsed.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_unregister",
        category: ToolCategory::Projects,
        summary: "Unregister a project root from the bbox project registry. Accepts project (project_id, registered canonical_path, or absolute path). Removes the registry entry only; does NOT delete project-scoped state (knowledge, threads, notes, pins, packets, Slack bindings, teams, whiteboards, pollers, crons) keyed on the project_id, which is derived from the canonical realpath and is stable across unregister+re-register. By default refuses when refs still exist and returns the counts; pass force=true to orphan them, or bbox_project_rename to migrate first. dry_run=true previews counts without mutating the registry. In catalog mode unregister is detach: the attachment is marked detached with census deregistration scoped to its checkout and scope pair, every logical store keeps its rows, and catalog deletion is the offline project-catalog retire surface.",
        when_to_use: "Use to drop a stale or accidentally-registered project root without hand-editing projects.json. Prefer `dry_run=true` first to see what is still attached, then `bbox_project_rename` to migrate or `force=true` to accept orphaning.",
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
        summary: "Detach one attachment: the row is marked detached with a timestamp, every logical store, entity ref, and generation is left untouched, and the catalog keeps its data. Census and watcher deregistration is scoped to the detached attachment's checkout and scope pair only, so a monorepo checkout carrying sibling attachments for other projects keeps their census rows and watcher coverage. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use when a checkout is going away or should stop being a local source for the project. Detach keeps every stored row: it is not deletion, and re-attaching later restores the local source.",
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
        summary: "Rebind the accepted-publication pointer of a published project to another of its attachments. The pointer's ref, accepted commit, accepted scope, generation, and payload bytes are unchanged: only the attachment binding moves, so the strict pointer and generation agreement holds identically before and after. The new attachment's object database must already contain the pointer's accepted commit, and a project with no pointer refuses rather than inventing one. Requires expected_catalog_epoch and a bounded audit_reason. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use after detaching or replacing the checkout that carried the publisher binding, so a later publication advance has a live attachment. Fetch the accepted commit into the new checkout first.",
        example: None,
    },
    ToolDoc {
        name: "bbox_project_publisher_advance",
        category: ToolCategory::ProjectCatalog,
        summary: "Establish or advance one published project's accepted publication. mode=establish creates the first pointer; mode=advance requires the generation and pointer tokens from bbox_project_publisher_status. Select exactly one source: attachment_id with full_ref reads a capable attached checkout, while source_generation_id consumes a Ready remote publication candidate and derives its producer, scope, ref, commit, and both source lanes from pinned immutable evidence. Candidate mode refuses caller-supplied full_ref. Both paths validate knowledge and gaps into one immutable generation and swap only after rechecking catalog authority and source freshness. Publishing uses the catalog's current scope, which clears a scope-migration bridge. dry_run validates and writes nothing. Requires expected_catalog_epoch and a bounded audit_reason. auto_advance is operator authority over this project's standing auto-advance grant: omit it to leave the grant unchanged, pass true to install it on the pointer this call writes, or false to revoke it. A granted project accepts later Ready candidates from the same bound producer, catalog scope, and published ref through this same validation and compare-and-swap discipline, with audit_reason policy:auto_advance; establish, rollback, scope changes, and every other non-linear move stay manual. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use to start publishing for a project left with no pointer (`establish`), or to advance published knowledge and gaps from either a local attachment or a Ready remote candidate. Read `bbox_project_publisher_status` first for compare-and-swap tokens, and run `dry_run=true` before a first advance.",
        example: Some(
            r#"bbox_project_publisher_advance(project_id="p_4f6a1c9e5b2d47a8b0c3e1f5a9d76b24", attachment_id="att_1b7d3f", mode="advance", full_ref="refs/heads/main", expected_generation_id="9f2c...", expected_pointer_sha256="41ab...", expected_catalog_epoch=7, audit_reason="publish reviewed knowledge")"#,
        ),
    },
    ToolDoc {
        name: "bbox_project_publisher_status",
        category: ToolCategory::ProjectCatalog,
        summary: "Read one catalog project's accepted-publication status: state, scope/ref/commit identity, typed source binding, advance availability, and the generation_id plus pointer_sha256 compare-and-swap tokens. Default health and connector sections are compact bounded summaries that keep stale, unavailable, queued, and partial signals visible with total, status, and omission counts; recorded rows are observations, not live filesystem authority. Oversized summary strings become explicit size-and-truncation markers (diagnostics keep a bounded prefix) whose exact bytes live only in detail pages. detail=health returns the complete runtime view and detail=connector returns the complete connector view as exact bounded body pages; replay detail.body.next_cursor while the body is unchanged. Connector detail requires a connector-scoped project. Observational, path-free, and takes no checkout lease; see design/daemon-runtime/publisher-auto-advance.md for deep mechanics. Returns error.project_catalog_inactive while the version-1 registry is the runtime authority.",
        when_to_use: "Use before an advance to read compare-and-swap tokens, or to diagnose unavailable, prior-fallback, detached, or non-advancing published knowledge. Use detail=health or detail=connector for exact diagnostics; a changed body refuses continuation.",
        example: None,
    },
    // ── Knowledge ────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_learn",
        category: ToolCategory::Knowledge,
        summary: "Persist an operator-approved rule or convention that should bind future sessions; rendered into provider markdown files. Use for narrative rules (\"we always X\", \"never Y\") only after the operator has approved the exact content and scope. If the rule you're storing is actually a priority-ordered decision function, classification rubric, or structured mechanism, use `bbox_compile` instead; that produces a shareable packet any agent can apply deterministically.",
        when_to_use: "Use only after the operator has approved the exact text and scope for a standing user rule that must outlive the current edit AND would still be correct a year from now with all current arcs complete. Anti-trigger: content naming a specific migration, phase, active arc, current initiative, or \"finish X before Y\" sequencing is arc-bound; route to `bbox_pin`. Not for one-off task constraints, not for facts you discovered yourself (that's `bbox_note(kind=\"learned\")`). Query `bbox_knowledge` first to avoid duplicate entries. On a transport-governed estate, project-scoped writes ride the checkout-owner backchannel: the daemon enqueues the committed `.bbox/knowledge/` bytes and the collector applies them within one cycle; commit the file to publish. See `sm-persistence-taxonomy` via `bbox_knowledge` for the deeper split.",
        example: Some(
            r#"bbox_learn(content="use rustls, not openssl", category="convention", scope="project", project="/repo/x")"#,
        ),
    },
    ToolDoc {
        name: "bbox_remember",
        category: ToolCategory::Knowledge,
        summary: "Persist a fact for later recall; indexed but NOT rendered.",
        when_to_use: "Observations, decisions, and context worth grepping for later but not worth every session loading. Use when you want persistence without prompt residency. Safer default than `learn` when unsure; use `bbox_pin` instead when the context must stay hot for one active execution lane.",
        example: Some(
            r#"bbox_remember(content="port 7263 conflicts with helper-daemon on host bravo", title="port clash")"#,
        ),
    },
    ToolDoc {
        name: "bbox_decide",
        category: ToolCategory::Knowledge,
        summary: "Record a durable commitment with required rationale; supports supersession.",
        when_to_use: "Use for real commitments or reversals that need rationale and audit trail. Query `bbox_knowledge` first to find the prior decision you may be superseding; `supersedes` takes the bare 8-hex entry ID. See `sm-persistence-taxonomy` via `bbox_knowledge` for the deeper split.",
        example: Some(
            r#"bbox_decide(content="use RocksDB for cache", rationale="SQLite locking conflicted with concurrent writers", supersedes="8a3f12cd")"#,
        ),
    },
    ToolDoc {
        name: "bbox_pin",
        category: ToolCategory::Knowledge,
        summary: "Persist scoped ambient context for an active execution lane. Pins survive daemon restarts, are never rendered into repo agent files, and are injected only when the current dispatch matches their session/bro/thread/work-item scope.",
        when_to_use: "Use for active-arc guidance that should stay hot for one execution lane without becoming standing repo policy: migration phase notes, bounded executor charters, current-initiative sequencing, or temporary reviewer context. Prefer `bbox_pin` over `bbox_learn` when the guidance is supposed to disappear with the session/arc rather than bind future unrelated agents. Self-inspection: `bbox_pin(action=\"list\")` with scope/target/project filters returns your active anchors — pins are not surfaced via `bbox_knowledge`, so `list` is the only read path. See `sm-scoped-pins` via `bbox_knowledge` for the deeper split.",
        example: Some(
            r#"bbox_pin(action="set", scope="bro", target="executor", project="/repo/x", title="Active arc", content="For the current migration, validate every phase cut against the canonical scoping doc before proposing code changes.")"#,
        ),
    },
    ToolDoc {
        name: "bbox_knowledge",
        category: ToolCategory::Knowledge,
        summary: "Query durable knowledge entries by free-text or filters. Use early when prior decisions, conventions, remembered facts, or system runbooks could change the answer. Also surfaces matching rule-packets and system memories; system memories include system_memory:<id> refs usable with bbox_inspect_entity or bbox_bundle_evidence. Pass category=\"packet\" to list compiled packets, category=\"system_memory\" to list memory metadata, or bbox_packet_list for structured packet filters.",
        when_to_use: "Use near the start of tasks where durable knowledge-store context could matter: prior decisions, project conventions, rendered rules, remembered facts, or system runbooks. This is not the surface for scoped pins (`bbox_pin`), side-channel notes (`bbox_notes`/`bbox_inbox`), active threads (`bbox_thread_list`), or transcript history (`bbox_search`). Prefer a short phrase from the user's request over a single generic keyword; adjacent terms broaden recall, quoted phrases stay exact, `AND` / `OR` work explicitly, and `-term` excludes. If the first query is empty or too broad, try one sharper phrase. Use `mode=substring` for literal whole-query matching. Add `project=<cwd>` when looking for a prior decision to supersede; `project` also accepts a project_id or a registered operator alias and matches entries by project identity, and a value that resolves to no registered project keeps literal substring matching and says so in the response diagnostics. System memories can also be fetched by canonical `sm-*` ID. Rule-packets appear in a separate section when the query hits their id / domain / rule ids / classifications — reach for bbox_packet_list when you want structured filters (scope, latest_per_domain) or richer per-packet previews.",
        example: Some(r#"bbox_knowledge(query="retry policy")"#),
    },
    ToolDoc {
        name: "bbox_knowledge_link",
        category: ToolCategory::Knowledge,
        summary: "Append a knowledge edge.",
        when_to_use: "Pass project to select the source entry's checkout-owner project when IDs overlap or unrelated publications are unavailable. The selected project must contain the source entry; there is no fallback to another owner. Omit project for global or local-store entries. Unscoped mutations refuse when a unique owner cannot be established.",
        example: None,
    },
    ToolDoc {
        name: "bbox_forget",
        category: ToolCategory::Knowledge,
        summary: "Retire or supersede an entry.",
        when_to_use: "Entry is stale or replaced. Prefer `bbox_decide` with `supersedes` if the replacement is itself a decision. Pass project to select a checkout-owner project when IDs overlap or unrelated publications are unavailable; the selected project must contain the entry, with no fallback to another owner. Omit project for global or local-store entries. Unscoped mutations refuse when a unique owner cannot be established.",
        example: None,
    },
    ToolDoc {
        name: "bbox_render",
        category: ToolCategory::Knowledge,
        summary: "Render entries into CLAUDE.md / AGENTS.md / GEMINI.md.",
        when_to_use: "Use to publish standing approved knowledge into managed files. `global` patches the DAEMON HOST's memory files; when the daemon is remote (cage) or its store is isolated it refuses with `error.global_render_authority` instead of writing files nobody reads. To refresh an operator host's global files from a remote daemon, run `bro render global` ON THAT HOST (`--check` previews): it requests `bbox_render(scope=\"global\", global_plan={host_common_target})` and applies the returned managed bodies locally with backups. `project` writes project-local provider files that include PROJECT.md by reference. Do not use render as a way to keep active-work guidance hot across turns — that is what `bbox_pin` is for. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
        example: Some(r#"bbox_render(scope="project", project="/repo/x")"#),
    },
    ToolDoc {
        name: "bbox_absorb",
        category: ToolCategory::Knowledge,
        summary: "Compatibility no-op for the old rendered-file import path.",
        when_to_use: "Rendered provider files are unidirectional projections now. Use indexed instruction refs or the checkout owner to inspect hand-authored content; knowledge imports use the explicit knowledge write tools. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
        example: None,
    },
    ToolDoc {
        name: "bbox_lint",
        category: ToolCategory::Knowledge,
        summary: "Health check for contradictions, stale entries, duplicates.",
        when_to_use: "Use for periodic hygiene, before large knowledge-store refactors, or when the render/review state looks inconsistent. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
        example: None,
    },
    ToolDoc {
        name: "bbox_review",
        category: ToolCategory::Knowledge,
        summary: "Approve or reject entries awaiting review.",
        when_to_use: "Use to approve or reject existing unverified entries. Review controls render eligibility; it does not import rendered-file edits. For approve/reject, pass project to select a checkout-owner project when IDs overlap or unrelated publications are unavailable; the selected project must contain the entry, with no fallback to another owner. Omit project for list, global entries, or local-store entries. Unscoped mutations refuse when a unique owner cannot be established. See `sm-render-lifecycle` via `bbox_knowledge` for the full lifecycle.",
        example: None,
    },
    ToolDoc {
        name: "bbox_bootstrap",
        category: ToolCategory::Knowledge,
        summary: "Retired compatibility operation. Use bbox_hybrid_search for indexed instruction-file discovery and bbox_inspect_entity to expand refs; this operation does not import knowledge or read caller files.",
        when_to_use: "Retained only as a migration refusal. It performs no import or onboarding. Use bbox_hybrid_search(project=..., doc_type=project_file) to discover indexed instructions and bbox_inspect_entity to expand exact refs. Missing indexed files require producer enrollment or checkout-owner inspection; daemon paths cannot substitute for either.",
        example: None,
    },
    // ── Threads ──────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_thread",
        category: ToolCategory::Threads,
        summary: "Open / continue / resolve / promote / rename / link a work thread.",
        when_to_use: "Use for investigations or QC walks that span sessions. Before `action=open`, call `bbox_thread_list` to avoid duplicate threads. Use `kind=work_item` for orchestrator-led execution loops. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene.",
        example: Some(
            r#"bbox_thread(action="open", topic="audit the dispatch path", project="/repo/x", kind="work_item")"#,
        ),
    },
    ToolDoc {
        name: "bbox_thread_list",
        category: ToolCategory::Threads,
        summary: "List thread summary pages (default 20, maximum 100), ordered by last activity then id. Continue with next_offset; use bbox_thread(action=get,id=...) for full context.",
        when_to_use: "Before starting work on a topic (continuity check). Use `status` for lifecycle (`open`, `active`, `resolved`, `promoted`) and `min_idle_days` to return only threads idle for at least N days. Filter by `kind=work_item`. Workflow-origin arc threads are hidden by default; pass `include_workflows=true` when you need historical workflow records.",
        example: None,
    },
    // ── Notes ────────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_note",
        category: ToolCategory::Notes,
        summary: "Record a structured side-channel note while working.",
        when_to_use: "Emit a note only when you have a *notable* signal worth surfacing to an orchestrator — a `dispute`, `surprise`, `blocked`, `learned` fact, or actionable `followup`. Silence is the correct default: this is a side channel, not a per-call progress log, so most dispatches should emit nothing. A `kind=done` sign-off is opt-in — emit it when an explicit caller contract asks for one, not on every dispatch. Use `learned` for agent-discovered facts, not user-stated rules. See `sm-side-channel-notes` via `bbox_knowledge` for the full note taxonomy. Substrate gaps that other projects could plausibly hit too are NOT side-channel notes — file them with `bbox_gap` (see `sm-gap-notes` via `bbox_knowledge`), not here.",
        example: Some(
            r#"bbox_note(kind="dispute", body="brief assumes schema is additive — migration 0042 makes it subtractive")"#,
        ),
    },
    ToolDoc {
        name: "bbox_notes",
        category: ToolCategory::Notes,
        summary: "List note summary pages (default 20, maximum 100), newest first then id. Continue with next_offset; use id and full=true for complete bodies. Filter by kind, project, session, thread, or resolution.",
        when_to_use: "Orchestrators reading what executors emitted this round, auditing past dispatch for a work-item thread, or retrieving a known note by `id=\"note-<8hex>\"`. The `query` filter searches note bodies, not IDs. Bodies are previewed at 200 chars by default; pass `full=true` to render complete bodies (useful for `done` summaries and structured `dispute` rationales). Addressed notes are hidden by default for list views but included by default for exact `id` lookups.",
        example: Some(r#"bbox_notes(id="note-a1b2c3d4", full=true)"#),
    },
    ToolDoc {
        name: "bbox_note_resolve",
        category: ToolCategory::Notes,
        summary: "Mark one note, or a batch of notes, acknowledged or addressed.",
        when_to_use: "Orchestrator close-the-loop move. Pass the full `note-<8hex>` ID verbatim as `id` for one note, pass `ids=[...]` to close multiple notes with a shared `note`, or pass `notes={\"note-<8hex>\":\"detail\"}` for per-note resolution details. The batch forms use one mutation and one durable persist. `addressed` removes notes from the default inbox view; `acknowledged` keeps them visible as deferred. See `sm-side-channel-notes` via `bbox_knowledge` for the full loop.",
        example: Some(
            r#"bbox_note_resolve(notes={"note-a1b2c3d4":"fixed parser","note-deadbeef":"deferred to thread-12345678"}, resolution="addressed")"#,
        ),
    },
    // ── Gap notes ────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_gap",
        category: ToolCategory::Gaps,
        summary: "File a first-class substrate gap note into the repo-owned gap store.",
        when_to_use: "When the blocker is in the blackbox substrate or shared agent workflow — a missing tool primitive, MCP surface, refactor atom, workflow shape, ontology edge, or runbook that agents in other projects could plausibly hit too — NOT an ordinary TODO in the current product codebase and NOT a user-stated rule (those go to bbox_learn / bbox_decide). Dedupe first with `bbox_gaps` and reuse the same `dedupe_key` (`<gap_kind>/<domain>/<slug>`); an open gap with that key dedupes by default (pass `allow_recurrence=true` to tally a recurrence). Project-scoped by default (committed in-repo under `.bbox/gaps/`); pass `scope=\"global\"` for cross-project substrate gaps. On a transport-governed estate the daemon holds no checkout authority: the call still works, but the daemon enqueues the committed-file bytes and the checkout-owner collector writes them into the checkout within one collector cycle (the response says where; commit the file to publish). While authoring a rule-packet, use `bbox_packet_gap` instead (it emits the companion gap for you). See `sm-gap-notes` via `bbox_knowledge`.",
        example: Some(
            r#"bbox_gap(title="Packet AST cannot express rate predicates", gap_kind="packet_ast", domain="review-policy", wanted_capability="Classify entities by count/rate within a time window.", dedupe_key="packet_ast/review-policy/rate-window-predicate", impact="medium")"#,
        ),
    },
    ToolDoc {
        name: "bbox_gaps",
        category: ToolCategory::Gaps,
        summary: "List paginated gap summaries with typed filters. Exact id defaults to full detail; detail=full expands a page. Omissions and continuation are explicit.",
        when_to_use: "The mandatory dedupe step before `bbox_gap`: search open gaps by `dedupe_key`, `gap_kind`, `domain`, `impact`, or free-text `query` before filing. Also the triage surface — pass `json=true` for machine-readable records to group/extract, or `include_addressed=true` to see closed gaps. Addressed gaps are hidden by default for lists, shown by default for an exact `id`. `project` accepts a project_id, a registered operator alias, or a project path, and matches rows by project identity; a value that resolves to no registered project keeps literal substring matching and says so in `diagnostics`, so an empty list is never silent about an unresolvable filter. Source visibility warnings are summarized by default; debug=true returns up to 10 bounded diagnostic previews. Narrow project for scope-specific diagnostics.",
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
    // ── Inbox ────────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_inbox",
        category: ToolCategory::Inbox,
        summary: "Aggregate attention layer across every store.",
        when_to_use: "Round boundaries, morning brief, any 'what needs my attention' moment. Surfaces unresolved disputes/blocked/surprises, deferred followups, stale threads, unverified knowledge, failed bro tasks. Single call, prioritized view. Open gaps appear here too. This is a read-only preview: default 10, maximum 20 rows per section. aggregate_gaps=true adds bounded group counts. Expand with the dedicated list/search tools. Gap-spool import and Git closeout checks are retired from this tool; use bbox_gap for filing and the owning harness for repository checks. See `sm-gap-notes` via `bbox_knowledge`.",
        example: Some(r#"bbox_inbox(project="/repo/x", stale_days=3)"#),
    },
    // ── Artifact catalog ─────────────────────────────────────────────
    ToolDoc {
        name: "bbox_artifact_install",
        category: ToolCategory::Artifacts,
        summary: "Install a packet, brofile, simple agent or team from an inline artifact object or explicit HTTP(S) URL. Supply exactly one; caller filesystem paths are rejected. Workflow, atom and cron installation is retired.",
        when_to_use: "List before installing. The installer validates packet, brofile, simple-agent or team JSON and records its version. Teams require installed member brofiles; reinstalling preserves live sessions. Automatic advisors are retired: dispatch reviewers explicitly. Caller paths are never read by this tool.",
        example: Some(
            r#"bbox_artifact_install(kind="brofile", artifact={"name":"reviewer","provider":"brodex","lens":"Review correctness and explain material findings."})"#,
        ),
    },
    ToolDoc {
        name: "bbox_artifact_list",
        category: ToolCategory::Artifacts,
        summary: "List installed artifact summaries (default 20, maximum 100); continue with next_offset. Retired kinds are omitted unless explicitly selected with kind. Historical receipts are marked retired and inactive. detail=true adds installation and supersession metadata.",
        when_to_use: "Inventory check before installing or superseding producer machinery. Use kind/name filters to inspect a specific artifact family.",
        example: Some(r#"bbox_artifact_list(kind="packet")"#),
    },
    ToolDoc {
        name: "bbox_artifact_supersede",
        category: ToolCategory::Artifacts,
        summary: "Mark one installed artifact superseded by another artifact of the same kind.",
        when_to_use: "Use when a customized packet/brofile/agent replaces an installed version but you want the old version retained for audit.",
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
    // ── Rule-packets ─────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_compile",
        category: ToolCategory::Packets,
        summary: "Compile a rubric / judge / decision-function into a shareable packet. Reach here when you're writing a priority-ordered rubric, ranking proposals against shared criteria, compressing an access table, coordinating sub-agents against identical standards, or classifying future cases the same way you classified past ones. Symptom: you're about to paste the same rubric text into multiple sub-agent prompts — compile once and dispatch the packet_id instead. Rules are first-match-wins over a predicate AST; validate with bbox_audit before trusting. Packets compose via `Apply{packet_id, expect}` — extract `is_breaking` / `privileged_role` / etc. once, reuse across packets. Full workflow: sm-rule-packets via bbox_knowledge.",
        when_to_use: "Symptoms that mean \"compile a packet\": (1) you're coordinating multiple sub-agents and pasting the same rubric text into each prompt — compile once, dispatch `packet_id` instead, guarantees bit-identical standards; (2) you're ranking a batch of proposals/PRs/incidents against shared criteria; (3) you've got 10+ labeled examples and need a mechanism that generalizes to the 100+ unlabeled ones; (4) you're about to write Python/prose to implement a decision tree. First-match-wins so put anomalies before general rules. Always follow with `bbox_audit` to verify fidelity.",
        example: Some(
            r#"bbox_compile(domain="pr-triage", classification_lattice=["fail","flag","manual","pass","info"], rules=[{"id":"fail_tests","classification":"fail","antecedent":{"op":"Eq","field":"tests_pass","value":false},"consequent":"REJECT"},{"id":"flag_api_change","classification":"flag","antecedent":{"op":"Eq","field":"api_surface_changed","value":true},"consequent":"FLAG"},{"id":"pass_default","classification":"pass","emit":"fallback","antecedent":{"op":"True"},"consequent":"ACCEPT"}])"#,
        ),
    },
    ToolDoc {
        name: "bbox_apply",
        category: ToolCategory::Packets,
        summary: "Evaluate a packet against one entity — deterministic, no LLM. The receive-side of the packet workflow: a sub-agent that received packet_id from its orchestrator calls this to classify without reinterpreting the rubric. mode=\"first\" returns the first matching rule; mode=\"all\" returns every matching rule plus an aggregate verdict (for review / multi-finding shape). Cheap at arbitrary scale.",
        when_to_use: "The receive-side of the packet workflow. Use from a sub-agent that received `packet_id` from its orchestrator — no need to re-read or re-interpret the rubric, just evaluate. Also use yourself after compiling to spot-check on specific entities. If no rule matches, returns `{match: false}` rather than guessing — so missing catchalls surface immediately.",
        example: Some(
            r#"bbox_apply(packet_id="packet-a1b2c3d4", entity={"tests_pass":true,"api_surface_changed":true,"migration_note_present":false}, mode="all")"#,
        ),
    },
    ToolDoc {
        name: "bbox_audit",
        category: ToolCategory::Packets,
        summary: "Run a packet against a {entity, expected}[] dataset; report fidelity + mismatching rule ids. The self-verify step: a packet with fidelity < 1.0 is lying about its training data. ALWAYS call this after bbox_compile against the observations you derived the rules from — catches over-generalization, rule-ordering bugs, and field-name typos.",
        when_to_use: "ALWAYS run this after `bbox_compile` against the observations you derived the rules from. Catches (a) rules that mis-generalized beyond the anomalies, (b) ordering bugs where a general rule shadows an anomaly, (c) typos in field names. Use `mode=\"all\"` when the packet is for multi-finding review and expected outputs are rule-id sets.",
        example: Some(
            r#"bbox_audit(packet_id="packet-a1b2c3d4", dataset=[{"entity":{"tests_pass":false,...}, "expected":"REJECT"}, ...])"#,
        ),
    },
    ToolDoc {
        name: "bbox_packet_list",
        category: ToolCategory::Packets,
        summary: "Discover compiled packet summary pages (default 20, maximum 100). Filter before paging and continue with next_offset. detail=true adds histograms and rule previews. Read complete rules with bbox_inspect_entity(entity_ref=packet:<id>, property=body).",
        when_to_use: "Run BEFORE `bbox_compile` on any new domain. Query by concept (\"breaking\", \"pii\", \"deny\") when you don't know the exact domain label. If a match exists: reuse via `bbox_apply` or compose via `Apply{packet_id, expect}` inside your new packet — don't re-derive. Pair with `bbox_packet_events(packet_id=...)` to check the packet's track record (fidelity, no_match rate) before depending on it. Pages order by created_at descending then id; latest_per_domain=true selects the newest revision per domain before paging. For faithful policy edits, inspect packet:<id> with property=body and join body.text pages using property_cursor=body.next_cursor. The body is complete installed JSON, including rules, lookup tables, classification configuration and provenance. Pass authoring fields to bbox_compile to create a new revision; do not send storage metadata as authoring input.",
        example: Some(r#"bbox_packet_list(query="breaking", latest_per_domain=true, limit=10)"#),
    },
    ToolDoc {
        name: "bbox_packet_events",
        category: ToolCategory::Packets,
        summary: "Query the packet operation log — every compile / apply / audit / gap event the daemon has recorded, plus `repair_candidate` events emitted by the self-heal scanner when enabled. Use to investigate packet behavior over time: low-fidelity audits, high no_match rates, compile failures, authoring gaps, and packets the scanner has flagged for repair. Filter by op (compile / apply / audit / gap / repair_candidate), packet_id, outcome, or since. Returns newest-first up to `limit` (default 50, max 500).",
        when_to_use: "Diagnostic surface for the packet subsystem. Use when a packet is behaving unexpectedly, when you want to see which domains have the highest compile error rate, or when aggregating authoring gaps to prioritize new AST primitives.",
        example: Some(r#"bbox_packet_events(op="gap", limit=20)"#),
    },
    ToolDoc {
        name: "bbox_packet_gap",
        category: ToolCategory::Packets,
        summary: "Log a packet-authoring gap: 'I wanted to compile a rule but the AST couldn't express it'. Use when you fall back to prose, ad-hoc code, or a different tool because a primitive you needed isn't available. The `description` names what you wanted; `ast_feature_requested` names the primitive you wished existed (e.g. `RateCmp`, `StringMatches`, `Within{temporal}`). These gaps are the highest-signal input for prioritizing new AST primitives — every gap logged is a vote for what the packet system can't yet say. Query via bbox_packet_events(op='gap').",
        when_to_use: "Reach here when you've tried to compile a packet but the AST can't express part of what you need. Don't silently fall back to prose — logging the gap turns the blocker into a vote for a new primitive. Equally valid for partial-compile cases: compile the mechanizable part, log a gap for the rest.",
        example: Some(
            r#"bbox_packet_gap(description="wanted regex matching on log messages; no StringContains-like primitive", ast_feature_requested="StringMatches")"#,
        ),
    },
    ToolDoc {
        name: "bbox_mcp_surface",
        category: ToolCategory::Packets,
        summary: "MCP surface debugging, listing, and inspection. Actions: 'replay' evaluates a surface selector against the routing packet; 'list' shows installed surface packets; 'describe' shows packet rules plus verdict for a selected surface.",
        when_to_use: "Reach here when authoring or debugging mcp-surface/routing packets. 'replay' evaluates a surface selector and shows verdict + visible tools. 'list' enumerates installed surface packets. 'describe' shows packet rules and the verdict for a selected surface.",
        example: Some(
            r#"bbox_mcp_surface(action="replay", surface="readonly", project="/home/user/repo")"#,
        ),
    },
    // ── Orchestration (bro) ──────────────────────────────────────────
    ToolDoc {
        name: "bro_exec",
        category: ToolCategory::Orchestration,
        summary: "Launch a fresh agent task/session and return {taskId, sessionId}. Optional request_key prevents duplicate launch after a lost reply. Required selector: provide either `bro`, `provider`, or runtime allocation fields such as `tier`, `pool_name`, `pin_provider`, `pin_model`, or `capabilities`.",
        when_to_use: "Use to start a fresh agent session only. Supply request_key before dispatch when an uncertain reply may need retry: repeat the same key and inputs in the same bound workspace. Keys never expire automatically. Changed inputs refuse reuse. admission_incomplete means execution_unknown: inspect the returned taskId; the same key never relaunches. Without a key, another call can start another task. A dispatch selector is required: pass exactly one selector family — `bro` for a named bro, `provider` for a raw ad-hoc provider, or allocator fields (`tier`, `tier_ladder`, `tier_mode`, `min_tier`, `max_tier`, `pool_name`, `pool_providers`, `capabilities`, `selection_policy`, `pin_provider`, `pin_account`, `pin_model`, `pin_effort`, `prefer_provider`) for pool-backed runtime allocation. Set the session's working directory with `cwd` (canonical name; `project_dir` is accepted as a deprecated alias). Fresh-session overrides such as `service_tier` apply after selector resolution. Prefer `bro:` over raw `provider:` so routing stays stable when a named bro exists. Record `taskId`, `sessionId`, and any `selectionTraceId`; inspect allocation decisions with `bro_allocator_trace`. Without an account pin, allocation uses only that provider's declared default or native credentials; unrelated global accounts are not candidates. For any follow-up on that same work, use `bro_resume`; another `bro_exec` starts fresh and has no continuity.",
        example: Some(
            r#"bro_exec(prompt="review this patch", cwd="/repo/x", tier="standard", pool_name="coding", durable=true)"#,
        ),
    },
    ToolDoc {
        name: "bro_resume",
        category: ToolCategory::Orchestration,
        summary: "Continue an existing session with a follow-up; single-flight per provider session. Optional request_key prevents duplicate continuation after a lost reply.",
        when_to_use: "Use for follow-ups on an existing bro session. request_key has the same durable retry contract as bro_exec; use a new key for each intentional turn. Workflow/atom-owned sessions refuse ordinary resume. Do not use `bro_exec` again when you need continuity. Pass explicit `session_id` / `provider` when possible; named bro targeting is only safe when the session is unambiguous. The working-directory override is `cwd` (canonical; `project_dir` accepted as a deprecated alias) — usually unnecessary because resume auto-resolves the session's recorded cwd. For Brodex, pass `service_tier=\"priority\"` to force fast routing for the continuation or `service_tier=\"default\"` to persist standard routing. `pin_model` / `pin_effort` override the model and reasoning effort for the resumed turns (absent ⇒ brofile/session default). Never call `bro_resume` on a session while its previous task is still running: first `bro_wait(task_id=...)`, or `bro_cancel(task_id=...)` if you are abandoning that turn. If a prior turn failed but the session is still useful, resume it with recovery context before starting a fresh `bro_exec`. See `sm-bro-dispatch-patterns` via `bbox_knowledge` for workflow shapes.",
        example: Some(
            r#"bro_resume(bro="executor", prompt="add tests for the edge case we discussed")"#,
        ),
    },
    ToolDoc {
        name: "bro_allocator_status",
        category: ToolCategory::Orchestration,
        summary: "Read pool-backed runtime allocation config plus bounded in-flight, probe, lease, and preview-candidate pages.",
        when_to_use: "Use when debugging or auditing late-bound bro dispatch: inspect effective tier mappings, pools, selection policies, and current runtime state. in_flight, probes, leases, and preview.candidates are paged (default 20, maximum 100 rows); continue each section from its next_offset. Probe rows are compact lane status; read one exact lane record with bro_allocator_probe body paging. Pass tier/pool/capability/pin fields to preview the candidate table without spawning a task or writing a lease.",
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
        when_to_use: "Use to record credential, quota, cooldown, and probe-confidence observations consumed by allocator scoring and bro_allocator_status previews. This mutates allocator/probes.json; write failures return error.probe_persistence_failed and leave the stored record unchanged, so success means persisted. Read-only inspection returns a compact probe status plus the exact redacted record paged with body.next_cursor. Use bro_allocator_status for multi-lane overviews.",
        example: Some(
            r#"bro_allocator_probe(provider="codex", quota_status="exhausted", quota_confidence="runtime_rate_limit", cooldown_ms=300000)"#,
        ),
    },
    ToolDoc {
        name: "bro_wait",
        category: ToolCategory::Orchestration,
        summary: "Observe one task until completion; never launches follow-up work. Timeout returns a snapshot, not proof the task is dead. If the result is empty or suspicious, inspect bro_status(tail=N) before resuming, cancelling, or treating it as success.",
        when_to_use: "After `bro_exec` or `bro_resume` when you need the result. USE MAXIMUM TIMEOUT for provider work. On timeout, or when a completed result is empty/suspicious, call `bro_status(tail=N)` before deciding the task is stuck, treating it as success, cancelling it, or dispatching replacement work. Completed replies include small deliverables inline. resultTruncated/resultCursor continues through bro_status(detail=result,cursor=...). structuredExitOmitted requires bro_status(detail=structured_exit); follow body.next_cursor to reconstruct the JSON value. ",
        example: None,
    },
    ToolDoc {
        name: "bro_when_all",
        category: ToolCategory::Orchestration,
        summary: "Observe ALL selected tasks or team members until completion; never launches follow-up work. Use for concurrent waits after explicit dispatch.",
        when_to_use: "Fan-out/fan-in pattern. Pair with `bro_broadcast` for blind deliberation / provider comparison. USE MAXIMUM TIMEOUT. On timeout, inspect member status before cancelling or redispatching. Completed replies include small deliverables inline. resultTruncated/resultCursor continues through bro_status(detail=result,cursor=...). structuredExitOmitted requires bro_status(detail=structured_exit); follow body.next_cursor to reconstruct the JSON value. ",
        example: None,
    },
    ToolDoc {
        name: "bro_when_any",
        category: ToolCategory::Orchestration,
        summary: "Block until the FIRST task completes; use for races instead of polling each task yourself.",
        when_to_use: "Racing providers / fast-path resolution. First result wins, others keep running unless cancelled. Before cancelling laggards, check status and cancel only if the remaining work is truly no longer useful. Completed replies include small deliverables inline. resultTruncated/resultCursor continues through bro_status(detail=result,cursor=...). structuredExitOmitted requires bro_status(detail=structured_exit); follow body.next_cursor to reconstruct the JSON value. ",
        example: None,
    },
    ToolDoc {
        name: "bro_broadcast",
        category: ToolCategory::Orchestration,
        summary: "Send the same prompt to every team member. `cwd` (canonical; `project_dir` deprecated alias) overrides the working directory for every member dispatch.",
        when_to_use: "Ensemble work. Follow with `bro_when_all` (deliberation) or `bro_when_any` (race). Resumed members are single-flight like `bro_resume`; wait or cancel a member's current task before broadcasting another turn to that same session. Interleave with individual `bro_resume` for cross-pollination between rounds.",
        example: None,
    },
    ToolDoc {
        name: "bro_status",
        category: ToolCategory::Orchestration,
        summary: "Read task progress. lastAssistantSnippet previews the latest assistant text (up to 256 characters), when known. detail=result, report, or structured_exit returns exact body pages; replay body.next_cursor to continue. debug adds execution diagnostics.",
        when_to_use: "Exact body reads omit repeated snippets and routine context/event telemetry; debug=true includes those diagnostics. Default summary includes state, progress, blockers and result availability. Result, report and structured_exit detail returns body.text with format, offset and total_bytes; pages contain at most 4096 UTF-8 bytes. Replay body.next_cursor unchanged with the same task and detail. A changed body rejects the cursor; restart from its first page. Reassemble JSON report/structured_exit pages before parsing. tail applies only to summary, capped at 50 events and 8192 serialized bytes. debug adds accounting and worker-owned transcript coordinates; those coordinates are not caller file paths. Context occupancy is telemetry, never remaining work capacity.",
        example: None,
    },
    ToolDoc {
        name: "bro_dashboard",
        category: ToolCategory::Orchestration,
        summary: "Page recent task summaries for lookup; do not take over another operator's task. Reports expand through bro_status. Context occupancy is not remaining work capacity.",
        when_to_use: "Defaults to 20 rows, maximum 100. Follow next_offset with the same filters; order is start time descending then task ID. Live state may change between pages. Agent metrics cover only returned tasks, and reports are bounded previews with detail hints. Unknown provider, status, or team filters fail explicitly. Use bro_status for exact results or reports, and coordination wait tools when awaiting completion.",
        example: None,
    },
    ToolDoc {
        name: "bro_report",
        category: ToolCategory::Orchestration,
        summary: "Attach the latest progress report to a task.",
        when_to_use: "Dispatched agents call this at major milestones so bro_dashboard and bro_status show what the task last reported, what it needs, and when it last checked in.",
        example: Some(
            r#"bro_report(task_id="...", message="writing tests", needs="review API naming")"#,
        ),
    },
    ToolDoc {
        name: "bro_steer",
        category: ToolCategory::Orchestration,
        summary: "Queue a user steer into a running bro-harness process without cancelling the active turn.",
        when_to_use: "Use when a running bro should incorporate extra direction but does not need to stop its current turn. If the task already finished, use bro_resume instead. Only live harness child processes are steerable.",
        example: Some(r#"bro_steer(task_id="...", prompt="Prefer the smaller scoped fix.")"#),
    },
    ToolDoc {
        name: "bro_interrupt",
        category: ToolCategory::Orchestration,
        summary: "Interrupt a running bro-harness process; optionally queue redirect text to run after interruption repair.",
        when_to_use: "Use when the current turn is going the wrong way and should stop now. Pass prompt for interrupt-and-redirect; omit it for a plain interrupt. This is different from bro_cancel: the live session is repaired and can continue inside the same task.",
        example: Some(
            r#"bro_interrupt(task_id="...", prompt="Stop that path; inspect the boundary doc first.")"#,
        ),
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
        when_to_use: "Stale failed/completed/cancelled tasks are cluttering bro_dashboard or bbox_inbox. Cleanup is part of external orchestration hygiene, but prune only terminal tasks and prefer filters that match work you created. Defaults to status=failed. Pass task_ids=[…] to drop exactly the tasks you created without a status-wide sweep of the shared store (matches any terminal status unless status is also given). Filter by provider or older_than_hours; use dry_run=true to preview. Running tasks are never touched. Pass retro=true to fire a fire-and-forget workload retrospective on each pruned task before it's dropped (see bro_retro); tune with retro_min_turns / retro_max.",
        example: Some(r#"bro_prune(task_ids=["abc123"])"#),
    },
    ToolDoc {
        name: "bro_retro",
        category: ToolCategory::Orchestration,
        summary: "Ask a terminal bro for a workload retrospective: resume its session with a non-compelling reflection prompt; it self-files substrate gaps via bbox_gap only if something's worth surfacing. Does not delete the task.",
        when_to_use: "You want a finished bro to reflect on friction with the blackbox substrate itself — missing/awkward bbox_/bro_ tools, stale guidance or memories, clumsy workflow/dispatch steps — and self-file substrate gaps via bbox_gap (surfaced in bbox_inbox) only if something's worth surfacing. Scoped to surfaces blackbox can change, not the target repo or its toolchain. Does not delete the task; bro_prune(retro=true) is the bulk path at cleanup time.",
        example: Some(r#"bro_retro(task_id="…")"#),
    },
    ToolDoc {
        name: "bro_providers",
        category: ToolCategory::Orchestration,
        summary: "List provider summaries; pass provider to list its model slugs and reasoning efforts.",
        when_to_use: "Discover providers with no arguments. Pass provider=\"brodex\" (or another returned provider id) for that provider's model slugs and model-specific effort support. This is a static catalog, not an observation of execution-worker availability.",
        example: None,
    },
    ToolDoc {
        name: "bro_brofile",
        category: ToolCategory::Orchestration,
        summary: "Manage brofiles and accounts. list returns paginated summaries; get by name returns the full lens and configuration.",
        when_to_use: "Create, inspect, and manage reusable bro blueprints. `action=list` returns sorted summaries (default limit=20, max=100) with total and next_offset; provider and name filter before pagination. Lenses and dispatch policy are omitted: use `action=get` with name for full details. `context.provider_defaults` controls provider-default suppression; see `sm-brofile-context` via `bbox_knowledge` before composing minimal probes or strict suppression brofiles. `set_account` and `list_accounts` return environment key names and account policy, never credential values. Before `action=create`, call `action=list` first to avoid duplicates. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene.",
        example: Some(r#"bro_brofile(action="list")"#),
    },
    ToolDoc {
        name: "bro_team",
        category: ToolCategory::Orchestration,
        summary: "Manage teamplates and teams without automatic advisor execution. list/list_templates/roster return bounded summaries; get/get_template return exact JSON body pages.",
        when_to_use: "Save templates, instantiate teams, inspect roster, or tear teams down. New advisor arguments are rejected. Legacy advisor settings/history remain readable and are marked execution=retired in summaries; create and waits never execute them. Dispatch any reviewer explicitly with bro_exec/bro_resume. list/list_templates/roster page at default 20, maximum 100; next_offset continues with the same exact name/project filters. get/get_template return exact stored JSON: concatenate body.text pages using cursor=body.next_cursor, then parse; source changes invalidate cursors. Template scope accepts global or project. In catalog mode, project template operations refuse because legacy .bro files have no owner transport; use global templates or owner-side operations. Team creation uses global template/brofile configuration and retains project_dir as worker context. Expanded membership must be 1..256; counts outside this range refuse before allocation or persistence. Team members resume existing sessions on later broadcasts; dissolve/recreate when validating new brofile context or provider-default suppression. See `sm-brofile-context` via `bbox_knowledge`. Before `save_template` or `create`, list existing objects first to avoid duplicates. Dissolve ad hoc teams you created after their work is terminal; do not dissolve another operator's team unless instructed. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene.",
        example: Some(
            r#"bro_team(action="create", template="red-team", name="bbox-red", project_dir="/repo/x")"#,
        ),
    },
    ToolDoc {
        name: "bro_mcp",
        category: ToolCategory::Orchestration,
        summary: "Manage MCP servers + tool filters for dispatched bros.",
        when_to_use: "Configuration list/get replies are redacted: endpoint origins and credential key names are visible, inline values and stdio arguments are withheld. Explicitly add/remove MCP servers and manage dispatch-time tool filters. The daemon does not rewrite provider MCP configs on startup. Before `action=add`, call `action=list` first. The default bro-tool disallow is mechanical recursion protection, not just prose guidance. See `sm-create-etiquette` via `bbox_knowledge` for dedupe hygiene.",
        example: Some(
            r#"bro_mcp(action="disallow", pattern="mcp__blackbox__bro_*", scope="global")"#,
        ),
    },
    // ── Workflows ────────────────────────────────────────────────────

    // ── Agents ──────────────────────────────────────────────────
    ToolDoc {
        name: "bro_agent_list",
        category: ToolCategory::Orchestration,
        summary: "List installed agents in name/version order as compact summary pages (default 20, maximum 100). Continue with next_offset. Existing registry filters apply before paging. detail=true expands descriptions and installation diagnostics; bro_agent_get/bro_agent_describe reads one exact agent.",
        when_to_use: "Discover what agents are available for dispatch, composition, or review. Filter by cost_class to find cheap/expensive agents; use include_superseded=true to see version history, including retired adapter-backed manifests marked inactive. Retired manifests remain readable by exact name/ref but never appear as callable agents.",
        example: Some(r#"bro_agent_list(include_superseded=true)"#),
    },
    ToolDoc {
        name: "bro_agent_get",
        category: ToolCategory::Orchestration,
        summary: "Read full details for a single agent by name or agent-ref (name@vN or agent:name@vN). Returns manifest, metadata, and lifecycle state.",
        when_to_use: "Inspect a specific agent's manifest (brofile config, filter overlay, inputs/outputs, composition constraints) before dispatching or composing it into a pipeline.",
        example: Some(r#"bro_agent_get(name="reviewer")"#),
    },
    ToolDoc {
        name: "bro_agent_describe",
        category: ToolCategory::Orchestration,
        summary: "Compact per-plane dispatch surface for one agent.",
        when_to_use: "Pre-dispatch inspection: see the stored manifest, resolved brofile, filter overlay, the computed deny-wins merge, and the runtime planes describe does not compute (project filters, surface packet, per-dispatch overrides, recursion guard). detail_plane=manifest|brofile pages the exact redacted JSON with body.next_cursor; a missing brofile returns an actionable readiness error. Use before bro_agent_dispatch to preview the dispatch plan.",
        example: Some(r#"bro_agent_describe(agent="code-reviewer")"#),
    },
    ToolDoc {
        name: "bro_agent_search",
        category: ToolCategory::Orchestration,
        summary: "Search installed agents by query string. Matches against description and when_to_use; penalizes or excludes results matching anti_patterns. Returns ranked results with scores, provenance, and matched anti-patterns.",
        when_to_use: "Discovery: find agents relevant to a task before dispatching. Call with the task description to get ranked candidates. Set exclude_anti_pattern_matches=false to see all matches including anti-pattern hits (useful for review). Filter by cost_class or provenance_kind to narrow results.",
        example: Some(
            r#"bro_agent_search(query="review pull request for security issues", limit=3)"#,
        ),
    },
    ToolDoc {
        name: "bro_agent_dispatch",
        category: ToolCategory::Orchestration,
        summary: "Dispatch a registered agent for a focused task. Resolves the brofile, merges filters, validates inputs and starts one bro turn. Custom dispatch adapters are retired and refused. Returns task_id, session, and agent attribution (agentLabel on the spawned task, preserved even when bro= routes to a named team member).",
        when_to_use: "Dispatching an agent after discovery via bro_agent_search. Returns (task_id, session) — resume with bro_resume, status with bro_status. Prefer over hand-rolling a brofile + bro_exec when the task matches an agent's description and when_to_use. Set the session's working directory with `cwd` (canonical; `project_dir` accepted as a deprecated alias). Pass runtime={...} to overlay tier/pool/pin allocation on the standard bro dispatch path. Recursion is manifest-declared: an agent whose manifest sets allow_recursion=true dispatches with the recursive bro_* tools available; there is no per-call override (ad-hoc recursive dispatch is bro_exec's allow_recursion). Anti-pattern: do not dispatch when the agent's manifest declares one of your task's properties as an anti_pattern.",
        example: Some(
            r#"bro_agent_dispatch(agent="code-reviewer", cwd="/repo/x", args={"diff": "..."})"#,
        ),
    },
    // ── Atoms ───────────────────────────────────────────────────

    // ── Whiteboards ─────────────────────────────────────────────

    // ── Whiteboards ─────────────────────────────────────────────
    ToolDoc {
        name: "bbox_tool_calls",
        category: ToolCategory::Workspace,
        summary: "Search indexed tool-call history by server, tool name, kind, target, project and time. Returns bounded rows and next_offset; paths in records describe historical calls, not files the caller must open.",
        when_to_use: "Use for tool-use evidence from indexed transcripts. Exact server/tool/kind filters narrow the index query. Glob (* wildcard), target substring, project and since filters apply to a bounded candidate page. Rows preview long fields with explicit truncation markers. When context is present, pass it as the arguments to bbox_context for the indexed source event (an indexed projection, not a guaranteed complete transcript). outcome=requested records invocation, not successful completion. Default limit 20, maximum 100. Follow next_offset even when rows is empty; use identical filters, and restart after index changes. Offsets stop at 100000; narrow filters beyond that window. since requires RFC 3339 with timezone. No automatic reindex or local file read occurs.",
        example: Some(r#"bbox_tool_calls(server="blackbox", tool_name="bro_exec", limit=20)"#),
    },
    // ── Roadmap ─────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_roadmap",
        category: ToolCategory::Roadmap,
        summary: "Manage the bbox roadmap — an operator-directed prospective work tracker for designed-but-not-implemented features, refactors, explorations, tech debt, and risks. Roadmap interactions are performed only at the express direction of the operator; never use the roadmap to defer, postpone, or avoid requested implementation work. Inbox is reactive; threads are active work; knowledge is atemporal. Status lifecycle: proposed → accepted → delivered (shipped) or rejected; accepted → deferred → accepted.",
        when_to_use: "Use only when the operator explicitly asks to manage future work, review what's designed but not yet built, decide what to work on next, or promote a specific roadmap item. Do not initiate roadmap actions as an agent-selected deferral path; if the operator requested implementation, do the implementation unless they explicitly redirect it to roadmap tracking. `action=\"next\"` ranks accepted items by priority, staleness, blockers, and design-link health. `action=\"promote\"` opens a bbox_thread with the item's context injected. Link to design docs (designed_in) and threads (spawns / deferred_from). `action=\"render\"` emits a Tera-templated markdown artifact; pass `template` (inline Tera source) to customise layout and which statuses are included — `delivered`/`rejected` are excluded from the default template. `action=\"default_template\"` returns the built-in Tera source as a starting point. Render returns markdown for caller-owned application; write_path/template_path are rejected and server-local configuration cannot choose implicit file destinations.",
        example: Some(r#"bbox_roadmap(action="next", n=5)"#),
    },
    // ── Operations ──────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_doctor",
        category: ToolCategory::Operations,
        summary: "Diagnose Blackbox health with ranked, paginated findings. format selects summary text or JSON; detail=full adds server-owned diagnostics. Narrow with section.",
        when_to_use: "Use as the first call when asking \"what do I need to know about Blackbox right now?\" — replaces the scattered manual smoke checklist (bbox_stats, bbox_embed_status, bbox_project_list, bbox_lint, bbox_inbox) with one ranked surface. Route findings distinguish real failures (action) from opt-in absence like unconfigured visual chunk kinds (info). Default detail=summary returns up to 20 findings (max 100) ordered worst severity, section, then message; next_offset continues. Section status is separate from the findings page. detail=full includes diagnostics and can be narrowed by an exact section name. format=json does not imply full detail. Health is live, so restart pagination after a state change.",
        example: Some(r#"bbox_doctor(format="summary")"#),
    },
    // ── Storage health ──────────────────────────────────────────────
    ToolDoc {
        name: "bbox_storage_health",
        category: ToolCategory::StorageHealth,
        summary: "Read daemon-owned edge storage totals and the ten largest contributors. include_files=true returns file pages (limit default 20, max 100); follow next_offset. File paths are relative to daemon storage, not caller-readable paths. Use bbox_storage_gc for managed cleanup. Manifest and retention warnings remain visible.",
        when_to_use: "Paths in file pages are relative diagnostic coordinates in daemon-owned storage, not caller filesystem paths. Manifest and retention warnings remain visible. Use when diagnosing storage growth or validating retention before GC. Observed history is reported separately and retained by explicit keep/no-cap policy unless an operator supplies a cap to GC.",
        example: None,
    },
    ToolDoc {
        name: "bbox_storage_gc",
        category: ToolCategory::StorageHealth,
        summary: "Preview (default) or apply storage GC. Returns bounded counts, estimated bytes, protection counts, stage outcomes, and a receipt_id; exact detail is opt-in and paged.",
        when_to_use: "Use after bbox_storage_health. dry_run=true never deletes; dry_run=false runs a fresh native plan, not a saved preview. Responses use status, apply_requested and counts, not an applied boolean. status=applied means every requested stage completed without reported errors; partial means apply had errors, not that nothing changed. GC is non-atomic: earlier deletions remain and failed tree removals may have partial effects. incomplete is a preview with stage errors. deleted_bytes_estimate uses plan-time sizes of fully removed edge candidates, excludes packets, and is not exact disk-space recovery. unconfirmed_count covers eligible candidates not confirmed removed (including native skips and errors). detail=candidates/deleted/errors/exclusions/packets/full returns exact JSON body pages, default/max 4096 bytes, minimum 4; concatenate body.text fragments to recover the selected JSON. The first operation response includes totals even with detail. Later detail reads contain only outcome, receipt_id, expires_in_seconds, detail and body; read receipt_id with default detail=summary to recover totals. Continue using receipt_id, the SAME detail, and body.next_cursor only. Cursor without receipt_id, non-default GC options on a receipt read, and unknown options are rejected before work. Receipt reads never plan or delete. Receipts are daemon-local immutable reports, NOT executable plans or durable records: retained up to 15 minutes and 16 receipts, subject to earlier eviction/restart. Download needed details promptly; an unavailable receipt never triggers GC. The cache targets 64 MiB of serialized detail, retaining an oversized newest receipt alone rather than discarding post-apply evidence. External sweepers must read detail=exclusions in full before acting; summary counts do not authorize a sweep. Rollback-marker refusal and protected roots remain enforced. Native defaults retain newest backups and observed history; inactive snapshot age/recent/grace preferences do not override per-workspace count (32) and byte (8 GiB) ceilings. Active snapshots stay protected. Dangling/legacy orphans auto-prune only after grace; explicitly unregistered storage requires prune_explicitly_unregistered=true. prune_duplicate_packets=true dedupes across ALL packet scopes/projects (the edge project filter does not narrow it), keeps newest identical content per domain/scope/project, protects Apply references, and reports packet/lock failures without losing earlier deletion counts.",
        example: Some(
            r#"bbox_storage_gc(detail="candidates"); bbox_storage_gc(receipt_id="<returned id>", detail="candidates", cursor="<body.next_cursor>")"#,
        ),
    },
    ToolDoc {
        name: "bbox_storage_migrate_legacy_edges",
        category: ToolCategory::StorageHealth,
        summary: "Dry-run or apply legacy edge sidecar migration into lifecycle-owned explicit/observed lanes. Drops derived only when managed replacement exists; quarantines malformed lines.",
        when_to_use: "Migrating pre-Phase-2 legacy sidecars into lane-split storage.",
        example: None,
    },
    // ── System Events ────────────────────────────────────────────────

    // ── Reactions ──────────────────────────────────────────────────

    // ── Identity ─────────────────────────────────────────────────────
];

pub const WORKFLOW_NOTES: &str = "\
## Retrieval cues

If a tool stanza says `See: sm-...`, fetch that runbook on demand with \
`bbox_knowledge(query=\"sm-...\")`. Keep primitive semantics hot; pull deep \
workflow guidance only when you need it.

## Query semantics

- `bbox_search` defaults to `mode=smart`: adjacent terms broaden recall, quoted \
phrases stay exact, and `-term` excludes. Use `mode=fulltext` when you want raw \
Tantivy/Lucene boolean syntax and conjunction semantics.
- `bbox_knowledge` uses the same natural query language by default. Use \
`mode=substring` only when you want literal whole-query matching instead of \
broader recall.

## Roles and the core loop

- **Orchestrator** — dispatches, reviews, reads `bbox_inbox`, resolves notes, \
and records durable commitments.
- **Executor** — when running as a dispatched bro/task actor, \
does the work and returns its result. `bbox_note` is available for *notable* \
observations worth surfacing to the orchestrator (`dispute`, `surprise`, \
`blocked`, `learned`, actionable `followup`) — it is not a per-dispatch ritual, \
so stay silent when nothing is notable. A caller that needs a structured \
sign-off contract may require a \
`kind=done` note on top of this default; absent that instruction, a done note \
is optional.

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
resume, cancel, prune, or dissolve a bro/team/task created by another external \
session unless the user explicitly asks. Prefer handles returned by your own \
dispatch.
- Before declaring a bro dead or cancelling after a timeout, call \
`bro_status(task_id=..., tail=N)`. A timeout can mean thinking, tests running, \
rate limiting, or failure; status/tail is the evidence.
- Use `bro_when_all` for fan-out/fan-in and `bro_when_any` for races. Do not \
hand-roll sequential wait/poll loops when the coordination primitive exists.
- After external orchestration, clean up only what you created — but only after \
explicit operator confirmation: terminal status is not the same as done, so ask \
before pruning terminal tasks with `bro_prune` (offer `retro=true`) or dissolving \
ad hoc teams. Cleanup is operator-gated, not automatic.
- Memory lanes: `bbox_thread` (investigation state), \
`bbox_learn`/`bbox_decide` (operator-approved standing rules / commitments), \
`bbox_remember` (cold grep-able facts), `bbox_pin` (arc-bound hot context). \
The one-year test picks between rendered and pin — would it still be correct \
a year from now with current arcs done?
- Compose gates, retries, schedules and review protocols in your caller. Blackbox \
executes and resumes bro turns; it does not choose the next application step.
- `bbox_learn` is for operator-approved, user-stated rules; `bbox_note(kind=learned)` is for \
agent-discovered facts.
- For long-running bro work, instruct dispatched agents \
to call `bro_report` at major milestones. `bro_dashboard` should show the last \
thing each bro reported, what it needs, and how long ago it checked in.
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
/// `bro_report` is intentionally excluded: it is telemetry, not
/// recursive dispatch, and dispatched agents should be able to report
/// their own progress.
pub fn recursion_guard_tool_names_prefixed() -> Vec<String> {
    let prefix = blackbox_mcp_prefix();
    TOOL_DOCS
        .iter()
        .filter(|d| d.name.starts_with("bro_") && d.name != "bro_report")
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

/// Render the hot-path tool reference as markdown. Deep categories are
/// rendered as on-demand system-memory pointers, not as full per-tool manuals.
pub fn render_markdown() -> String {
    let mut out = String::new();
    out.push_str(
        "Blackbox tool reference — the MCP tools this daemon exposes and when to reach for them. ",
    );
    out.push_str(
        "This entry is generated from `src/tool_docs.rs` and refreshed on every daemon restart. ",
    );
    out.push_str("Do not hand-edit.\n\n");

    out.push_str("## CORE RULE: agentic opening sequence\n\n");
    out.push_str("**For any task that touches the codebase, prior decisions, or conversational history, run this five-step sequence before falling back to filesystem search or training-prior answers:**\n\n");
    out.push_str("```\n");
    out.push_str("1. bbox_describe_schema           # orient — entity types + edge families\n");
    out.push_str(
        "2. bbox_hybrid_search(q, k=5)     # seeds — mixed-modal results with notable_edges\n",
    );
    out.push_str("3. bbox_inspect_entity(ref)       # confirm — properties + edges in one call\n");
    out.push_str("4. bbox_find_paths(from, to_*)    # traverse — direction-preserving BFS chains (when multi-hop)\n");
    out.push_str("5. bbox_bundle_evidence(...)      # answer — package refs + path_ids\n");
    out.push_str("```\n\n");
    out.push_str("Step 1 is one-time per session — cache the schema mentally. Step 4 is conditional (skip when the question is single-hop). Step 5 is the close-the-loop write that lets the user re-query your evidence.\n\n");
    out.push_str("`bbox_blame(file, line)` is the line-level provenance escape hatch when the question is \"who/why does this line exist?\" rather than a graph walk.\n\n");
    out.push_str("**Hard rules (break these and quality collapses):**\n\n");
    out.push_str("1. Entity refs are canonical `<type>:<segments>` — when a tool returns `error.bad_input` with a `suggested_fix`, use the suggestion verbatim, don't guess.\n");
    out.push_str("2. Don't restate paths from memory — pass `path_ids` from `bbox_find_paths` directly to `bbox_bundle_evidence` (the server holds the validated graph).\n");
    out.push_str("3. Targeted inspection beats broad inspection — pass `edge_types` and `direction` once you know what you're looking for; default `direction=both` is for orientation only.\n");
    out.push_str("4. Follow `recommended_next_hops` from `bbox_inspect_entity` — they're ordered semantic-first, structural-last.\n");
    out.push_str("5. Trust topical hits — `bbox_hybrid_search` blends BM25 + vector + path-token boost. Top seed is the canonical entity even when wording doesn't exactly match.\n\n");
    out.push_str("Final-answer protocol by question type and pattern recipes (where/what/who/why/how/replacement/historical/impact) live in `sm-agentic-opening-sequence`. Pull it via `bbox_knowledge(query=\"sm-agentic-opening-sequence\")` the first time you handle one of those question shapes.\n\n");

    out.push_str("## CORE RULE: contextual recall fallback\n\n");
    out.push_str("**When the opening sequence above doesn't fit (fast lookup of stored rules, no graph walk needed), query `bbox_knowledge` directly before committing to an approach.** This is a recall check, not a ritual call for every tiny command.\n\n");
    out.push_str("Use it for prior decisions, project conventions, rendered rules, remembered facts, system runbooks (sm-* IDs), and packet discovery. It is not the surface for scoped pins (`bbox_pin`), side-channel notes (`bbox_notes` / `bbox_inbox`), active threads (`bbox_thread_list`), or transcript history (`bbox_search`).\n\n");
    out.push_str("Do not add a `bbox_knowledge` call just because you are doing procedural state work on an already-authoritative live surface: deduping or resolving gaps with `bbox_gaps`/`bbox_gap*`, opening or continuing threads with `bbox_thread*`, triaging notes/inbox, or committing repo-owned state files. Use the live surface directly unless a specific durable convention, decision, or runbook could materially change the operation.\n\n");
    out.push_str("The signature failure mode: agents confidently produce training-prior answers to questions whose actual answer is stored in bbox. Avoid that on work involving repo conventions, prior decisions, active runbooks, durable user preferences, bro/orchestration behavior, or anything where durable project memory could plausibly override defaults.\n\n");
    out.push_str("Prefer a short phrase from the user's request over a single generic keyword. If the first query is empty or too broad, try one sharper phrase or escalate to `bbox_hybrid_search` (vector lane catches paraphrases). Then proceed with the opening sequence above or normal implementation work using the retrieved context.\n\n");
    out.push_str(
        "Cost of a wasted query: near zero. Cost of a confident wrong answer: the entire task.\n\n",
    );

    out.push_str("## CORE RULE: operator-approved persistence\n\n");
    out.push_str("**When the user states a rule, convention, or preference that may need to bind future sessions, do not immediately call `bbox_learn`, `bbox_remember`, or `bbox_decide`.** First decide whether persistence is warranted, then present the proposed memory text, lane, and scope to the operator and wait for explicit approval. Mechanical enforcement in code/config can enforce the current edit but does not transmit intent to future sessions; persistence still requires approval unless the operator has already approved the exact memory write in the current turn.\n\n");
    out.push_str("Triggers (positive and negative bind equally): \"from now on\", \"always X\", \"never X\", \"we (don't) use Y\", \"prefer Y\", \"X is banned / retired / out of scope\", \"stop using X\", \"no more X\", \"house rule\", \"standing order\", \"keep X out of\", \"X must not\".\n\n");
    out.push_str("Lane selection - when preparing a persistence proposal, walk the ladder and stop at the first yes:\n\n");
    out.push_str("1. Is this investigation state tied to one debug/QC walk? → `bbox_thread`\n");
    out.push_str("2. Would the statement still be correct a year from now with all current arcs complete? → propose `bbox_learn` or `bbox_decide`\n");
    out.push_str("3. Is it a cold searchable fact worth grepping for later but not worth every session loading? → propose `bbox_remember`\n");
    out.push_str("4. Otherwise - arc-bound guidance that must stay hot for one execution lane - → `bbox_pin`\n\n");
    out.push_str("The one-year test at step 2 is the load-bearing filter. Content naming a specific migration, phase, active arc, current initiative, or \"finish X before Y\" sequencing fails it and belongs in `bbox_pin`, not `bbox_learn`. Ephemeral task constraints (\"for this fix, skip tests\", \"just for today\") don't get persisted at all.\n\n");
    out.push_str("After implementing any user directive in code/config, explicitly ask yourself: did the user just state a standing rule? If yes, propose the exact storage text, lane, and scope before replying; only emit the storage call after the operator approves it.\n\n");

    out.push_str("**Scope selection.** Default to `project` for repo-local conventions. Choose `global` only when the user's phrasing explicitly reaches beyond this repo — \"across every project\", \"on every machine\", \"in every X I write\", \"I always X as a personal rule\", \"house rule on this machine\". Technology-scoped but project-agnostic statements (\"in all Rust code I write\", \"always prefer fd over find\") are `global`. Strong wording alone is not enough — \"we always use tokio here\" stays `project`. Presence of a current project does not imply `project` scope when the user states a cross-project personal rule. If both readings are plausible, choose `project`.\n\n");

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
            for doc in TOOL_DOCS.iter().filter(|d| {
                d.category == *cat && !matches!(d.name, "bbox_absorb" | "bbox_bootstrap")
            }) {
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

// ── Sync into knowledge store ────────────────────────────────────────

pub struct SyncResult {
    /// true = upsert wrote to disk; false = content unchanged
    pub wrote: bool,
    pub bytes: usize,
}

/// Upsert the canonical tool reference as a fixed-ID global entry.
/// Idempotent: no-op if the content hasn't changed.
pub fn sync_into_knowledge(kb: &mut bbox_knowledge::knowledge::Knowledge) -> Result<SyncResult> {
    let content = render_markdown();
    let bytes = content.len();

    // Look for existing entry by stable ID
    let existing = kb
        .all_entries()
        .iter()
        .find(|e| e.id == TOOL_DOC_ENTRY_ID)
        .cloned();

    if let Some(ref e) = existing {
        if e.content == content {
            return Ok(SyncResult {
                wrote: false,
                bytes,
            });
        }
    }

    let now = bbox_util::util::now_iso();
    let entry = KnowledgeEntry {
        id: TOOL_DOC_ENTRY_ID.to_string(),
        title: "Blackbox tool reference".to_string(),
        content,
        cluster: None,
        variants: Default::default(),
        category: Category::Tool,
        scope: Scope::Global,
        project: None,
        project_id: None,
        providers: Vec::new(),
        priority: Priority::Standard,
        weight: 100,
        render: true,
        decay: false, // generated; managed by code
        review_at: None,
        status: Status::Active,
        approval: Approval::UserConfirmed,
        supersedes: None,
        links: Vec::new(),
        rationale: None,
        expires_at: None,
        source: "tool_docs".to_string(),
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
    fn render_contains_hot_tool_names() {
        let md = render_markdown();
        for doc in TOOL_DOCS {
            if !HOT_RENDER_CATEGORIES.contains(&doc.category)
                || matches!(doc.name, "bbox_absorb" | "bbox_bootstrap")
            {
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
        for (cat, memory_id) in [
            (ToolCategory::Packets, "sm-rule-packets"),
            (ToolCategory::Orchestration, "sm-bro-dispatch-patterns"),
        ] {
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
    fn render_omits_workspace_tools_from_hot_layer() {
        let md = render_markdown();
        assert!(!md.contains("## Workspace tools"));
        for doc in TOOL_DOCS
            .iter()
            .filter(|d| d.category == ToolCategory::Workspace)
        {
            assert!(
                !md.contains(doc.name),
                "workspace tool leaked into rendered hot layer: {}",
                doc.name
            );
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
    fn roadmap_guidance_requires_operator_direction_and_forbids_deferral() {
        let md = render_markdown();
        let doc = TOOL_DOCS
            .iter()
            .find(|d| d.name == "bbox_roadmap")
            .expect("bbox_roadmap doc exists");

        assert!(md.contains("express direction of the operator"));
        assert!(md.contains("never use the roadmap to defer"));
        assert!(doc.summary.contains("express direction of the operator"));
        assert!(doc.summary.contains("never use the roadmap to defer"));
        assert!(
            doc.when_to_use
                .contains("Use only when the operator explicitly asks")
        );
        assert!(doc.when_to_use.contains("Do not initiate roadmap actions"));
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
        assert!(md.contains("sm-rule-packets"));
        assert!(md.contains("bbox_knowledge(query=\"sm-rule-packets\")"));
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
                    || n.starts_with("whiteboard_")
                    || n.starts_with("work_")
                    || n.starts_with("atom_")
                    || n.starts_with("system_event_")
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

    #[test]
    fn description_summary_parity() {
        // Fourth-surface invariant: the per-call chooser blurb in
        // `#[tool(description = ...)]` (src/**/*.rs) must equal the
        // managed-layer `ToolDoc.summary` (this file). They're the same
        // text to the agent — let them drift and the agent gets
        // contradictory guidance at the two surfaces. See the
        // `bb846aad` decision entry for the four-surface policy.
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
